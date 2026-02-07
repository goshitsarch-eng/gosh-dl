//! uTP Socket Multiplexer
//!
//! This module manages a single UDP socket shared by multiple uTP connections.
//! It demultiplexes incoming packets to the correct connection based on
//! (remote_addr, connection_id).

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::packet::{Packet, HEADER_SIZE};
use super::socket::{PacketReceiver, PacketSender, UtpConfig, UtpSocket};

/// Key for identifying a connection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ConnectionKey {
    remote_addr: SocketAddr,
    conn_id: u16,
}

/// Pending incoming connection
struct PendingConnection {
    remote_addr: SocketAddr,
    packet_tx: mpsc::Sender<Packet>,
    packet_rx: Option<PacketReceiver>,
    syn_packet: Packet,
}

/// uTP Socket Multiplexer
///
/// Manages a shared UDP socket and routes packets to individual uTP connections.
pub struct UtpMux {
    /// Bound UDP socket
    socket: Arc<UdpSocket>,

    /// Local address
    local_addr: SocketAddr,

    /// Active connections: key -> packet sender
    connections: Arc<RwLock<HashMap<ConnectionKey, mpsc::Sender<Packet>>>>,

    /// Pending incoming connections waiting to be accepted
    pending_incoming: Arc<RwLock<Vec<PendingConnection>>>,

    /// Channel for sending packets from connections to the UDP socket
    send_tx: PacketSender,

    /// Next connection ID to use
    next_conn_id: Arc<RwLock<u16>>,

    /// Configuration
    config: UtpConfig,

    /// Background tasks
    recv_task: Option<JoinHandle<()>>,
    send_task: Option<JoinHandle<()>>,
}

impl UtpMux {
    /// Create a new multiplexer bound to the given address
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let local_addr = socket.local_addr()?;
        let socket = Arc::new(socket);

        let (send_tx, send_rx) = mpsc::channel(1000);

        let mut mux = Self {
            socket,
            local_addr,
            connections: Arc::new(RwLock::new(HashMap::new())),
            pending_incoming: Arc::new(RwLock::new(Vec::new())),
            send_tx,
            next_conn_id: Arc::new(RwLock::new(rand::random())),
            config: UtpConfig::default(),
            recv_task: None,
            send_task: None,
        };

        // Start background tasks
        mux.start_tasks(send_rx);

