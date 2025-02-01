use std::{borrow::Cow, cmp::Ordering, future::Future, pin::Pin, sync::Mutex};

use anyhow::Result;
use iroh::{endpoint::RecvStream, protocol::ProtocolHandler};
use loro::{ExportMode, LoroDoc, VersionVector};
use n0_future::{FuturesUnorderedBounded, StreamExt};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct IrohLoroProtocol {
    doc: LoroDoc,
}

impl IrohLoroProtocol {
    pub const ALPN: &'static [u8] = b"iroh/loro/1";

    pub fn new(doc: LoroDoc) -> Self {
        Self { doc }
    }

    pub fn doc(&self) -> &LoroDoc {
        &self.doc
    }

    pub async fn initiate_sync(&self, conn: iroh::endpoint::Connection) -> Result<()> {
        let vv_msg = Message {
            version_vector: Some(self.doc.oplog_vv()),
            diff: None,
        };
        let session = SyncSession {
            conn: &conn,
            doc: &self.doc,
            remote_vv: Mutex::new(VersionVector::new()),
        };
        // Do two things simultaneously:
        // 1. send our version vector
        // 2. run a loop that accepts messages from the remote and sends updates
        n0_future::future::try_zip(session.send(vv_msg), session.run_sync()).await?;
        Ok(())
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
            if let Err(e) = result {
                println!("❌ Error: {e}");
                return Err(e);
            }
            Ok(())
        })
    }
}

struct SyncSession<'a> {
    doc: &'a LoroDoc,
    conn: &'a iroh::endpoint::Connection,
    remote_vv: Mutex<VersionVector>,
}

impl SyncSession<'_> {
    async fn run_sync(&self) -> Result<()> {
        let (tx, rx) = async_channel::bounded(128);
        let _sub = self.doc.subscribe_local_update(Box::new(move |u| {
            tx.send_blocking(u.clone()).unwrap();
            true
        }));

        const TASK_CONCURRENCY: usize = 20;
        let mut pending_recv = FuturesUnorderedBounded::new(TASK_CONCURRENCY);
        let mut pending_send = FuturesUnorderedBounded::new(TASK_CONCURRENCY);

        // Wait for changes & sync
        loop {
            tokio::select! {
                close = self.conn.closed() => {
                    println!("🔌 Peer disconnected: {close:?}");
                    return Ok(());
                },
                // Accept incoming messages via uni-direction streams, if we have capacities to handle them
                stream = self.conn.accept_uni(), if has_capacity(&pending_recv) && has_capacity(&pending_send) => {
                    // capacity checked in precondition above
                    pending_recv.push(self.recv(stream?));
                },
                // Work on receiving messages
                Some(result) = pending_recv.next(), if has_capacity(&pending_send) => {
                    match result {
                        Ok(None) => {}
                        Ok(Some(message)) => {
                            pending_send.push(self.send(message));
                        }
                        Err(e) => {
                            eprintln!("Receiving message failed: {e}");
                        }
                    }
                },
                // Work on sending diffs
                Some(result) = pending_send.next() => {
                    if let Err(e) = result {
                        eprintln!("Sending message failed: {e}");
                    }
                },
                // Responses to local document changes
                msg = rx.recv(), if has_capacity(&pending_send) => {
                    // capacity checked in precondition above
                    pending_send.push(self.send(Message {
                        version_vector: None,
                        diff: Some(Diff { bytes: msg?.into() })
                    }));
                }
            }
        }
    }

    async fn recv(&self, mut stream: RecvStream) -> Result<Option<Message<'static>>> {
        let msg = stream.read_to_end(10_000_000).await?; // 10 MB limit for now
        let message: Message<'_> = postcard::from_bytes(&msg)?;
        println!(
            "📥 Received sync msg from peer (size={} has_vv={} has_diff={})",
            msg.len(),
            message.version_vector.is_some(),
            message.diff.is_some()
        );

        if let Some(diff) = message.diff {
            let status = self.doc.import(diff.as_ref())?;
            println!(
                "📥 Imported diff (pending={} any_successes={})",
                status.pending.is_some(),
                status.success.is_empty()
            );
        }

        let mut remote_vv = self.remote_vv.lock().unwrap_or_else(|p| p.into_inner()); // Ignore lock poisons.

        if let Some(received_remote_vv) = message.version_vector {
            remote_vv.extend_to_include_vv(received_remote_vv.iter());
        }

        let our_vv = self.doc.oplog_vv();
        match our_vv.partial_cmp(&remote_vv) {
            None => {
                // We diverged: Send a diff and request to get a diff back, too
                println!("⛓️‍💥 We are diverged");
                let diff = self.doc.export(ExportMode::updates(&remote_vv))?;
                Ok(Some(Message {
                    version_vector: Some(our_vv),
                    diff: Some(Diff { bytes: diff.into() }),
                }))
            }
            Some(Ordering::Greater) => {
                // We're ahead: Send a diff, but no need to tell the other side to update us
                println!("📈 We are ahead");
                let diff = self.doc.export(ExportMode::updates(&remote_vv))?;
                Ok(Some(Message {
                    version_vector: None,
                    diff: Some(Diff { bytes: diff.into() }),
                }))
            }
            Some(Ordering::Less) => {
                // We are behind: we inform the other side about what we're missing, apparently we didn't get it
                println!("📉 We are behind");
                Ok(Some(Message {
                    version_vector: Some(our_vv),
                    diff: None,
                }))
            }
            Some(Ordering::Equal) => {
                println!("🤝 In sync with peer");
                Ok(None)
            }
        }
    }

    async fn send(&self, message: Message<'_>) -> Result<()> {
        let msg = postcard::to_allocvec(&message)?;
        println!(
            "📤 Sending sync msg to peer (size={} has_vv={} has_diff={})",
            msg.len(),
            message.version_vector.is_some(),
            message.diff.is_some()
        );
        let mut stream = self.conn.open_uni().await?;
        stream.write_all(&msg).await?;
        stream.finish()?;
        Ok(())
    }
}

fn has_capacity<F>(tasks: &FuturesUnorderedBounded<F>) -> bool {
    tasks.len() < tasks.capacity()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Message<'a> {
    version_vector: Option<VersionVector>,
    #[serde(borrow)]
    diff: Option<Diff<'a>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Diff<'a> {
    #[serde(borrow, with = "serde_bytes")]
    bytes: Cow<'a, [u8]>,
}

impl<'a> AsRef<[u8]> for Diff<'a> {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}
