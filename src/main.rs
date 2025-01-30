use anyhow::Context;
use clap::{Parser, command};
use iroh_loro::IrohLoroProtocol;
use notify::Watcher;
use tokio::signal;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(version, about, long_about = None)]
enum Cli {
    Serve {
        file_path: String,
    },
    Join {
        remote_id: iroh::NodeId,
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
        key_path: Option<&str>,
    ) -> anyhow::Result<(
        IrohLoroProtocol,
        iroh::protocol::Router,
        tokio::task::JoinHandle<()>,
    )> {
        let secret_key = if let Some(key_path) = key_path {
            iroh_node_util::fs::load_secret_key(
                dirs_next::cache_dir()
                    .context("no dir for secret key")?
                    .join("iroh-loro")
                    .join(key_path),
            )
            .await?
        } else {
            iroh::SecretKey::generate(rand::rngs::OsRng)
        };

        let (tx, mut rx) = mpsc::channel(100);
        let protocol = IrohLoroProtocol::new(doc, tx);

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

        let endpoint = iroh::Endpoint::builder()
            .discovery_n0()
            .secret_key(secret_key)
            .bind()
            .await?;

        // Create and configure iroh node
        let iroh = iroh::protocol::Router::builder(endpoint)
            .accept(IrohLoroProtocol::ALPN, protocol.clone())
            .spawn()
            .await?;

        let addr = iroh.endpoint().node_addr().await?;
        println!("Running\nNode Id: {}", addr.node_id);

        Ok((protocol, iroh, writer_handle))
    }

    // Modified file watcher setup to return the JoinHandle
    fn spawn_file_watcher(
        file_path: String,
        protocol: IrohLoroProtocol,
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
                            protocol.update_doc(&contents);
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

            let (p, iroh, _writer) =
                setup_node(doc, file_path.clone(), Some("key.ed25519")).await?;
            let _watcher = spawn_file_watcher(file_path, p);

            // Wait for Ctrl+C
            signal::ctrl_c().await?;
            println!("Received Ctrl+C, shutting down...");
            iroh.shutdown().await?;
        }

        Cli::Join {
            remote_id,
            file_path,
        } => {
            let doc = loro::LoroDoc::new();
            if !std::path::Path::new(&file_path).exists() {
                std::fs::write(&file_path, "").unwrap();
                println!("Created new file at: {}", file_path);
            }
            let (protocol, iroh, _writer) = setup_node(doc, file_path.clone(), None).await?;
            let _watcher = spawn_file_watcher(file_path, protocol.clone());

            // Connect to remote node and sync
            let node_addr = iroh::NodeAddr::new(remote_id);
            let conn = iroh
                .endpoint()
                .connect(node_addr, IrohLoroProtocol::ALPN)
                .await?;

            protocol.inner.initiate_sync(conn).await?;

            // Wait for Ctrl+C
            signal::ctrl_c().await?;
            println!("Received Ctrl+C, shutting down...");
            iroh.shutdown().await?;
        }
    }

    Ok(())
}
