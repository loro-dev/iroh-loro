use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::Result;
use iroh::{endpoint::Connection, protocol::ProtocolHandler};
use loro::{ExportMode, LoroDoc};
use tokio::{
    select,
    sync::{Mutex, mpsc},
};

#[derive(Debug)]
pub struct IrohLoroProtocolInner {
    inner: Mutex<LoroDoc>,
    sender: mpsc::Sender<String>,
}

#[derive(Debug)]
pub struct IrohLoroProtocol {
    pub inner: Arc<IrohLoroProtocolInner>,
}

impl IrohLoroProtocolInner {
    pub const ALPN: &'static [u8] = b"iroh/loro/1";

    pub fn new(inner: LoroDoc, sender: mpsc::Sender<String>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(inner),
            sender,
        })
    }

    pub async fn update_doc(&self, new_doc: &str) {
        println!(
            "📝 Local file changed. Updating doc... (length={})",
            new_doc.len()
        );
        let doc = self.inner.lock().await;
        doc.get_text("text")
            .update(new_doc, loro::UpdateOptions::default())
            .unwrap();
        doc.commit();
        println!("✅ Local update committed");
    }

    pub async fn initiate_sync(self: Arc<Self>, conn: Connection) -> Result<()> {
        let (tx, rx) = async_channel::bounded(128);
        let _sub = self
            .inner
            .lock()
            .await
            .subscribe_local_update(Box::new(move |u| {
                tx.send_blocking(u.clone()).unwrap();
                true
            }));

        let sync = self.inner.lock().await.export(ExportMode::all_updates())?;

        // Initial sync
        let mut stream = conn.open_uni().await?;
        stream.write_all(&sync).await?;
        stream.finish()?;

        // Wait for changes & sync
        loop {
            select! {
                close = conn.closed() => {
                    println!("🔌 Peer disconnected: {close:?}");
                    return Ok(());
                },
                // Incoming sync messages
                stream = conn.accept_uni() => {
                    let mut stream = stream?;
                    tokio::spawn({
                        let this = self.clone();
                        async move {
                            let msg = stream.read_to_end(10_000).await?; // 10 KB limit for now
                            println!("📥 Received sync message from peer (size={})", msg.len());
                            if let Err(e) = this.inner.lock().await.import(&msg) {
                                println!("❌ Failed to import sync message: {}", e);
                            };
                            println!("✅ Successfully imported sync message");

                            this.sender
                                .send(this.inner.lock().await.get_text("text").to_string())
                                .await?;

                            println!("✅ Successfully sent update to local");

                            anyhow::Ok(())
                        }
                    });
                },
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

    pub async fn respond_sync(self: Arc<Self>, conn: iroh::endpoint::Connecting) -> Result<()> {
        let conn = conn.await?;
        self.initiate_sync(conn).await
    }
}

impl ProtocolHandler for IrohLoroProtocol {
    fn accept(
        &self,
        conn: Connection,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            println!("🔌 Peer connected");
            let result = inner.initiate_sync(conn).await;
            println!("🔌 Peer disconnected");
            if let Err(e) = result {
                println!("❌ Error: {}", e);
                return Err(e);
            }
            Ok(())
        })
    }
}