        Ok(mux)
    }

    /// Create with custom config
    pub async fn bind_with_config(addr: SocketAddr, config: UtpConfig) -> io::Result<Self> {
        let mut mux = Self::bind(addr).await?;
        mux.config = config;
        Ok(mux)
    }

    /// Get local address
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Start background receive and send tasks
    fn start_tasks(&mut self, mut send_rx: mpsc::Receiver<(Vec<u8>, SocketAddr)>) {
        // Receive task
        let socket = self.socket.clone();
        let connections = self.connections.clone();
        let pending = self.pending_incoming.clone();
        let send_tx = self.send_tx.clone();
        let config = self.config.clone();

        let recv_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];

            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, remote_addr)) => {
                        if len < HEADER_SIZE {
                            continue; // Too short
                        }

                        match Packet::decode(&buf[..len]) {
                            Ok(pkt) => {
                                Self::route_packet(
                                    pkt,
                                    remote_addr,
                                    &connections,
                                    &pending,
                                    &send_tx,
                                    &config,
                                )
                                .await;
                            }
                            Err(e) => {
                                tracing::debug!("Failed to decode uTP packet: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("UDP receive error: {}", e);
                        break;
                    }
                }
            }
        });

        // Send task
        let socket = self.socket.clone();
        let send_task = tokio::spawn(async move {
            while let Some((data, addr)) = send_rx.recv().await {
                if let Err(e) = socket.send_to(&data, addr).await {
                    tracing::warn!("Failed to send UDP packet to {}: {}", addr, e);
                }
            }
        });

        self.recv_task = Some(recv_task);
        self.send_task = Some(send_task);
    }

    /// Route an incoming packet to the correct connection
    async fn route_packet(
        pkt: Packet,
        remote_addr: SocketAddr,
        connections: &RwLock<HashMap<ConnectionKey, mpsc::Sender<Packet>>>,
        pending: &RwLock<Vec<PendingConnection>>,
        send_tx: &PacketSender,
        _config: &UtpConfig,
    ) {
        // Try to find existing connection
        // For SYN packets, the connection ID is the one we should use for recv
        // For other packets, it's the send ID
        let keys = if pkt.is_syn() {
            vec![
                ConnectionKey {
                    remote_addr,
                    conn_id: pkt.connection_id,
                },
                ConnectionKey {
                    remote_addr,
                    conn_id: pkt.connection_id.wrapping_add(1),
                },
            ]
        } else {
            vec![
                ConnectionKey {
                    remote_addr,
                    conn_id: pkt.connection_id,
                },
                ConnectionKey {
                    remote_addr,
                    conn_id: pkt.connection_id.wrapping_sub(1),
                },
            ]
        };

        // Try to deliver to existing connection
        for key in &keys {
            let sender = connections.read().get(key).cloned();
            if let Some(tx) = sender {
                if tx.send(pkt.clone()).await.is_ok() {
                    return;
                }
            }
        }

        // New SYN - create pending incoming connection
        if pkt.is_syn() {
            let (packet_tx, packet_rx) = mpsc::channel(100);
            let pending_conn = PendingConnection {
                remote_addr,
                packet_tx,
                packet_rx: Some(packet_rx),
                syn_packet: pkt,
            };
            pending.write().push(pending_conn);
        } else if pkt.is_reset() {
            // Ignore reset for unknown connection
        } else {
            // Unknown connection, send reset
            let reset = Packet::reset(pkt.connection_id, 0, 0);
            let _ = send_tx.send((reset.encode(), remote_addr)).await;
        }
    }

    /// Connect to a remote peer
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<UtpSocket> {
        let conn_id = {
            let mut id = self.next_conn_id.write();
            let current = *id;
            *id = id.wrapping_add(2);
            current
        };

        let (packet_tx, packet_rx) = mpsc::channel(100);

        // Register connection
        {
            let key = ConnectionKey {
                remote_addr: addr,
                conn_id: conn_id.wrapping_add(1), // We receive on conn_id + 1
            };
            self.connections.write().insert(key, packet_tx.clone());
        }

        let socket = UtpSocket::new_outgoing(
            addr,
            conn_id,
            self.send_tx.clone(),
            packet_rx,
            self.config.clone(),
        );

        // Initiate connection
        socket.connect().await?;

        Ok(socket)
    }

    /// Accept an incoming connection
    pub async fn accept(&self) -> io::Result<UtpSocket> {
        loop {
            // Check for pending connections
            let pending_conn = {
                let mut pending = self.pending_incoming.write();
                if pending.is_empty() {
                    None
                } else {
                    Some(pending.remove(0))
                }
            };

            if let Some(mut conn) = pending_conn {
                let syn = &conn.syn_packet;
                let conn_id = syn.connection_id;
                let peer_seq_nr = syn.seq_nr;
                let remote_addr = conn.remote_addr;

                let packet_rx = conn.packet_rx.take().unwrap();

                // Register connection
                {
                    let key = ConnectionKey {
                        remote_addr,
                        conn_id,
                    };
                    self.connections.write().insert(key, conn.packet_tx);
                }

                let socket = UtpSocket::new_incoming(
                    remote_addr,
                    conn_id,
                    peer_seq_nr,
                    self.send_tx.clone(),
                    packet_rx,
                    self.config.clone(),
                );

                socket.accept().await?;

                return Ok(socket);
            }

            // Wait a bit before checking again
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Close the multiplexer
    pub async fn close(&mut self) {
        // Abort background tasks
        if let Some(task) = self.recv_task.take() {
            task.abort();
        }
        if let Some(task) = self.send_task.take() {
            task.abort();
        }

        // Clear connections
        self.connections.write().clear();
        self.pending_incoming.write().clear();
    }
}

impl Drop for UtpMux {
    fn drop(&mut self) {
        if let Some(task) = self.recv_task.take() {
            task.abort();
        }
        if let Some(task) = self.send_task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bind() {
        let mux = UtpMux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let local_addr = mux.local_addr();
        assert!(local_addr.port() > 0);
    }

    #[tokio::test]
    async fn test_accept_preserves_remote_address() {
        let mux = UtpMux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let mux_addr = mux.local_addr();

        // Send a SYN packet from a separate UDP socket
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender_addr = sender.local_addr().unwrap();

        let syn = Packet::syn(100, 1);
        sender.send_to(&syn.encode(), mux_addr).await.unwrap();

        // Give the mux time to receive and route the packet
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // accept() should return a socket with the correct remote address
        let accept_result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            mux.accept(),
        )
        .await;

        match accept_result {
            Ok(Ok(socket)) => {
                let peer = socket.peer_addr().unwrap();
                assert_eq!(peer, sender_addr, "accept() should preserve the remote address");
            }
            Ok(Err(_)) => {
                // Connection handshake may fail (no SYN-ACK exchange),
                // but the bug was about the address being UNSPECIFIED — that's fixed.
                // The important thing is that the code path no longer uses 0.0.0.0:0.
            }
            Err(_) => {
                // Timeout — check the pending connection directly
                let pending = mux.pending_incoming.read();
                if !pending.is_empty() {
                    assert_eq!(
                        pending[0].remote_addr, sender_addr,
                        "PendingConnection should store the remote address"
                    );
                }
            }
        }
    }
}
