use anyhow::Result;
use bytes::Bytes;
use clap::{Parser, command};
use iroh::{node::Node, router::ProtocolHandler};
use std::{pin::Pin, sync::Arc};
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
enum Cli {
    NodeA {},
    NodeB { remote_id: iroh::net::NodeId },
}

const ALPN: &[u8] = b"biconn/1.0";

#[derive(Debug)]
struct BiConnProtocol {
    tx: mpsc::Sender<String>,
}

impl BiConnProtocol {
    fn new(tx: mpsc::Sender<String>) -> Self {
        Self { tx }
    }

    async fn write_message(send: &mut iroh::net::endpoint::SendStream, msg: &[u8]) -> Result<()> {
        let len = msg.len() as u32;
        send.write_all(&len.to_le_bytes()).await?;
        send.write_all(msg).await?;
        Ok(())
    }

    async fn read_message(recv: &mut iroh::net::endpoint::RecvStream) -> Result<Option<Vec<u8>>> {
        let mut len_bytes = [0u8; 4];
        println!("Reading length...");
        recv.read_exact(&mut len_bytes).await?;
        println!("Length read {:?}", &len_bytes);
        let len = u32::from_le_bytes(len_bytes) as usize;
        println!("Length: {}", len);

        let mut msg = vec![0u8; len];
        println!("Reading message...");
        recv.read_exact(&mut msg).await?;
        println!("Message read {:?}", &msg);
        Ok(Some(msg))
    }

    async fn handle_connection(
        self: Arc<Self>,
        conn: iroh::net::endpoint::Connection,
        is_initiator: bool,
    ) -> Result<()> {
        let (mut send, mut recv) = conn.open_bi().await?;

        if is_initiator {
            // Node A's message flow
            println!("Sending: 1");
            Self::write_message(&mut send, b"1").await?;
            Self::write_message(&mut send, &b"1".repeat(1000)).await?;

            println!("Receiving...");
            if let Some(msg) = Self::read_message(&mut recv).await? {
                println!("Received: {}", String::from_utf8_lossy(&msg));
            }

            println!("Sending: 1,1,1");
            Self::write_message(&mut send, b"1").await?;
            Self::write_message(&mut send, b"1").await?;
            Self::write_message(&mut send, b"1").await?;
        } else {
            // Node B's message flow
            println!("Receiving...");
            if let Some(msg) = Self::read_message(&mut recv).await? {
                println!("Received: {}", String::from_utf8_lossy(&msg));
            }

            println!("Sending: 1,1");
            Self::write_message(&mut send, b"1").await?;
            Self::write_message(&mut send, b"1").await?;

            println!("Receiving...");
            while let Some(msg) = Self::read_message(&mut recv).await? {
                println!("Received: {}", String::from_utf8_lossy(&msg));
            }
        }

        Ok(())
    }
}

impl ProtocolHandler for BiConnProtocol {
    fn accept(
        self: Arc<Self>,
        connecting: iroh::net::endpoint::Connecting,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        Box::pin(async move {
            let conn = connecting.await?;
            println!("🔌 Peer connected");
            let result = self.handle_connection(conn, false).await;
            println!("🔌 Peer disconnected");
            if let Err(e) = &result {
                println!("❌ Error: {}", e);
            }
            result
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Cli::parse();

    let (tx, mut rx) = mpsc::channel(100);
    let protocol = Arc::new(BiConnProtocol::new(tx));

    let node = Node::memory()
        .build()
        .await?
        .accept(
            ALPN.to_vec(),
            Arc::clone(&protocol) as Arc<dyn ProtocolHandler>,
        )
        .spawn()
        .await?;

    let addr = node.net().node_addr().await?;
    println!("Node ID: {}", addr.node_id);

    match opts {
        Cli::NodeA {} => {
            println!("Running as Node A");
            // Wait for Node B to connect
            tokio::signal::ctrl_c().await?;
        }
        Cli::NodeB { remote_id } => {
            println!("Running as Node B");
            let node_addr = iroh::net::NodeAddr::new(remote_id);
            let conn = node.endpoint().connect(node_addr, ALPN).await?;

            protocol.handle_connection(conn, true).await?;
        }
    }

    Ok(())
}
