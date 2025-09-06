use anyhow::{Context, Result};
use clap::{Parser, command};
use iroh_loro::IrohLoroProtocol;
use notify::Watcher as NotifyWatcher;
use rand_core::TryRngCore;
use tokio::signal;
use tokio::sync::broadcast;
use tokio::task::JoinSet;

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
async fn main() -> Result<()> {
    let opts = Cli::parse();

    let mut tasks = JoinSet::new();

    let iroh = match opts {
        Cli::Serve { file_path } => {
            // Initialize document with file contents
            let contents = tokio::fs::read_to_string(&file_path).await?;

            let doc = loro::LoroDoc::new();
            let text = doc.get_text("text");
            text.insert(0, &contents)?;
            doc.commit();

            println!("Serving file: {}", file_path);

            let protocol = setup_protocol(doc, file_path.clone(), &mut tasks).await?;
            let iroh = setup_node(protocol.clone(), Some("key.ed25519")).await?;

            tasks.spawn(async move {
                if let Err(e) = watch_files(file_path, protocol).await {
                    println!("❌ File watcher task failed: {e}");
                }
            });

            iroh
        }

        Cli::Join {
            remote_id,
            file_path,
        } => {
            let doc = loro::LoroDoc::new();
            if !std::path::Path::new(&file_path).exists() {
                tokio::fs::write(&file_path, "").await?;
                println!("Created new file at: {file_path}");
            }

            let protocol = setup_protocol(doc, file_path.clone(), &mut tasks).await?;
            let iroh = setup_node(protocol.clone(), None).await?;

            tasks.spawn({
                let protocol = protocol.clone();
                async move {
                    if let Err(e) = watch_files(file_path, protocol).await {
                        println!("❌ File watcher task failed: {e}");
                    }
                }
            });

            // Connect to remote node and sync
            let conn = iroh
                .endpoint()
                .connect(remote_id, IrohLoroProtocol::ALPN)
                .await?;

            tasks.spawn(async move {
                if let Err(e) = protocol.initiate_sync(conn).await {
                    println!("Sync protocol failed: {e}");
                }
            });

            iroh
        }
    };

    // Wait for Ctrl+C
    signal::ctrl_c().await?;

    n0_future::future::race(
        async move {
            println!("Received Ctrl+C, shutting down...");
            iroh.shutdown().await?;
            tasks.shutdown().await;
            println!("shut down gracefully");
            Ok(())
        },
        async {
            signal::ctrl_c().await?;
            println!("Another Ctrl+C detected, forcefully shutting down...");
            std::process::exit(1);
        },
    )
    .await
}

// Common protocol setup function for both Serve and Join modes
async fn setup_protocol(
    doc: loro::LoroDoc,
    file_path: String,
    tasks: &mut JoinSet<()>,
) -> Result<IrohLoroProtocol> {
    let (change_broadcaster, mut change_receiver) = broadcast::channel::<Vec<u8>>(1000);
    let protocol = IrohLoroProtocol::new(doc, change_broadcaster);

    // Spawn file writer task that handles document changes
    let protocol_for_file_writer = protocol.clone();
    tasks.spawn(async move {
        while let Ok(_changes) = change_receiver.recv().await {
            // Get the current text content from the document
            let doc = protocol_for_file_writer.doc();
            let text = doc.get_text("text");
            let content = text.to_string();
            
            println!("💾 Writing document content to file. Length={}", content.len());
            match tokio::fs::write(&file_path, &content).await {
                Ok(_) => println!("✅ Successfully wrote to file"),
                Err(e) => println!("❌ Failed to write to file: {e}"),
            }
        }
    });

    Ok(protocol)
}

// Common setup function for both Serve and Join modes
async fn setup_node(
    protocol: IrohLoroProtocol,
    key_path: Option<&str>,
) -> Result<iroh::protocol::Router> {
    let secret_key = if let Some(key_path) = key_path {
        // Load or generate secret key
        let key_file_path = dirs_next::cache_dir()
            .context("no dir for secret key")?
            .join("iroh-loro")
            .join(key_path);
        
        if key_file_path.exists() {
            let key_hex = tokio::fs::read_to_string(&key_file_path).await?;
            let key_hex = key_hex.trim();
            if key_hex.is_empty() {
                // File exists but is empty, generate new key
                let mut key_bytes = [0u8; 32];
                rand_core::OsRng.try_fill_bytes(&mut key_bytes).unwrap();
                let key = iroh::SecretKey::from_bytes(&key_bytes);
                tokio::fs::write(&key_file_path, hex::encode(key.to_bytes())).await?;
                key
            } else {
                let key_bytes = hex::decode(key_hex)?;
                let key_array: [u8; 32] = key_bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid key length"))?;
                iroh::SecretKey::from_bytes(&key_array)
            }
        } else {
            // Create directory if it doesn't exist
            if let Some(parent) = key_file_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // Generate random bytes using rand_core 0.9.3 API
            let mut key_bytes = [0u8; 32];
            rand_core::OsRng.try_fill_bytes(&mut key_bytes).unwrap();
            let key = iroh::SecretKey::from_bytes(&key_bytes);
            let key_bytes = key.to_bytes();
            tokio::fs::write(&key_file_path, hex::encode(key_bytes)).await?;
            key
        }
    } else {
        {
            // Generate random bytes using rand_core 0.9.3 API
            let mut key_bytes = [0u8; 32];
            rand_core::OsRng.try_fill_bytes(&mut key_bytes).unwrap();
            iroh::SecretKey::from_bytes(&key_bytes)
        }
    };

    let endpoint = iroh::Endpoint::builder()
        .discovery_n0()
        .discovery_local_network()
        .secret_key(secret_key)
        .bind()
        .await?;

    // Create and configure iroh node
    let iroh = iroh::protocol::Router::builder(endpoint)
        .accept(IrohLoroProtocol::ALPN, protocol)
        .spawn();

    // Get the node ID directly from the endpoint
    let node_id = iroh.endpoint().node_id();
    println!("Running\nNode Id: {}", node_id);

    Ok(iroh)
}

// File watcher for watching given file path that updates the loro doc when it changes
async fn watch_files(file_path: String, protocol: IrohLoroProtocol) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if event.kind.is_modify() {
                let _ = tx.try_send(());
            }
        }
    })?;
    
    watcher.watch(
        std::path::Path::new(&file_path),
        notify::RecursiveMode::NonRecursive,
    )?;
    
    println!("👀 Watching file: {}", file_path);
    
    while rx.recv().await.is_some() {
        // Small delay to avoid rapid fire events
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        
        match tokio::fs::read_to_string(&file_path).await {
            Ok(new_content) => {
                // Use the document directly for operations
                let doc = protocol.doc();
                let text = doc.get_text("text");
                let current_content = text.to_string();
                
                if current_content != new_content {
                    // Clear and insert new content
                    let current_len = text.len_utf16();
                    if current_len > 0 {
                        if let Err(e) = text.delete(0, current_len) {
                            println!("❌ Failed to delete text: {e}");
                            continue;
                        }
                    }
                    if !new_content.is_empty() {
                        if let Err(e) = text.insert(0, &new_content) {
                            println!("❌ Failed to insert text: {e}");
                            continue;
                        }
                    }
                    
                    // Commit and sync changes
                    if let Err(e) = protocol.commit_and_sync() {
                        println!("❌ Failed to commit and sync: {e}");
                    } else {
                        println!("📝 File change detected and synced to all peers");
                    }
                }
            }
            Err(e) => {
                println!("❌ Failed to read file: {e}");
            }
        }
    }
    
    Ok(())
}
