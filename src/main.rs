use clap::{Parser, command};
use iroh::net::key::SecretKey;
use iroh::{blobs::store::mem::Store, node::Node, router::ProtocolHandler};
use iroh_loro::IrohLoroProtocol;
use notify::Watcher;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(version, about, long_about = None)]
enum Cli {
    Serve {
        file_path: String,
    },
    Join {
        remote_id: iroh::net::NodeId,
        file_path: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Cli::parse();

    // Common setup function for both Serve and Join modes
    async fn setup_node(
        doc: loro::LoroDoc,
        file_path: String,
        secret_key: Option<SecretKey>,
    ) -> anyhow::Result<(
        Arc<IrohLoroProtocol>,
        Node<Store>,
        tokio::task::JoinHandle<()>,
    )> {
        let (tx, mut rx) = mpsc::channel(100);
        let p = IrohLoroProtocol::new(doc, tx);

        // Spawn file writer task
        let writer_handle = tokio::spawn(async move {
            while let Some(contents) = rx.recv().await {
                println!("💾 Writing new contents to file. Length={}", contents.len());
                match std::fs::write(&file_path, contents) {
                    Ok(_) => println!("✅ Successfully wrote to file"),
                    Err(e) => println!("❌ Failed to write to file: {}", e),
                }
            }
        });

        let mut node = Node::memory();
        if let Some(s) = secret_key {
            node = node.secret_key(s);
        }

        // Create and configure iroh node
        let iroh = node
            .build()
            .await?
            .accept(
                IrohLoroProtocol::ALPN.to_vec(),
                Arc::clone(&p) as Arc<dyn ProtocolHandler>,
            )
            .spawn()
            .await?;

        let addr = iroh.net().node_addr().await?;
        println!("Running\nNode Id: {}", addr.node_id);

        Ok((p, iroh, writer_handle))
    }

    // Modified file watcher setup to return the JoinHandle
    fn spawn_file_watcher(
        file_path: String,
        p: Arc<IrohLoroProtocol>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            println!("👀 Starting file watcher for: {}", file_path);
            let (notify_tx, mut notify_rx) = mpsc::channel(1);
            let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
                if let Ok(event) = res {
                    if let notify::EventKind::Modify(_) = event.kind {
                        println!("📝 File modification detected");
                        let _ = notify_tx.blocking_send(());
                    }
                }
            })
            .unwrap();
            watcher
                .watch(
                    std::path::Path::new(&file_path),
                    notify::RecursiveMode::NonRecursive,
                )
                .unwrap();

            loop {
                if let Some(_) = notify_rx.recv().await {
                    match std::fs::read_to_string(&file_path) {
                        Ok(contents) => {
                            println!("📖 Read file contents (length={})", contents.len());
                            p.update_doc(&contents).await;
                        }
                        Err(e) => println!("❌ Failed to read file: {}", e),
                    }
                }
            }
        })
    }

    match opts {
        Cli::Serve { file_path } => {
            // Initialize document with file contents
            let contents = std::fs::read_to_string(&file_path)
                .expect("Should have been able to read the file");
            let doc = loro::LoroDoc::new();
            doc.get_text("text")
                .update(&contents, loro::UpdateOptions::default())
                .unwrap();
            doc.commit();
            println!("Serving file: {}", file_path);

            let (p, _iroh, _writer) = setup_node(
                doc,
                file_path.clone(),
                Some(SecretKey::from_bytes(&[114; 32])),
            )
            .await?;
            let _watcher = spawn_file_watcher(file_path, p);

            // Wait for Ctrl+C
            signal::ctrl_c().await?;
            println!("Received Ctrl+C, shutting down...");
        }

        Cli::Join {
            remote_id,
            file_path,
        } => {
            let doc = loro::LoroDoc::new();
            let (p, iroh, _writer) = setup_node(doc, file_path.clone(), None).await?;
            let _watcher = spawn_file_watcher(file_path, p.clone());

            // Connect to remote node and sync
            let node_addr = iroh::net::NodeAddr::new(remote_id);
            let conn = iroh
                .endpoint()
                .connect(node_addr, IrohLoroProtocol::ALPN)
                .await?;

            p.initiate_sync(conn).await?;

            // Wait for Ctrl+C
            signal::ctrl_c().await?;
            println!("Received Ctrl+C, shutting down...");
        }
    }

    Ok(())
}