use std::{borrow::Cow, future::Future, pin::Pin, sync::Arc};

use anyhow::Result;
use iroh::{endpoint::RecvStream, protocol::ProtocolHandler};
use loro::{ExportMode, LoroDoc, VersionVector};
use n0_future::{FuturesUnorderedBounded, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

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

    pub fn doc(&self) -> &LoroDoc {
        &self.doc
    }

    pub async fn sync_changes(&self, conn: iroh::endpoint::Connection) -> Result<()> {
        let (tx, rx) = async_channel::bounded(128);
        let _sub = self.doc.subscribe_local_update(Box::new(move |u| {
            tx.send_blocking(u.clone()).unwrap();
            true
        }));

        let mut remote_missing_spans = Vec::new();

        const TASK_CONCURRENCY: usize = 20;
        let mut pending_recv = FuturesUnorderedBounded::new(TASK_CONCURRENCY);
        let mut pending_send_diff = FuturesUnorderedBounded::new(TASK_CONCURRENCY);
        let mut pending_send_vv = FuturesUnorderedBounded::new(TASK_CONCURRENCY);

        // Wait for changes & sync
        loop {
            tokio::select! {
                close = conn.closed() => {
                    println!("🔌 Peer disconnected: {close:?}");
                    return Ok(());
                },
                // Accept incoming messages via uni-direction streams, if we have capacities to handle them
                stream = conn.accept_uni(), if pending_recv.len() < pending_recv.capacity() => {
                    // capacity checked in precondition above
                    pending_recv.push(self.handle_sync_message(stream?));
                },
                // Work on receiving messages
                Some(result) = pending_recv.next() => {
                    match result {
                        Ok(Some(remote_vv)) => {
                            let version = self.doc.state_vv();
                            let (remote_missing, mut us_missing) = version.diff_iter(&remote_vv);
                            remote_missing_spans = remote_missing.collect();
                            if us_missing.next().is_some() && pending_send_vv.len() < pending_send_vv.capacity() {
                                pending_send_vv.push(self.send_vv(&conn));
                            }
                            // Fill capacity
                            while pending_send_diff.len() < pending_send_diff.capacity() {
                                let Some(missing) = remote_missing_spans.pop() else {
                                    break;
                                };
                                let diff = self.doc.export(ExportMode::updates_in_range(Cow::Owned(vec![missing])))?;
                                pending_send_diff.push(self.send_diff(&conn, diff));
                            }
                        }
                        Ok(None) => {},
                        Err(e) => {
                            eprintln!("Receiving message failed: {e}");
                        }
                    }
                },
                // Work on sending diffs
                Some(result) = pending_send_diff.next() => {
                    if let Err(e) = result {
                        eprintln!("Sending diff failed: {e}");
                    }
                    // Keep sending further missing diffs if we have capacity & know remote is missing spans
                    while pending_send_diff.len() < pending_send_diff.capacity() {
                        let Some(missing) = remote_missing_spans.pop() else {
                            break;
                        };
                        let diff = self.doc.export(ExportMode::updates_in_range(Cow::Owned(vec![missing])))?;
                        pending_send_diff.push(self.send_diff(&conn, diff));
                    }
                },
                // Work on sending version vector updates
                Some(result) = pending_send_vv.next() => {
                    if let Err(e) = result {
                        eprintln!("Sending version vector update failed: {e}");
                    }
                },
                // Responses to local document changes
                msg = rx.recv(), if pending_send_diff.len() < pending_send_diff.capacity() => {
                    // capacity checked in precondition above
                    pending_send_diff.push(self.send_diff(&conn, msg?));
                }
            }
        }
    }

    async fn handle_sync_message(&self, mut stream: RecvStream) -> Result<Option<VersionVector>> {
        let msg_type = MessageType::decode(stream.read_u8().await?)?;
        match msg_type {
            MessageType::Diff => {
                let msg = stream.read_to_end(10_000_000).await?; // 10 MB limit for now

                println!("📥 Received sync message from peer (size={})", msg.len());
                if let Err(e) = self.doc.import(&msg) {
                    println!("❌ Failed to import sync message: {e}");
                };
                println!("✅ Successfully imported sync message");

                self.sender
                    .send(self.doc.get_text("text").to_string())
                    .await?;

                println!("✅ Successfully sent update to local");
                Ok(None)
            }
            MessageType::StateVV => {
                let msg = stream.read_to_end(10_000).await?; // 10 KB limit for now

                println!(
                    "📥 Received state version vector message from peer (size={})",
                    msg.len()
                );

                let vv = VersionVector::decode(&msg)?;
                Ok(Some(vv))
            }
        }
    }

    async fn send_diff(&self, conn: &iroh::endpoint::Connection, diff: Vec<u8>) -> Result<()> {
        println!("📤 Sending diff update to peer (size={})", diff.len());
        let mut stream = conn.open_uni().await?;
        stream.write_u8(MessageType::Diff.encode()).await?;
        stream.write_all(&diff).await?;
        stream.finish()?;
        println!("✅ Successfully sent update to peer");
        Ok(())
    }

    async fn send_vv(&self, conn: &iroh::endpoint::Connection) -> Result<()> {
        let msg = self.doc.state_vv().encode();
        println!("📤 Sending vv update to peer (size={})", msg.len());
        let mut stream = conn.open_uni().await?;
        stream.write_u8(MessageType::StateVV.encode()).await?;
        stream.write_all(&msg).await?;
        stream.finish()?;
        println!("✅ Successfully sent vv update to peer");
        Ok(())
    }

    pub async fn initiate_sync(&self, conn: iroh::endpoint::Connection) -> Result<()> {
        let conn_send = conn.clone();
        // Do two things simultaneously:
        // 1. send our version vector
        // 2. run a loop that accepts new version vectors and sends updates
        n0_future::future::try_zip(self.send_vv(&conn_send), self.sync_changes(conn)).await?;
        Ok(())
    }

    pub async fn respond_sync(&self, conn: iroh::endpoint::Connecting) -> Result<()> {
        let conn = conn.await?;
        // Just run the protocol loop. We'll receive a version vector from the remote
        // that informs us about what we need to send over.
        // We'll do that as part of the loop.
        self.sync_changes(conn).await
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

#[derive(Debug, Clone, Copy)]
enum MessageType {
    Diff = 1,
    StateVV = 2,
}

impl MessageType {
    pub fn encode(self) -> u8 {
        self as u8
    }

    pub fn decode(n: u8) -> Result<Self> {
        match n {
            1 => Ok(Self::Diff),
            2 => Ok(Self::StateVV),
            _ => Err(anyhow::anyhow!(
                "Unexpected message type, expected 1 or 2, but got {n}"
            )),
        }
    }
}
