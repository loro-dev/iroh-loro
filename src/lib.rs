use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::Result;
use iroh::{endpoint::RecvStream, protocol::ProtocolHandler};
use loro::{ExportMode, LoroDoc};
use n0_future::{FuturesUnorderedBounded, StreamExt};
use tokio::select;

#[derive(Debug, Clone)]
pub struct IrohLoroProtocol {
    doc: Arc<LoroDoc>,
}

impl IrohLoroProtocol {
    pub const ALPN: &'static [u8] = b"iroh/loro/1";

    pub fn new(doc: Arc<LoroDoc>) -> Self {
        Self { doc }
    }
}

impl ProtocolHandler for IrohLoroProtocol {
    fn accept(
        &self,
        conn: iroh::endpoint::Connecting,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>> {
        let doc = self.doc.clone();
        Box::pin(async move {
            println!("🔌 Peer connected");
            let conn = conn.await?;
            let result = sync_doc(&doc, &conn).await;
            println!("🔌 Peer disconnected");
            if let Err(e) = result {
                println!("❌ Error: {}", e);
                return Err(e);
            }
            Ok(())
        })
    }
}

pub async fn sync_doc(doc: &LoroDoc, conn: &iroh::endpoint::Connection) -> Result<()> {
    // Subscribe to document updates
    let (tx, rx) = async_channel::bounded(128);
    let _sub = doc.subscribe_local_update(Box::new(move |u| {
        tx.send_blocking(u.clone()).unwrap();
        true
    }));

    // Initial sync
    {
        let msg = doc.export(ExportMode::all_updates())?;
        let mut stream = conn.open_uni().await?;
        stream.write_all(&msg).await?;
        stream.finish()?;
    }

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
                // capacity checked in precondition above
                running_syncs.push(handle_sync_message(doc, stream?));
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

async fn handle_sync_message(doc: &LoroDoc, mut stream: RecvStream) -> Result<()> {
    let msg = stream.read_to_end(10_000_000).await?; // 10 MB limit for now

    println!("📥 Received sync message from peer (size={})", msg.len());
    if let Err(e) = doc.import(&msg) {
        println!("❌ Failed to import sync message: {}", e);
    };
    println!("✅ Successfully imported sync message");

    Ok(())
}
