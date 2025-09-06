use std::sync::Arc;

use anyhow::Result;
use iroh::protocol::{ProtocolHandler, AcceptError};
use loro::{ExportMode, LoroDoc};
use tokio::sync::broadcast;


#[derive(Debug, Clone)]
pub struct IrohLoroProtocol {
    doc: Arc<LoroDoc>,
    change_broadcaster: broadcast::Sender<Vec<u8>>, // Broadcast changes to all peers
}

impl IrohLoroProtocol {
    pub const ALPN: &'static [u8] = b"iroh/loro/1";

    pub fn new(doc: LoroDoc, change_broadcaster: broadcast::Sender<Vec<u8>>) -> Self {
        Self {
            doc: Arc::new(doc),
            change_broadcaster,
        }
    }

    /// Get a reference to the underlying LoroDoc for direct operations
    pub fn doc(&self) -> &Arc<LoroDoc> {
        &self.doc
    }

    /// Get the current document state as bytes for syncing
    pub fn export_snapshot(&self) -> Result<Vec<u8>> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// Import changes from another peer
    pub fn import_changes(&self, data: &[u8]) -> Result<()> {
        self.doc.import(data)?;
        Ok(())
    }

    /// Get all updates for syncing
    pub fn export_updates(&self) -> Result<Vec<u8>> {
        Ok(self.doc.export(ExportMode::all_updates())?)
    }

    /// Commit any pending changes and notify peers
    pub fn commit_and_sync(&self) -> Result<()> {
        self.doc.commit();
        
        // Export recent changes and broadcast to all peers
        let changes = self.export_updates()?;
        if !changes.is_empty() {
            let _ = self.change_broadcaster.send(changes);
        }
        
        Ok(())
    }


    pub async fn initiate_sync(&self, conn: iroh::endpoint::Connection) -> Result<()> {
        println!("🔄 Starting sync with peer");
        
        let (mut send_stream, mut recv_stream) = conn.open_bi().await?;
        
        // Send our current document state
        let export_data = self.export_snapshot()?;
        let data_len = export_data.len() as u32;
        send_stream.write_all(&data_len.to_le_bytes()).await?;
        send_stream.write_all(&export_data).await?;
        
        println!("📤 Sent document snapshot ({} bytes)", export_data.len());
        
        // Subscribe to changes to forward to this peer
        let mut change_receiver = self.change_broadcaster.subscribe();
        let (change_tx, mut change_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(100);
        
        // Clone self for the import task
        let protocol_for_import = self.clone();
        
        // Spawn task to forward local changes to this peer
        let forward_task = tokio::spawn(async move {
            while let Ok(changes) = change_receiver.recv().await {
                let _ = change_tx.send(changes).await;
            }
        });
        
        // Handle both sending and receiving in parallel
        let send_recv_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Send changes to peer
                    Some(changes) = change_rx.recv() => {
                        let data_len = changes.len() as u32;
                        if send_stream.write_all(&data_len.to_le_bytes()).await.is_err() {
                            break;
                        }
                        if send_stream.write_all(&changes).await.is_err() {
                            break;
                        }
                        println!("📤 Forwarded changes to peer ({} bytes)", changes.len());
                    }
                    
                    // Receive changes from peer
                    result = async {
                        let mut buffer = [0u8; 4];
                        match recv_stream.read_exact(&mut buffer).await {
                            Ok(_) => {
                                let msg_len = u32::from_le_bytes(buffer) as usize;
                                let mut msg_buffer = vec![0u8; msg_len];
                                match recv_stream.read_exact(&mut msg_buffer).await {
                                    Ok(_) => Ok(msg_buffer),
                                    Err(_) => Err("Failed to read message body")
                                }
                            }
                            Err(_) => Err("Failed to read message length")
                        }
                    } => {
                        match result {
                            Ok(msg_buffer) => {
                                println!("📥 Received sync message ({} bytes)", msg_buffer.len());
                                let _ = protocol_for_import.import_changes(&msg_buffer);
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });
        
        // Wait for tasks to complete (they run until connection closes)
        let _ = tokio::try_join!(forward_task, send_recv_task);
        
        println!("🔄 Sync completed");
        Ok(())
    }

}

impl ProtocolHandler for IrohLoroProtocol {
    fn accept(&self, conn: iroh::endpoint::Connection) -> impl n0_future::Future<Output = Result<(), AcceptError>> + std::marker::Send {
        let this = self.clone();
        Box::pin(async move {
            println!("🔌 Peer connected");
            let result = this.initiate_sync(conn).await;
            println!("🔌 Peer disconnected");
            if let Err(e) = result {
                println!("❌ Error: {}", e);
                return Err(AcceptError::from_err(std::io::Error::new(std::io::ErrorKind::Other, e)));
            }
            Ok(())
        })
    }
}
