use std::{future::Future, ops::DerefMut, pin::Pin, sync::Arc};

use anyhow::Result;
use iroh::{
    net::endpoint::{RecvStream, SendStream},
    router::ProtocolHandler,
};
use loro::LoroDoc;
use serde::{Deserialize, Serialize};
use tokio::{
    select,
    sync::{Mutex, mpsc},
};

#[derive(Debug)]
pub struct IrohLoroProtocol {
    inner: Mutex<LoroDoc>,
    sender: mpsc::Sender<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Protocol {
    SyncMessage(Vec<u8>),
    Ack,
}

impl IrohLoroProtocol {
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

    pub async fn initiate_sync(
        self: Arc<Self>,
        conn: iroh::net::endpoint::Connection,
    ) -> Result<()> {
        let (mut send, mut recv) = conn.open_bi().await?;
        let (tx, rx) = std::sync::mpsc::channel();
        let _sub = self
            .inner
            .lock()
            .await
            .subscribe_local_update(Box::new(move |u| {
                tx.send(u.clone()).unwrap();
                true
            }));
        Self::send_msg(Protocol::Ack, &mut send).await.unwrap();
        let send = Arc::new(tokio::sync::Mutex::new(send));
        let send_clone = send.clone();
        let _h = tokio::spawn({
            async move {
                while let Ok(update) = rx.recv() {
                    println!("📤 Sending local update to peer (size={})", update.len());
                    if let Err(e) =
                        Self::send_msg(Protocol::SyncMessage(update), send.lock().await.deref_mut())
                            .await
                    {
                        println!("❌ Failed to send update: {}", e);
                        break;
                    }
                    println!("✅ Successfully sent update to peer");
                }
            }
        });
        select! {
            _ = conn.closed() => {
                println!("🔌 Peer disconnected");
            }
            result = async {
                loop {
                    match Self::recv_msg(&mut recv).await {
                        Ok(Protocol::Ack) => {
                            println!("📥 Received ack from peer");
                        }
                        Ok(Protocol::SyncMessage(sync_msg)) => {
                            println!("📥 Received sync message from peer (size={})", sync_msg.len());
                            if let Err(e) = self.inner.lock().await.import(&sync_msg) {
                                println!("❌ Failed to import sync message: {}", e);
                                return Err(anyhow::Error::from(e));
                            }
                            println!("✅ Successfully imported sync message");

                            self.sender
                                .send(self.inner.lock().await.get_text("text").to_string())
                                .await?;
                            println!("✅ Successfully sent update to local");
                            Self::send_msg(Protocol::Ack, send_clone.lock().await.deref_mut())
                                .await?;
                        }
                        Err(e) => {
                            println!("🔌 Connection closed or error: {}", e);
                            return Ok(());
                        }
                    }
                }
            } => {
                result?
            }
        }

        println!("DONE!");
        Ok(())
    }

    pub async fn respond_sync(
        self: Arc<Self>,
        conn: iroh::net::endpoint::Connecting,
    ) -> Result<()> {
        let conn = conn.await?;
        let (mut send, mut recv) = conn.accept_bi().await?;
        let loro_doc = self.inner.lock().await;
        let mut last_version = loro_doc.oplog_vv();
        let bytes = loro_doc.export(loro::ExportMode::Snapshot).unwrap();
        Self::send_msg(Protocol::SyncMessage(bytes.clone()), &mut send).await?;
        // Self::send_msg(Protocol::SyncMessage(bytes), &mut send).await?;
        let (tx, rx) = std::sync::mpsc::channel();
        let _sub = loro_doc.subscribe_root(Arc::new(move |_| {
            tx.send(()).unwrap();
        }));
        drop(loro_doc);
        let this = self.clone();
        let send = Arc::new(tokio::sync::Mutex::new(send));
        let send_clone = send.clone();
        let _h = tokio::spawn({
            async move {
                while let Ok(_) = rx.recv() {
                    let update = this
                        .inner
                        .lock()
                        .await
                        .export(loro::ExportMode::updates(&last_version))
                        .unwrap();
                    last_version = this.inner.lock().await.oplog_vv();
                    println!("📤 Sending update to peer (size={})", update.len());
                    if let Err(e) =
                        Self::send_msg(Protocol::SyncMessage(update), send.lock().await.deref_mut())
                            .await
                    {
                        println!("❌ Failed to send update: {}", e);
                        break;
                    }
                    println!("✅ Successfully sent update to peer");
                }
            }
        });
        select! {
            _ = conn.closed() => {
                println!("🔌 Peer disconnected");
            }
            result = async {
                loop {
                    match Self::recv_msg(&mut recv).await {
                        Ok(Protocol::Ack) => {
                            println!("📥 Received ack from peer");
                        }
                        Ok(Protocol::SyncMessage(sync_msg)) => {
                            println!("📥 Received sync message from peer (size={})", sync_msg.len());
                            if let Err(e) = self.inner.lock().await.import(&sync_msg) {
                                println!("❌ Failed to import sync message: {}", e);
                                return Err(anyhow::Error::from(e));
                            }
                            println!("✅ Successfully imported sync message");

                            self.sender
                                .send(self.inner.lock().await.get_text("text").to_string())
                                .await?;
                            println!("✅ Successfully sent update to local");
                            Self::send_msg(Protocol::Ack, send_clone.lock().await.deref_mut()).await.unwrap();
                        }
                        Err(e) => {
                            println!("🔌 Connection closed or error: {}", e);
                            return Ok(());
                        }
                    }
                }
            } => {
                result?
            }
        }

        println!("DONE!");
        Ok(())
    }

    async fn send_msg(msg: Protocol, send: &mut SendStream) -> Result<()> {
        let encoded = postcard::to_stdvec(&msg)?;
        send.write_all(&(encoded.len() as u64).to_le_bytes())
            .await?;
        send.write_all(&encoded).await?;
        Ok(())
    }

    async fn recv_msg(recv: &mut RecvStream) -> Result<Protocol> {
        let mut incoming_len = [0u8; 8];
        if let Err(e) = recv.read_exact(&mut incoming_len).await {
            println!("📡 Connection closed while reading length: {}", e);
            return Err(e.into());
        }
        let len = u64::from_le_bytes(incoming_len);
        println!("📨 Reading message of length: {}", len);

        let mut buffer = vec![0u8; len as usize];
        if let Err(e) = recv.read_exact(&mut buffer).await {
            println!("📡 Connection closed while reading message: {}", e);
            return Err(e.into());
        }

        let msg = postcard::from_bytes(&buffer)?;
        Ok(msg)
    }
}

impl ProtocolHandler for IrohLoroProtocol {
    fn accept(
        self: Arc<Self>,
        conn: iroh::net::endpoint::Connecting,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>> {
        Box::pin(async move {
            println!("🔌 Peer connected");
            let result = Arc::clone(&self).respond_sync(conn).await;
            println!("🔌 Peer disconnected");
            if let Err(e) = result {
                println!("❌ Error: {}", e);
                return Err(e);
            }
            Ok(())
        })
    }
}
