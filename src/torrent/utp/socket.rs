//! uTP Socket Implementation
//!
//! This module implements a single uTP connection with reliable,
//! ordered delivery over UDP.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;

use super::congestion::LedbatController;
use super::packet::{Packet, PacketType, SelectiveAck, MAX_PAYLOAD_SIZE, timestamp_us};
use super::state::{ConnectionState, ConnectionStats, PendingPacket};

/// Maximum number of retransmissions before giving up
const MAX_RETRANSMITS: u32 = 10;

/// Receive buffer size
const RECV_BUFFER_SIZE: usize = 1024 * 1024; // 1MB

/// Maximum out-of-order packets to buffer
const MAX_OOO_PACKETS: usize = 256;

/// uTP socket configuration
#[derive(Debug, Clone)]
pub struct UtpConfig {
    /// Enable selective ACK extension
    pub enable_sack: bool,

    /// Maximum window size (bytes)
    pub max_window_size: u32,

    /// Initial receive window (bytes)
    pub recv_window: u32,
}

impl Default for UtpConfig {
    fn default() -> Self {
        Self {
            enable_sack: true,
            max_window_size: 1024 * 1024,
            recv_window: 1024 * 1024,
        }
    }
}

/// Channel for sending packets to the multiplexer
pub type PacketSender = mpsc::Sender<(Vec<u8>, SocketAddr)>;

/// Channel for receiving packets from the multiplexer
pub type PacketReceiver = mpsc::Receiver<Packet>;

/// Internal state for a uTP socket
pub struct UtpSocketInner {
    /// Remote peer address
    pub remote_addr: SocketAddr,

    /// Connection ID for sending (remote expects this)
    pub send_conn_id: u16,

    /// Connection ID for receiving (we expect this)
    pub recv_conn_id: u16,

    /// Current connection state
    pub state: ConnectionState,

    /// Our sequence number (next to send)
    pub seq_nr: u16,

    /// Their sequence number (next expected)
    pub ack_nr: u16,

    /// Last ACK we sent
    pub last_ack_sent: u16,

    /// Congestion controller
    pub congestion: LedbatController,

    /// Packets awaiting acknowledgment
    pub pending_packets: BTreeMap<u16, PendingPacket>,

    /// Out-of-order received packets
    pub ooo_packets: BTreeMap<u16, Vec<u8>>,

    /// Receive buffer (ordered data ready for reading)
    pub recv_buffer: VecDeque<u8>,

    /// Receive window size we advertise
    pub recv_window: u32,

    /// Remote's advertised window size
    pub remote_window: u32,

    /// Time of last received packet
    pub last_recv_time: Instant,

    /// Time of last sent packet
    pub last_send_time: Instant,

    /// Connection statistics
    pub stats: ConnectionStats,

    /// Configuration
    pub config: UtpConfig,

    /// Channel to send packets
    pub packet_tx: PacketSender,

    /// FIN received from peer
    pub fin_received: bool,

    /// FIN sent to peer
    pub fin_sent: bool,
}

impl UtpSocketInner {
    /// Create a new socket for an outgoing connection
    pub fn new_outgoing(
        remote_addr: SocketAddr,
        conn_id: u16,
        packet_tx: PacketSender,
        config: UtpConfig,
    ) -> Self {
        let now = Instant::now();
        Self {
            remote_addr,
            send_conn_id: conn_id,
            recv_conn_id: conn_id.wrapping_add(1),
            state: ConnectionState::Idle,
            seq_nr: 1,
            ack_nr: 0,
            last_ack_sent: 0,
            congestion: LedbatController::new(),
            pending_packets: BTreeMap::new(),
            ooo_packets: BTreeMap::new(),
            recv_buffer: VecDeque::with_capacity(RECV_BUFFER_SIZE),
            recv_window: config.recv_window,
            remote_window: 0,
            last_recv_time: now,
            last_send_time: now,
            stats: ConnectionStats::new(),
            config,
            packet_tx,
            fin_received: false,
            fin_sent: false,
        }
    }

