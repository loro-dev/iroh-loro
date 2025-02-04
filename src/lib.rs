use std::{
    borrow::Cow,
    cmp::Ordering,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{self, AtomicBool},
        Mutex,
    },
};

use anyhow::Result;
use iroh::{
    endpoint::{get_remote_node_id, ConnectionError, RecvStream},
    protocol::ProtocolHandler,
};
use loro::{ExportMode, LoroDoc, VersionVector};
use n0_future::{FuturesUnorderedBounded, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, error_span, info, Instrument};

#[derive(Debug, Clone)]
pub struct IrohLoroProtocol {
    doc: LoroDoc,
}

#[derive(Debug, Clone, Copy)]
pub enum SyncMode {
    Continuous,
    Once,
}

impl IrohLoroProtocol {
    pub const ALPN: &'static [u8] = b"iroh/loro/1";

    pub fn new(doc: LoroDoc) -> Self {
        Self { doc }
    }

    pub fn doc(&self) -> &LoroDoc {
        &self.doc
    }

    pub async fn initiate_sync(
        &self,
        conn: iroh::endpoint::Connection,
        mode: SyncMode,
    ) -> Result<()> {
        let vv_msg = Message {
            version_vector: Some(self.doc.oplog_vv()),
            ..Message::default()
        };
        let session = SyncSession {
            conn: conn.clone(),
            doc: self.doc.clone(),
            close_when_done: match mode {
                SyncMode::Continuous => false.into(),
                SyncMode::Once => true.into(),
            },
            remote: Mutex::new(RemoteState {
                vv: VersionVector::new(),
            }),
        };
        // Do two things simultaneously:
        // 1. send our version vector
        // 2. run a loop that accepts messages from the remote and sends updates
        n0_future::future::try_zip(session.send(vv_msg), session.run_sync()).await?;
        Ok(())
    }

    pub async fn respond_sync(&self, conn: iroh::endpoint::Connecting) -> Result<()> {
        let conn = conn.await?;
        let peer = get_remote_node_id(&conn)?;
        info!(peer=%peer.fmt_short(),"incoming sync request");
        self.initiate_sync(conn, SyncMode::Continuous)
            .instrument(error_span!("accept", peer=%peer.fmt_short()))
            .await
    }
}

impl ProtocolHandler for IrohLoroProtocol {
    fn accept(
        &self,
        conn: iroh::endpoint::Connecting,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>> {
        let this = self.clone();
        Box::pin(async move {
            let result = this.respond_sync(conn).await;
            if let Err(e) = result {
                error!("incoming sync request failed: {e}");
                return Err(e);
            }
            Ok(())
        })
    }
}

struct SyncSession {
    doc: LoroDoc,
    conn: iroh::endpoint::Connection,
    close_when_done: AtomicBool,
    remote: Mutex<RemoteState>,
}

struct RemoteState {
    vv: VersionVector,
}

impl SyncSession {
    async fn run_sync(&self) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let _sub = self.doc.subscribe_local_update(Box::new(move |_u| {
            info!("doc updated locally, queue update message");
            tx.try_send(()).ok();
            true
        }));

        const TASK_CONCURRENCY: usize = 20;
        let mut pending_recv = FuturesUnorderedBounded::new(TASK_CONCURRENCY);
        let mut pending_send = FuturesUnorderedBounded::new(TASK_CONCURRENCY);

        // Wait for changes & sync
        loop {
            tokio::select! {
                close = self.conn.closed() => {
                    info!("🔌 Peer disconnected: {close:?}");
                    break;
                },
                // Accept incoming messages via uni-direction streams, if we have capacities to handle them
                stream = self.conn.accept_uni(), if has_capacity(&pending_recv) && has_capacity(&pending_send) => {
                    // capacity checked in precondition above
                    match stream {
                        Err(ConnectionError::ApplicationClosed(close)) => {
                            info!("🔌 Peer disconnected: connection closed by peer {close:?}");
                            break;
                        },
                        Err(ConnectionError::LocallyClosed) => {
                            info!("🔌 Peer disconnected: connection closed by us");
                            break;
                        },
                        Err(err) => {
                            error!("🔌 Peer disconnected with error: {err:?}");
                            return Err(err.into());
                        }
                        Ok(stream) => pending_recv.push(self.recv(stream)),
                    }
                },
                // Work on receiving messages
                Some(result) = pending_recv.next(), if has_capacity(&pending_send) => {
                    match result {
                        Ok(()) => {
                            if let Some(message) = self.update_if_needed()? {
                                pending_send.push(self.send(message));
                            }
                        }
                        Err(e) => {
                            error!("Receiving message failed: {e}");
                        }
                    }
                },
                // Work on sending diffs
                Some(result) = pending_send.next() => {
                    if let Err(e) = result {
                        error!("Sending message failed: {e}");
                    }
                },
                // Responses to local document changes
                _ = rx.recv(), if has_capacity(&pending_send) && !rx.is_closed() => {
                    // capacity checked in precondition above
                    if let Some(message) = self.update_if_needed()? {
                        pending_send.push(self.send(message));
                    }
                }
            }
        }
        Ok(())
    }

