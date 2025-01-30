use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::Result;
use iroh::protocol::ProtocolHandler;
use loro::{ExportMode, LoroDoc};
use n0_future::{FuturesUnorderedBounded, StreamExt};
use tokio::{select, sync::mpsc};

#[derive(Debug, Clone)]
pub struct IrohLoroProtocol {
    doc: Arc<LoroDoc>,
    sender: mpsc::Sender<String>,
}

impl IrohLoroProtocol {
    pub const ALPN: &'static [u8] = b"iroh/loro/1";

    pub fn new(doc: LoroDoc, sender: mpsc::Sender<String>) -> Self {
        Self {
            doc: Arc::new(doc),
            sender,
        }
    }

    pub fn update_doc(&self, new_doc: &str) {
        println!(
            "📝 Local file changed. Updating doc... (length={})",
            new_doc.len()
        );
        self.doc
            .get_text("text")
            .update(new_doc, loro::UpdateOptions::default())
            .unwrap();
        self.doc.commit();
        println!("✅ Local update committed");
    }

    pub async fn initiate_sync(&self, conn: iroh::endpoint::Connection) -> Result<()> {
        let (tx, rx) = async_channel::bounded(128);
        let _sub = self.doc.subscribe_local_update(Box::new(move |u| {
            tx.send_blocking(u.clone()).unwrap();
            true
        }));

        let sync = self.doc.export(ExportMode::all_updates())?;

        // Initial sync
        let mut stream = conn.open_uni().await?;
        stream.write_all(&sync).await?;
        stream.finish()?;

        const MAX_CONCURRENT_SYNCS: usize = 20;
        let mut running_syncs = FuturesUnorderedBounded::new(MAX_CONCURRENT_SYNCS);

        // Wait for changes & sync
        loop {
            select! {
                close = conn.closed() => {
                    println!("🔌 Peer disconnected: {close:?}");
                    return Ok(());
                },
                // Accept incoming messages via uni-direction streams, if we have capacities to handle them
                stream = conn.accept_uni(), if running_syncs.len() < running_syncs.capacity() => {
                    let mut stream = stream?;
                    let push_result = running_syncs.try_push(async move {
                        let msg = stream.read_to_end(10_000).await?; // 10 KB limit for now

                        println!("📥 Received sync message from peer (size={})", msg.len());
                        if let Err(e) = self.doc.import(&msg) {
                            println!("❌ Failed to import sync message: {}", e);
                        };
                        println!("✅ Successfully imported sync message");

                        self.sender.send(self.doc.get_text("text").to_string()).await?;

                        println!("✅ Successfully sent update to local");

                        anyhow::Ok(())
                    });
                    if push_result.is_err() {
                        eprintln!("Cannot start sync - already have too many concurrent syncs running");
                    }
                },
                // Work on current syncs
                Some(result) = running_syncs.next() => {
                    if let Err(e) = result {
                        eprintln!("Sync failed: {e}");
                    }
                }
                // Responses to local document changes
                msg = rx.recv() => {
                    let msg = msg?;
                    println!("📤 Sending update to peer (size={})", msg.len());
                    let mut stream = conn.open_uni().await?;
                    stream.write_all(&msg).await?;
                    stream.finish()?;
                    println!("✅ Successfully sent update to peer");
                }
            }
        }
    }

    pub async fn respond_sync(&self, conn: iroh::endpoint::Connecting) -> Result<()> {
        let conn = conn.await?;
        self.initiate_sync(conn).await
    }
}

impl ProtocolHandler for IrohLoroProtocol {
    fn accept(
        &self,
        conn: iroh::endpoint::Connecting,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>> {
        let this = self.clone();
        Box::pin(async move {
            println!("🔌 Peer connected");
            let result = this.respond_sync(conn).await;
            println!("🔌 Peer disconnected");
            if let Err(e) = result {
                println!("❌ Error: {}", e);
                return Err(e);
            }
            Ok(())
        })
    }
}