    /// Create a new socket for an incoming connection
    pub fn new_incoming(
        remote_addr: SocketAddr,
        conn_id: u16,
        peer_seq_nr: u16,
        packet_tx: PacketSender,
        config: UtpConfig,
    ) -> Self {
        let now = Instant::now();
        Self {
            remote_addr,
            send_conn_id: conn_id,
            recv_conn_id: conn_id.wrapping_add(1),
            state: ConnectionState::SynRecv,
            seq_nr: 1,
            ack_nr: peer_seq_nr,
            last_ack_sent: 0,
            congestion: LedbatController::new(),
            pending_packets: BTreeMap::new(),
            ooo_packets: BTreeMap::new(),
            recv_buffer: VecDeque::with_capacity(RECV_BUFFER_SIZE),
            recv_window: config.recv_window,
            remote_window: 0,
            last_recv_time: now,
            last_send_time: now,
            stats: ConnectionStats::new(),
            config,
            packet_tx,
            fin_received: false,
            fin_sent: false,
        }
    }

    /// Start the connection (send SYN)
    pub async fn connect(&mut self) -> io::Result<()> {
        if self.state != ConnectionState::Idle {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Connection already started",
            ));
        }

        self.state = ConnectionState::SynSent;
        let pkt = self.build_packet(PacketType::Syn, Vec::new());
        self.send_packet(pkt).await
    }

    /// Accept an incoming connection (send SYN-ACK)
    pub async fn accept(&mut self) -> io::Result<()> {
        if self.state != ConnectionState::SynRecv {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "No incoming connection to accept",
            ));
        }

        let pkt = self.build_packet(PacketType::State, Vec::new());
        self.send_packet(pkt).await?;
        self.state = ConnectionState::Connected;
        Ok(())
    }

    /// Process a received packet
    pub async fn process_packet(&mut self, pkt: Packet) -> io::Result<()> {
        self.last_recv_time = Instant::now();
        self.remote_window = pkt.wnd_size;
        self.stats.packets_received += 1;

        // Update congestion control with delay sample
        if pkt.timestamp_diff_us > 0 {
            // Calculate RTT if we can correlate
            let _rtt_sample = if !self.pending_packets.is_empty() {
                Some(pkt.timestamp_diff_us)
            } else {
                None
            };

            // Any packet with timestamp_diff gives us delay info
            // (This is their measurement of our delay)
        }

        match self.state {
            ConnectionState::SynSent => {
                // Expecting SYN-ACK (STATE packet with our seq_nr acked)
                if pkt.is_state() {
                    self.ack_nr = pkt.seq_nr;
                    self.process_acks(pkt.ack_nr, pkt.selective_ack.as_ref());
                    self.state = ConnectionState::Connected;
                } else if pkt.is_reset() {
                    self.state = ConnectionState::Reset;
                }
            }

            ConnectionState::Connected | ConnectionState::FinSent => {
                if pkt.is_reset() {
                    self.state = ConnectionState::Reset;
                    return Ok(());
                }

                if pkt.is_fin() {
                    self.fin_received = true;
                    self.ack_nr = pkt.seq_nr;
                    // Send ACK for FIN
                    let ack = self.build_packet(PacketType::State, Vec::new());
                    self.send_packet_direct(ack).await?;

                    if self.fin_sent {
                        self.state = ConnectionState::Closed;
                    } else {
                        self.state = ConnectionState::Closing;
                    }
                }

                // Process ACKs
                self.process_acks(pkt.ack_nr, pkt.selective_ack.as_ref());

                // Process data
                if pkt.is_data() && !pkt.payload.is_empty() {
                    self.receive_data(pkt.seq_nr, pkt.payload)?;
                }
            }

            ConnectionState::Closing => {
                if pkt.is_state() {
                    self.process_acks(pkt.ack_nr, pkt.selective_ack.as_ref());
                    if self.pending_packets.is_empty() {
                        self.state = ConnectionState::Closed;
                    }
                }
            }

            _ => {}
        }

        Ok(())
    }

    /// Process acknowledgments
    fn process_acks(&mut self, ack_nr: u16, sack: Option<&SelectiveAck>) {
        // Remove all packets up to and including ack_nr
        let to_remove: Vec<u16> = self.pending_packets
            .keys()
            .copied()
            .filter(|&seq| self.seq_before_eq(seq, ack_nr))
            .collect();

        for seq in to_remove {
            if let Some(pkt) = self.pending_packets.remove(&seq) {
                let rtt = pkt.first_sent.elapsed().as_micros() as u32;
                self.congestion.on_ack(pkt.size, 0, Some(rtt));
            }
        }

        // Process selective ACKs
        if let Some(sack) = sack {
            // SACK bitmap starts at ack_nr + 2
            for i in 0..sack.bitmask.len() * 8 {
                if sack.is_acked(i as u16) {
                    let seq = ack_nr.wrapping_add(2).wrapping_add(i as u16);
                    if let Some(pkt) = self.pending_packets.remove(&seq) {
                        self.congestion.on_ack(pkt.size, 0, None);
                    }
                }
            }
        }
    }

    /// Receive data into buffer, handling out-of-order
    fn receive_data(&mut self, seq_nr: u16, payload: Vec<u8>) -> io::Result<()> {
        let expected = self.ack_nr.wrapping_add(1);

        if seq_nr == expected {
            // In-order packet
            self.recv_buffer.extend(&payload);
            self.ack_nr = seq_nr;
            self.stats.bytes_received += payload.len() as u64;

            // Deliver any buffered out-of-order packets
            loop {
                let next = self.ack_nr.wrapping_add(1);
                if let Some(data) = self.ooo_packets.remove(&next) {
                    self.recv_buffer.extend(&data);
                    self.ack_nr = next;
                    self.stats.bytes_received += data.len() as u64;
                } else {
                    break;
                }
            }
        } else if self.seq_after(seq_nr, expected) && self.ooo_packets.len() < MAX_OOO_PACKETS {
            // Out-of-order packet - buffer it
            self.ooo_packets.insert(seq_nr, payload);
        }
        // Else: duplicate or too old, ignore

        Ok(())
    }

    /// Send data (returns amount actually queued)
    pub async fn send_data(&mut self, data: &[u8]) -> io::Result<usize> {
        if !self.state.can_send_data() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Cannot send in current state",
            ));
        }

        let mut sent = 0;

        // Check if we can send based on congestion window
        while sent < data.len() && self.congestion.can_send() {
            let end = (sent + MAX_PAYLOAD_SIZE).min(data.len());
            let chunk = data[sent..end].to_vec();

            let pkt = self.build_packet(PacketType::Data, chunk);
            self.send_packet(pkt).await?;
            sent = end;
        }

        Ok(sent)
    }

    /// Read data from receive buffer
    pub fn read_data(&mut self, buf: &mut [u8]) -> usize {
        let len = buf.len().min(self.recv_buffer.len());
        for (i, byte) in self.recv_buffer.drain(..len).enumerate() {
            buf[i] = byte;
        }
        len
    }

    /// Send a FIN to close the connection
    pub async fn close(&mut self) -> io::Result<()> {
        if self.state == ConnectionState::Connected {
            self.fin_sent = true;
            let pkt = self.build_packet(PacketType::Fin, Vec::new());
            self.send_packet(pkt).await?;
            self.state = ConnectionState::FinSent;
        }
        Ok(())
    }

    /// Build a packet with current state
    fn build_packet(&mut self, pkt_type: PacketType, payload: Vec<u8>) -> Packet {
        let conn_id = if pkt_type == PacketType::Syn {
            self.recv_conn_id
        } else {
            self.send_conn_id
        };

        let seq_nr = self.seq_nr;
        if pkt_type == PacketType::Data || pkt_type == PacketType::Syn || pkt_type == PacketType::Fin {
            self.seq_nr = self.seq_nr.wrapping_add(1);
        }

        let mut pkt = Packet::new(pkt_type, conn_id, seq_nr, self.ack_nr)
            .with_timestamps(timestamp_us(), 0)
            .with_window(self.available_recv_window());

        // Add selective ACK if we have out-of-order packets
        if self.config.enable_sack && !self.ooo_packets.is_empty() && pkt_type == PacketType::State {
            let mut sack = SelectiveAck::default();
            for &seq in self.ooo_packets.keys() {
                let offset = seq.wrapping_sub(self.ack_nr).wrapping_sub(2);
                if offset < 256 {
                    sack.set_acked(offset);
                }
            }
            pkt = pkt.with_selective_ack(sack);
        }

        pkt.payload = payload;
        pkt
    }

    /// Send a packet and track it for retransmission
    async fn send_packet(&mut self, pkt: Packet) -> io::Result<()> {
        let data = pkt.encode();
        let payload = pkt.payload.clone();
        let seq_nr = pkt.seq_nr;
        let is_data = pkt.is_data() || pkt.is_syn() || pkt.is_fin();

        self.send_packet_direct(pkt).await?;

        // Track for retransmission if it's a data-bearing packet
        if is_data {
            let pending = PendingPacket::new(seq_nr, data.clone(), payload);
            self.congestion.on_send(pending.size);
            self.pending_packets.insert(seq_nr, pending);
        }

        Ok(())
    }

    /// Send a packet without tracking
    async fn send_packet_direct(&mut self, pkt: Packet) -> io::Result<()> {
        let data = pkt.encode();

        self.packet_tx
            .send((data, self.remote_addr))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::ConnectionReset, "Send channel closed"))?;

        self.stats.packets_sent += 1;
        if !pkt.payload.is_empty() {
            self.stats.bytes_sent += pkt.payload.len() as u64;
        }
        self.last_send_time = Instant::now();
        self.last_ack_sent = self.ack_nr;

        Ok(())
    }

    /// Check and perform retransmissions
    pub async fn check_retransmits(&mut self) -> io::Result<()> {
        let rto = self.congestion.rto();
        let now = Instant::now();

        let to_retransmit: Vec<u16> = self.pending_packets
            .iter()
            .filter(|(_, p)| now.duration_since(p.last_sent) > rto)
            .map(|(seq, _)| *seq)
            .collect();

        for seq in to_retransmit {
            // Extract data we need from pending packet
            let (pkt_seq_nr, payload, _retransmits, max_exceeded) = {
                let pending = match self.pending_packets.get(&seq) {
                    Some(p) => p,
                    None => continue,
                };
                (pending.seq_nr, pending.payload.clone(), pending.retransmits, pending.retransmits >= MAX_RETRANSMITS)
            };

            if max_exceeded {
                self.state = ConnectionState::TimedOut;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Max retransmits exceeded",
                ));
            }

            // Rebuild packet with current timestamps
            let pkt_type = if pkt_seq_nr == 1 && self.state == ConnectionState::SynSent {
                PacketType::Syn
            } else {
                PacketType::Data
            };

            let recv_window = self.available_recv_window();
            let mut pkt = Packet::new(pkt_type, self.send_conn_id, pkt_seq_nr, self.ack_nr)
                .with_timestamps(timestamp_us(), 0)
                .with_window(recv_window);

            pkt.payload = payload;

            self.send_packet_direct(pkt).await?;

            // Now update the pending packet
            if let Some(pending) = self.pending_packets.get_mut(&seq) {
                pending.mark_retransmit();
            }
            self.stats.retransmits += 1;
            self.congestion.on_loss();
        }

        Ok(())
    }

    /// Send an ACK if needed
    pub async fn maybe_send_ack(&mut self) -> io::Result<()> {
        if self.ack_nr != self.last_ack_sent {
            let pkt = self.build_packet(PacketType::State, Vec::new());
            self.send_packet_direct(pkt).await?;
        }
        Ok(())
    }

    /// Calculate available receive window
    fn available_recv_window(&self) -> u32 {
        let used = self.recv_buffer.len() as u32;
        self.recv_window.saturating_sub(used)
    }

    /// Check if seq_a comes before seq_b (handles wrapping)
    fn seq_before_eq(&self, seq_a: u16, seq_b: u16) -> bool {
        let diff = seq_b.wrapping_sub(seq_a);
        diff == 0 || diff < 32768
    }

    /// Check if seq_a comes after seq_b (handles wrapping)
    fn seq_after(&self, seq_a: u16, seq_b: u16) -> bool {
        let diff = seq_a.wrapping_sub(seq_b);
        diff > 0 && diff < 32768
    }

    /// Get connection state
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Get statistics
    pub fn stats(&self) -> &ConnectionStats {
        &self.stats
    }

    /// Check if there's data available to read
    pub fn has_data(&self) -> bool {
        !self.recv_buffer.is_empty()
    }

    /// Get amount of data available to read
    pub fn available_data(&self) -> usize {
        self.recv_buffer.len()
    }
}

