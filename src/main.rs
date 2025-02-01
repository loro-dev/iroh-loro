use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, command};
use iroh_loro::IrohLoroProtocol;
use notify::Watcher;
use tokio::signal;
use tokio::sync::mpsc;
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

    let doc = loro::LoroDoc::new();
    let protocol = IrohLoroProtocol::new(doc.clone());

    let mut tasks = JoinSet::new();

    let (iroh, file_path) = match opts {
        Cli::Serve { file_path } => {
            // Initialize document with file contents
            let contents = tokio::fs::read_to_string(&file_path).await?;
            doc.get_text("text").insert(0, &contents)?;
            doc.commit();

            println!("Serving file: {}", file_path);

            let iroh = setup_node(protocol.clone(), Some("key.ed25519")).await?;

            (iroh, file_path)
        }

        Cli::Join {
            remote_id,
            file_path,
        } => {
            if !std::path::Path::new(&file_path).exists() {
                tokio::fs::write(&file_path, "").await?;
                println!("Created new file at: {file_path}");
            }

            let iroh = setup_node(protocol.clone(), None).await?;

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

            (iroh, file_path)
        }
    };

    // Subscribe to loro doc changes and write them to disk
    tasks.spawn({
        let doc = doc.clone();
        let file_path = file_path.clone();
        async move {
            if let Err(e) = write_txt_changes_to_file(doc, file_path).await {
                println!("❌ Loro subscription watcher task failed: {e}");
            }
        }
    });
    // Subscribe to disk changes and write them back to loro
    tasks.spawn({
        let doc = doc.clone();
        async move {
            if let Err(e) = watch_file_and_update_txt(file_path, doc).await {
                println!("❌ File watcher task failed: {e}");
            }
        }
    });

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
async fn write_txt_changes_to_file(doc: loro::LoroDoc, file_path: String) -> Result<()> {
    // This will the data we're working on
    let txt = doc.get_text("text");
    let (tx, mut rx) = mpsc::channel(100);
    let callback: loro::event::Subscriber = Arc::new({
        let txt = txt.clone();
        move |_event| {
            let _ignore = tx.try_send(txt.to_string());
        }
    });
    let _sub = doc.subscribe(&txt.id(), callback);

    while let Some(contents) = rx.recv().await {
        println!("💾 Writing new contents to file. Length={}", contents.len());
        match tokio::fs::write(&file_path, contents).await {
            Ok(_) => println!("✅ Successfully wrote to file"),
            Err(e) => println!("❌ Failed to write to file: {}", e),
        }
    }

    Ok(())
}

// Common setup function for both Serve and Join modes
async fn setup_node(
    protocol: IrohLoroProtocol,
    key_path: Option<&str>,
) -> Result<iroh::protocol::Router> {
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

    let endpoint = iroh::Endpoint::builder()
        .discovery_n0()
        .secret_key(secret_key)
        .bind()
        .await?;

    // Create and configure iroh node
    let iroh = iroh::protocol::Router::builder(endpoint)
        .accept(IrohLoroProtocol::ALPN, protocol)
        .spawn()
        .await?;

    let addr = iroh.endpoint().node_addr().await?;
    println!("Running\nNode Id: {}", addr.node_id);

    Ok(iroh)
}

// File watcher for watching given file path that updates the loro doc when it changes
async fn watch_file_and_update_txt(file_path: String, doc: loro::LoroDoc) -> Result<()> {
    println!("👀 Starting file watcher for: {file_path}");
    let (notify_tx, mut notify_rx) = mpsc::channel(100);
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = notify_tx.try_send(res); // ignore when the rx is dropped
    })?;

    watcher.watch(
        std::path::Path::new(&file_path),
        notify::RecursiveMode::NonRecursive,
    )?;

    while let Some(res) = notify_rx.recv().await {
        match res? {
            notify::Event {
                kind: notify::EventKind::Modify(_),
                ..
            } => {
                println!("📝 File modification detected");
                let contents = tokio::fs::read_to_string(&file_path).await?;
                println!("📖 Read file contents (length={})", contents.len());
                let mut opts = loro::UpdateOptions::default();
                if contents.len() > 50_000 {
                    opts.use_refined_diff = false;
                    doc.get_text("text").update_by_line(&contents, opts)?;
                } else {
                    doc.get_text("text").update(&contents, opts)?;
                }
                doc.commit();
                println!("✅ Local update committed");
            }
            _ => {
                // Ignoring other watcher events
            }
        }
    }

    Ok(())
}