    async fn recv(&self, mut stream: RecvStream) -> Result<()> {
        let msg = stream.read_to_end(10_000_000).await?; // 10 MB limit for now
        let message: Message<'_> = postcard::from_bytes(&msg)?;
        info!(
            size = msg.len(),
            has_diff = message.diff.is_some(),
            has_vv = message.version_vector.is_some(),
            "Received sync msg from peer",
        );

        if let Some(vv) = &message.version_vector {
            let mut remote = self.remote.lock().unwrap_or_else(|p| p.into_inner()); // Ignore lock poisons.
            remote.vv.extend_to_include_vv(vv.iter());
        }

        if let Some(diff) = message.diff {
            let status = self.doc.import(diff.as_ref())?;
            info!(
                "📥 Imported diff (pending={} any_successes={})",
                status.pending.is_some(),
                status.success.is_empty()
            );
        }

        self.close_when_done
            .fetch_or(message.close_when_done, atomic::Ordering::Relaxed);

        Ok(())
    }

    async fn send(&self, message: Message<'_>) -> Result<()> {
        let msg = postcard::to_allocvec(&message)?;
        info!(
            size = msg.len(),
            has_diff = message.diff.is_some(),
            has_vv = message.version_vector.is_some(),
            "Sending sync msg to peer",
        );
        let mut stream = self.conn.open_uni().await?;
        stream.write_all(&msg).await?;
        stream.finish()?;
        Ok(())
    }

    fn update_if_needed(&self) -> Result<Option<Message<'static>>> {
        let close_when_done = self.close_when_done.load(atomic::Ordering::Relaxed);
        let our_vv = self.doc.oplog_vv();
        let mut remote = self.remote.lock().unwrap_or_else(|p| p.into_inner()); // Ignore lock poisons.
        Ok(match our_vv.partial_cmp(&remote.vv) {
            None => {
                // We diverged: Send a diff and request to get a diff back, too
                info!("⛓️‍💥 We are diverged");
                let diff = self.doc.export(ExportMode::updates(&remote.vv))?;
                // We assume that the remote will eventually receive our message and be on our state
                remote.vv.extend_to_include_vv(our_vv.iter());
                Some(Message {
                    version_vector: Some(our_vv),
                    close_when_done,
                    diff: Some(Diff { bytes: diff.into() }),
                })
            }
            Some(Ordering::Greater) => {
                // We're ahead: Send a diff, but no need to tell the other side to update us
                info!("📈 We are ahead");
                let diff = self.doc.export(ExportMode::updates(&remote.vv))?;
                // We assume that the remote will eventually receive our message and be on our state
                remote.vv.extend_to_include_vv(our_vv.iter());
                info!("🤝 Assuming to be in sync once peer receives this");
                Some(Message {
                    version_vector: None,
                    close_when_done,
                    diff: Some(Diff { bytes: diff.into() }),
                })
            }
            Some(Ordering::Less) => {
                // We are behind: we inform the other side about what we're missing, apparently we didn't get it
                info!("📉 We are behind");
                Some(Message {
                    version_vector: Some(our_vv),
                    close_when_done,
                    diff: None,
                })
            }
            Some(Ordering::Equal) => {
                info!("🤝 In sync with peer");
                if close_when_done {
                    self.conn.close(0u32.into(), b"in sync, thank you");
                }
                None
            }
        })
    }
}