/// High-level uTP socket wrapper
pub struct UtpSocket {
    inner: Arc<Mutex<UtpSocketInner>>,
    packet_rx: Mutex<PacketReceiver>,
}

impl UtpSocket {
    /// Create a new outgoing socket
    pub fn new_outgoing(
        remote_addr: SocketAddr,
        conn_id: u16,
        packet_tx: PacketSender,
        packet_rx: PacketReceiver,
        config: UtpConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(UtpSocketInner::new_outgoing(
                remote_addr,
                conn_id,
                packet_tx,
                config,
            ))),
            packet_rx: Mutex::new(packet_rx),
        }
    }

    /// Create a new incoming socket
    pub fn new_incoming(
        remote_addr: SocketAddr,
        conn_id: u16,
        peer_seq_nr: u16,
        packet_tx: PacketSender,
        packet_rx: PacketReceiver,
        config: UtpConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(UtpSocketInner::new_incoming(
                remote_addr,
                conn_id,
                peer_seq_nr,
                packet_tx,
                config,
            ))),
            packet_rx: Mutex::new(packet_rx),
        }
    }

    /// Connect to remote peer
    pub async fn connect(&self) -> io::Result<()> {
        {
            let mut inner = self.inner.lock().await;
            inner.connect().await?;
        }

        // Wait for SYN-ACK with timeout
        let result = timeout(Duration::from_secs(30), self.wait_connected()).await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                let mut inner = self.inner.lock().await;
                inner.state = ConnectionState::TimedOut;
                Err(io::Error::new(io::ErrorKind::TimedOut, "Connection timeout"))
            }
        }
    }

    /// Wait for connection to be established
    async fn wait_connected(&self) -> io::Result<()> {
        loop {
            // Process incoming packets
            let pkt = {
                let mut rx = self.packet_rx.lock().await;
                rx.recv().await
            };

            if let Some(pkt) = pkt {
                let mut inner = self.inner.lock().await;
                inner.process_packet(pkt).await?;

                if inner.state == ConnectionState::Connected {
                    return Ok(());
                }
                if inner.state.is_closed() {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "Connection failed",
                    ));
                }
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "Packet channel closed",
                ));
            }
        }
    }

    /// Accept an incoming connection
    pub async fn accept(&self) -> io::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.accept().await
    }

    /// Read data from the socket
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // Check if we have data
            {
                let mut inner = self.inner.lock().await;
                if inner.has_data() {
                    return Ok(inner.read_data(buf));
                }
                if inner.fin_received && inner.recv_buffer.is_empty() {
                    return Ok(0); // EOF
                }
                if inner.state.is_closed() {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        format!("Connection closed: {}", inner.state),
                    ));
                }
            }

            // Wait for more data
            let pkt = {
                let mut rx = self.packet_rx.lock().await;
                rx.recv().await
            };

            if let Some(pkt) = pkt {
                let mut inner = self.inner.lock().await;
                inner.process_packet(pkt).await?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "Connection lost",
                ));
            }
        }
    }

    /// Read exactly len bytes
    pub async fn read_exact(&self, buf: &mut [u8]) -> io::Result<()> {
        let mut total = 0;
        while total < buf.len() {
            let n = self.read(&mut buf[total..]).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Unexpected EOF",
                ));
            }
            total += n;
        }
        Ok(())
    }

    /// Write data to the socket
    pub async fn write_all(&self, data: &[u8]) -> io::Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            let sent = {
                let mut inner = self.inner.lock().await;
                inner.send_data(&data[offset..]).await?
            };
            if sent == 0 {
                // Congestion window full, wait a bit
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            offset += sent;
        }
        Ok(())
    }

    /// Flush (no-op for uTP, data is sent immediately)
    pub async fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    /// Shutdown the socket
    pub async fn shutdown(&self) -> io::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.close().await
    }

    /// Get peer address
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        // This is sync-safe since remote_addr doesn't change
        Ok(self.inner.try_lock().map(|i| i.remote_addr).unwrap_or_else(|_| {
            // Fallback - shouldn't happen in practice
            "0.0.0.0:0".parse().unwrap()
        }))
    }

    /// Get connection state
    pub async fn state(&self) -> ConnectionState {
        self.inner.lock().await.state
    }

    /// Get inner for direct access (used by multiplexer)
    pub fn inner(&self) -> Arc<Mutex<UtpSocketInner>> {
        self.inner.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seq_comparison() {
        let inner = UtpSocketInner::new_outgoing(
            "127.0.0.1:8080".parse().unwrap(),
            1000,
            mpsc::channel(1).0,
            UtpConfig::default(),
        );

        // Normal case
        assert!(inner.seq_before_eq(10, 20));
        assert!(inner.seq_before_eq(10, 10));
        assert!(!inner.seq_before_eq(20, 10));

        // Wrap around
        assert!(inner.seq_before_eq(65530, 5));
        assert!(!inner.seq_before_eq(5, 65530));
    }
}
