use std::sync::Arc;
use std::collections::HashMap;

use anyhow::Result;
use iroh::protocol::{ProtocolHandler, AcceptError};
use loro::{ExportMode, LoroDoc};
use tokio::sync::{broadcast, RwLock};
use sha2::{Sha256, Digest};

#[derive(Clone, Debug)]
pub struct IrohLoroTextProtocol {
    doc: Arc<LoroDoc>,
    change_broadcaster: broadcast::Sender<Vec<u8>>, // Broadcast changes to all peers
    file_path: Option<String>, // Associated file path for writing back changes
    connected_peers: Arc<RwLock<HashMap<String, tokio::sync::mpsc::Sender<ConsistencyMessage>>>>, // Track connected peers for consistency checks
}

#[derive(Clone, Debug)]
pub enum ConsistencyMessage {
    StateHashRequest,
    StateHashResponse(String), // SHA256 hash of document state
    ResyncRequest,
}

impl IrohLoroTextProtocol {
    pub const ALPN: &'static [u8] = b"iroh/loro/1";

    pub fn new(doc: LoroDoc, change_broadcaster: broadcast::Sender<Vec<u8>>) -> Self {
        Self {
            doc: Arc::new(doc),
            change_broadcaster,
            file_path: None,
            connected_peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_file_path(mut self, file_path: String) -> Self {
        self.file_path = Some(file_path);
        self
    }

    /// Get a reference to the underlying LoroDoc for direct operations
    pub fn doc(&self) -> &Arc<LoroDoc> {
        &self.doc
    }

    /// Get the current document state as bytes for syncing
    pub fn export_snapshot(&self) -> Result<Vec<u8>> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// Get a hash of the current document state for consistency checking
    pub fn get_state_hash(&self) -> Result<String> {
        let snapshot = self.export_snapshot()?;
        let mut hasher = Sha256::new();
        hasher.update(&snapshot);
        Ok(format!("{:x}", hasher.finalize()))
    }

    /// Start periodic consistency checks with all connected peers
    pub async fn start_consistency_checker(&self) {
        let protocol = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                if let Err(e) = protocol.check_consistency().await {
                    println!("⚠️ Consistency check failed: {}", e);
                }
            }
        });
    }

    /// Check consistency with all connected peers
    async fn check_consistency(&self) -> Result<()> {
        let peers = self.connected_peers.read().await;
        if peers.is_empty() {
            return Ok(()); // No peers to check with
        }

        let our_hash = self.get_state_hash()?;
        println!("🔍 Checking consistency (our hash: {})...", &our_hash[..8]);

        // Request state hashes from all peers
        for (peer_id, sender) in peers.iter() {
            if let Err(e) = sender.send(ConsistencyMessage::StateHashRequest).await {
                println!("⚠️ Failed to send hash request to peer {}: {}", peer_id, e);
            }
        }

        Ok(())
    }

    /// Handle consistency messages from peers
    pub async fn handle_consistency_message(&self, peer_id: &str, message: ConsistencyMessage) -> Result<()> {
        match message {
            ConsistencyMessage::StateHashRequest => {
                let our_hash = self.get_state_hash()?;
                let peers = self.connected_peers.read().await;
                if let Some(sender) = peers.get(peer_id) {
                    let _ = sender.send(ConsistencyMessage::StateHashResponse(our_hash)).await;
                }
            }
            ConsistencyMessage::StateHashResponse(peer_hash) => {
                let our_hash = self.get_state_hash()?;
                if our_hash != peer_hash {
                    println!("❌ Inconsistency detected with peer {}! Our: {}, Theirs: {}", 
                             peer_id, &our_hash[..8], &peer_hash[..8]);
                    println!("🔄 Triggering resync...");
                    
                    // Request resync from this peer
                    let peers = self.connected_peers.read().await;
                    if let Some(sender) = peers.get(peer_id) {
                        let _ = sender.send(ConsistencyMessage::ResyncRequest).await;
                    }
                } else {
                    println!("✅ Consistent with peer {} (hash: {})", peer_id, &our_hash[..8]);
                }
            }
            ConsistencyMessage::ResyncRequest => {
                println!("🔄 Resync requested by peer {}", peer_id);
                // The resync will be handled by the existing sync mechanism
            }
        }
        Ok(())
    }

    /// Register a new peer connection for consistency tracking
    pub async fn register_peer(&self, peer_id: String, sender: tokio::sync::mpsc::Sender<ConsistencyMessage>) {
        let mut peers = self.connected_peers.write().await;
        peers.insert(peer_id.clone(), sender);
        println!("📝 Registered peer {} for consistency tracking", peer_id);
    }

    /// Unregister a peer connection
    pub async fn unregister_peer(&self, peer_id: &str) {
        let mut peers = self.connected_peers.write().await;
        peers.remove(peer_id);
        println!("📝 Unregistered peer {} from consistency tracking", peer_id);
    }

    /// Import changes from another peer and update associated file
    pub async fn import_changes(&self, data: &[u8]) -> Result<()> {
        self.doc.import(data)?;
        
        // After importing changes, write updated content back to file if we have one
        if let Some(file_path) = &self.file_path {
            let text = self.doc.get_text("text");
            let content = text.to_string();
            if let Err(e) = tokio::fs::write(file_path, content).await {
                println!("⚠️ Failed to write updated content to file {}: {}", file_path, e);
            } else {
                println!("📝 Updated file {} with synced content", file_path);
            }
        }
        
        Ok(())
    }

    /// Get all updates for syncing
    pub fn export_updates(&self) -> Result<Vec<u8>> {
        Ok(self.doc.export(ExportMode::all_updates())?)
    }

    /// Get recent updates since last export for ongoing sync
    pub fn export_recent_updates(&self) -> Result<Vec<u8>> {
        // For now, use all_updates - we'll optimize this later for incremental sync
        Ok(self.doc.export(ExportMode::all_updates())?)
    }

    /// Commit any pending changes and notify peers
    pub fn commit_and_sync(&self) -> Result<()> {
        // First commit the changes
        self.doc.commit();
        
        // Export only the recent changes for broadcasting
        let changes = self.export_recent_updates()?;
        if !changes.is_empty() {
            println!("📤 Broadcasting {} bytes of changes", changes.len());
            let _ = self.change_broadcaster.send(changes);
        } else {
            println!("📤 No changes to broadcast");
        }
        
        Ok(())
    }


    pub async fn initiate_sync(&self, conn: iroh::endpoint::Connection) -> Result<()> {
        println!("🔄 Starting sync with peer (as client)");
        
        let (mut send_stream, mut recv_stream) = conn.open_bi().await?;
        
        // CLIENT: Send our snapshot first
        let export_data = self.export_snapshot()?;
        let data_len = export_data.len() as u32;
        send_stream.write_all(&data_len.to_le_bytes()).await?;
        send_stream.write_all(&export_data).await?;
        
        println!("📤 Client sent document snapshot ({} bytes)", export_data.len());
        
        // CLIENT: Then receive host's snapshot
        let mut buffer = [0u8; 4];
        recv_stream.read_exact(&mut buffer).await?;
        let msg_len = u32::from_le_bytes(buffer) as usize;
        let mut msg_buffer = vec![0u8; msg_len];
        recv_stream.read_exact(&mut msg_buffer).await?;
        
        println!("📥 Client received host snapshot ({} bytes)", msg_buffer.len());
        println!("🔍 Host snapshot data: {:?}", String::from_utf8_lossy(&msg_buffer[..std::cmp::min(100, msg_buffer.len())]));
        
        if let Err(e) = self.import_changes(&msg_buffer).await {
            println!("⚠️ Failed to import host snapshot: {}", e);
        } else {
            println!("✅ Successfully imported host snapshot");
            let doc = self.doc();
            let text = doc.get_text("text");
            let content = text.to_string();
            println!("📄 Document content after import: '{}'", content);
        }
        
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
                    
                    // Receive changes from peer (including initial snapshot)
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
                                
                                // Debug: Show what we're importing
                                println!("🔍 Importing data: {:?}", String::from_utf8_lossy(&msg_buffer[..std::cmp::min(100, msg_buffer.len())]));
                                
                                if let Err(e) = protocol_for_import.import_changes(&msg_buffer).await {
                                    println!("⚠️ Failed to import changes: {}", e);
                                } else {
                                    println!("✅ Successfully imported and wrote to file");
                                    
                                    // Debug: Show current document state after import
                                    let doc = protocol_for_import.doc();
                                    let text = doc.get_text("text");
                                    let content = text.to_string();
                                    println!("📄 Document content after import: '{}'", content);
                                }
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

    pub async fn accept_sync(&self, conn: iroh::endpoint::Connection) -> Result<()> {
        println!("🔄 Starting sync with peer (as host)");
        
        let (mut send_stream, mut recv_stream) = conn.accept_bi().await?;
        
        // HOST: Receive client's snapshot first
        let mut buffer = [0u8; 4];
        recv_stream.read_exact(&mut buffer).await?;
        let msg_len = u32::from_le_bytes(buffer) as usize;
        let mut msg_buffer = vec![0u8; msg_len];
        recv_stream.read_exact(&mut msg_buffer).await?;
        
        println!("📥 Host received client snapshot ({} bytes)", msg_buffer.len());
        println!("🔍 Client snapshot data: {:?}", String::from_utf8_lossy(&msg_buffer[..std::cmp::min(100, msg_buffer.len())]));
        
        if let Err(e) = self.import_changes(&msg_buffer).await {
            println!("⚠️ Failed to import client snapshot: {}", e);
        } else {
            println!("✅ Successfully imported client snapshot");
            let doc = self.doc();
            let text = doc.get_text("text");
            let content = text.to_string();
            println!("📄 Document content after import: '{}'", content);
        }
        
        // HOST: Then send our snapshot
        let export_data = self.export_snapshot()?;
        let data_len = export_data.len() as u32;
        send_stream.write_all(&data_len.to_le_bytes()).await?;
        send_stream.write_all(&export_data).await?;
        
        println!("📤 Host sent document snapshot ({} bytes)", export_data.len());
        
        // Continue with ongoing sync...
        let mut change_receiver = self.change_broadcaster.subscribe();
        let (change_tx, mut change_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(100);
        
        let protocol_for_import = self.clone();
        
        // Spawn task to forward local changes to this peer
        let forward_task = tokio::spawn(async move {
            while let Ok(changes) = change_receiver.recv().await {
                if change_tx.send(changes).await.is_err() {
                    break;
                }
            }
        });
        
        // Spawn task to receive and import changes from this peer
        let import_task = tokio::spawn(async move {
            loop {
                let mut buffer = [0u8; 4];
                match recv_stream.read_exact(&mut buffer).await {
                    Ok(_) => {
                        let msg_len = u32::from_le_bytes(buffer) as usize;
                        let mut msg_buffer = vec![0u8; msg_len];
                        match recv_stream.read_exact(&mut msg_buffer).await {
                            Ok(_) => {
                                println!("📥 Received changes ({} bytes)", msg_buffer.len());
                                if let Err(e) = protocol_for_import.import_changes(&msg_buffer).await {
                                    println!("⚠️ Failed to import changes: {}", e);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        
        // Forward changes to peer
        while let Some(changes) = change_rx.recv().await {
            let data_len = changes.len() as u32;
            if send_stream.write_all(&data_len.to_le_bytes()).await.is_err() {
                break;
            }
            if send_stream.write_all(&changes).await.is_err() {
                break;
            }
        }
        
        forward_task.abort();
        import_task.abort();
        
        Ok(())
    }
}

impl ProtocolHandler for IrohLoroTextProtocol {
    fn accept(&self, conn: iroh::endpoint::Connection) -> impl n0_future::Future<Output = Result<(), AcceptError>> + std::marker::Send {
        let this = self.clone();
        Box::pin(async move {
            println!("🔌 Peer connected");
            let result = this.accept_sync(conn).await;
            println!("🔌 Peer disconnected");
            if let Err(e) = result {
                println!("❌ Error: {}", e);
                return Err(AcceptError::from_err(std::io::Error::new(std::io::ErrorKind::Other, e)));
            }
            Ok(())
        })
    }
}