fn has_capacity<F>(tasks: &FuturesUnorderedBounded<F>) -> bool {
    tasks.len() < tasks.capacity()
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct Message<'a> {
    version_vector: Option<VersionVector>,
    close_when_done: bool,
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

#[cfg(test)]
mod tests {

    use std::{sync::Arc, time::Duration};

    use iroh::protocol::Router;
    use loro::{EventTriggerKind, LoroDoc};
    use tracing::{error_span, info, Instrument};
    use tracing_test::traced_test;

    use crate::{IrohLoroProtocol, SyncMode};

    async fn setup_node(doc: LoroDoc) -> anyhow::Result<(Router, IrohLoroProtocol)> {
        let proto = IrohLoroProtocol::new(doc);
        let endpoint = iroh::Endpoint::builder().bind().await?;

        // Create and configure iroh node
        let router = iroh::protocol::Router::builder(endpoint)
            .accept(IrohLoroProtocol::ALPN, proto.clone())
            .spawn()
            .await?;
        Ok((router, proto))
    }

    #[tokio::test]
    #[traced_test]
    async fn basic() {
        let doc_a = LoroDoc::new();
        let doc_b = LoroDoc::new();
        doc_b
            .get_text("text")
            .update("hello", Default::default())
            .unwrap();
        assert_eq!(doc_b.get_text("text").to_string().as_str(), "hello");
        assert_eq!(doc_a.get_text("text").to_string().as_str(), "");

        let (router_a, proto_a) = setup_node(doc_a.clone()).await.unwrap();
        let (router_b, _proto_b) = setup_node(doc_b.clone()).await.unwrap();

        let addr_b = router_b.endpoint().node_addr().await.unwrap();

        let conn_a_to_b = router_a
            .endpoint()
            .connect(addr_b.clone(), IrohLoroProtocol::ALPN)
            .await
            .unwrap();

        proto_a
            .initiate_sync(conn_a_to_b, SyncMode::Once)
            .instrument(error_span!("connect", peer = %addr_b.node_id.fmt_short()))
            .await
            .unwrap();

        assert_eq!(doc_b.get_text("text").to_string().as_str(), "hello");
        assert_eq!(doc_a.get_text("text").to_string().as_str(), "hello");
    }

    #[tokio::test]
    #[traced_test]
    async fn updates() {
        let doc_a = LoroDoc::new();
        let doc_b = LoroDoc::new();
        let (router_a, proto_a) = setup_node(doc_a.clone()).await.unwrap();
        let (router_b, _proto_b) = setup_node(doc_b.clone()).await.unwrap();

        let addr_b = router_b.endpoint().node_addr().await.unwrap();

        let conn_a_to_b = router_a
            .endpoint()
            .connect(addr_b.clone(), IrohLoroProtocol::ALPN)
            .await
            .unwrap();

        let _t = tokio::task::spawn(async move {
            proto_a
                .initiate_sync(conn_a_to_b, SyncMode::Continuous)
                .instrument(error_span!("connect", peer = %addr_b.node_id.fmt_short()))
                .await
                .unwrap();
        });

        // setup update listener channels
        let (update_a_tx, mut update_a_rx) = tokio::sync::mpsc::channel(1);
        let _sub = doc_a.subscribe_root(Arc::new(move |update| {
            if update.triggered_by == EventTriggerKind::Import {
                update_a_tx.try_send(()).ok();
            }
        }));
        let (update_b_tx, mut update_b_rx) = tokio::sync::mpsc::channel(1);
        let _sub = doc_b.subscribe_root(Arc::new(move |update| {
            if update.triggered_by == EventTriggerKind::Import {
                update_b_tx.try_send(()).ok();
            }
        }));

        info!("now update text on a");
        doc_a
            .get_text("text")
            .update("a", Default::default())
            .unwrap();
        doc_a.commit();
        let _ = tokio::time::timeout(Duration::from_millis(500), update_b_rx.recv())
            .await
            .expect("did not receive update within timeout")
            .expect("update channel closed before receiving update");

        info!("b received update from a");
        assert_eq!(doc_b.get_text("text").to_string().as_str(), "a");

        info!("now update text on b");
        doc_b
            .get_text("text")
            .update("b", Default::default())
            .unwrap();
        doc_b.commit();

        let _ = tokio::time::timeout(Duration::from_millis(1000), update_a_rx.recv())
            .await
            .expect("did not receive update within timeout")
            .expect("update channel closed before receiving update");
        info!("a received update from b");
        assert_eq!(doc_a.get_text("text").to_string().as_str(), "b");
    }
}
