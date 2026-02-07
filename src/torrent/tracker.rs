//! Tracker Client
//!
//! This module implements communication with BitTorrent trackers using
//! both HTTP (BEP 3) and UDP (BEP 15) protocols.

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures::{SinkExt, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::bencode::BencodeValue;
use super::metainfo::Sha1Hash;
use crate::error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result};

/// Default timeout for tracker requests
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Magic constant for UDP tracker protocol
const UDP_PROTOCOL_ID: i64 = 0x41727101980;

/// Minimum allowed announce interval (60 seconds)
/// Prevents aggressive tracker spam
const MIN_ANNOUNCE_INTERVAL: u32 = 60;

/// Maximum allowed announce interval (3600 seconds = 1 hour)
/// Prevents excessively long intervals that could make peers stale
const MAX_ANNOUNCE_INTERVAL: u32 = 3600;

/// Tracker client for HTTP and UDP trackers
pub struct TrackerClient {
    http_client: reqwest::Client,
    peer_id: [u8; 20],
    timeout: Duration,
}

/// Announce event type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceEvent {
    /// No event (regular announce)
    None,
    /// Download has started
    Started,
    /// Download has stopped
    Stopped,
    /// Download has completed
    Completed,
}

impl AnnounceEvent {
    fn to_http_string(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Started => "started",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
        }
    }

    fn to_udp_id(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Completed => 1,
            Self::Started => 2,
            Self::Stopped => 3,
        }
    }
}

/// Announce request parameters
#[derive(Debug, Clone)]
pub struct AnnounceRequest {
    /// Info hash of the torrent
    pub info_hash: Sha1Hash,
    /// Peer ID
    pub peer_id: [u8; 20],
    /// Port we're listening on
    pub port: u16,
    /// Bytes uploaded
    pub uploaded: u64,
    /// Bytes downloaded
    pub downloaded: u64,
    /// Bytes remaining
    pub left: u64,
    /// Event type
    pub event: AnnounceEvent,
    /// Request compact response
    pub compact: bool,
    /// Number of peers to request (0 = default)
    pub numwant: Option<u32>,
    /// Optional key for tracker
    pub key: Option<u32>,
    /// Optional tracker ID (from previous response)
    pub tracker_id: Option<String>,
}

/// Announce response from tracker
#[derive(Debug, Clone)]
pub struct AnnounceResponse {
    /// Interval between announces (seconds)
    pub interval: u32,
    /// Minimum interval (optional)
    pub min_interval: Option<u32>,
    /// Tracker ID (optional)
    pub tracker_id: Option<String>,
    /// Number of complete peers (seeders)
    pub complete: Option<u32>,
    /// Number of incomplete peers (leechers)
    pub incomplete: Option<u32>,
    /// List of peers
    pub peers: Vec<PeerAddr>,
    /// Warning message (optional)
    pub warning_message: Option<String>,
}

/// Peer address from tracker
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerAddr {
    /// IP address
    pub ip: String,
    /// Port
    pub port: u16,
    /// Peer ID (if available)
    pub peer_id: Option<[u8; 20]>,
}

impl PeerAddr {
    /// Convert to socket address
    pub fn to_socket_addr(&self) -> Option<SocketAddr> {
        format!("{}:{}", self.ip, self.port)
            .to_socket_addrs()
            .ok()?
            .next()
    }
}

/// Scrape request
#[derive(Debug, Clone)]
pub struct ScrapeRequest {
    /// Info hashes to scrape
    pub info_hashes: Vec<Sha1Hash>,
}

/// Scrape response for a single torrent
#[derive(Debug, Clone)]
pub struct ScrapeInfo {
    /// Info hash
    pub info_hash: Sha1Hash,
    /// Number of complete peers (seeders)
    pub complete: u32,
    /// Number of incomplete peers (leechers)
    pub incomplete: u32,
    /// Number of times downloaded
    pub downloaded: u32,
    /// Torrent name (optional)
    pub name: Option<String>,
}

/// Scrape response
#[derive(Debug, Clone)]
pub struct ScrapeResponse {
    /// Scrape info for each torrent
    pub files: Vec<ScrapeInfo>,
}

// ============================================================================
// WebSocket Tracker Protocol Types (for wss:// trackers)
// ============================================================================

/// WebSocket announce request (JSON format per WebTorrent protocol)
#[derive(Debug, Serialize)]
struct WsAnnounceRequest<'a> {
    action: &'static str,
    info_hash: String,
    peer_id: String,
    uploaded: u64,
    downloaded: u64,
    left: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    numwant: Option<u32>,
    port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    offers: Option<&'a [()]>, // Placeholder for WebRTC offers (not used)
}

/// WebSocket announce response (JSON format)
#[derive(Debug, Deserialize)]
struct WsAnnounceResponse {
    #[serde(default)]
    interval: Option<u32>,
    #[serde(default)]
    complete: Option<u32>,
    #[serde(default)]
    incomplete: Option<u32>,
    #[serde(default)]
    peers: Option<WsPeers>,
    #[serde(rename = "failure reason")]
    failure_reason: Option<String>,
    #[serde(rename = "warning message")]
    warning_message: Option<String>,
}

/// WebSocket peer list format (can be dictionary or compact base64)
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WsPeers {
    /// Dictionary format with full peer info
    Dict(Vec<WsPeerInfo>),
    /// Compact format (base64-encoded binary, 6 bytes per peer)
    Compact(String),
}

/// WebSocket peer info (dictionary format)
#[derive(Debug, Deserialize)]
struct WsPeerInfo {
    ip: String,
    port: u16,
    #[serde(rename = "peer id")]
    peer_id: Option<String>,
}

impl TrackerClient {
    /// Create a new tracker client with a random peer ID
    pub fn new() -> Result<Self> {
        Self::with_peer_id(generate_peer_id())
    }

    /// Create a tracker client with a specific peer ID
    pub fn with_peer_id(peer_id: [u8; 20]) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::Tls,
                    format!("Failed to create HTTP client: {}", e),
                )
            })?;

        Ok(Self {
            http_client,
            peer_id,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Set the timeout for tracker requests
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Get the peer ID
    pub fn peer_id(&self) -> &[u8; 20] {
        &self.peer_id
    }

    /// Announce to a tracker (auto-detects HTTP, UDP, or WebSocket)
    pub async fn announce(
        &self,
        tracker_url: &str,
        request: &AnnounceRequest,
    ) -> Result<AnnounceResponse> {
        if tracker_url.starts_with("http://") || tracker_url.starts_with("https://") {
            self.announce_http(tracker_url, request).await
        } else if tracker_url.starts_with("udp://") {
            self.announce_udp(tracker_url, request).await
        } else if tracker_url.starts_with("wss://") || tracker_url.starts_with("ws://") {
            self.announce_ws(tracker_url, request).await
        } else {
            Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("Unsupported tracker protocol: {}", tracker_url),
            ))
        }
    }

    /// Announce to an HTTP tracker (BEP 3)
    pub async fn announce_http(
        &self,
        tracker_url: &str,
        request: &AnnounceRequest,
    ) -> Result<AnnounceResponse> {
        // Build the announce URL with query parameters
        let mut url = tracker_url.to_string();
        if url.contains('?') {
            url.push('&');
        } else {
            url.push('?');
        }

        // Add info_hash (URL-encoded)
        url.push_str("info_hash=");
        for byte in &request.info_hash {
            url.push_str(&format!("%{:02X}", byte));
        }

        // Add peer_id (URL-encoded)
        url.push_str("&peer_id=");
        for byte in &request.peer_id {
            url.push_str(&format!("%{:02X}", byte));
        }

        // Add other parameters
        url.push_str(&format!("&port={}", request.port));
        url.push_str(&format!("&uploaded={}", request.uploaded));
        url.push_str(&format!("&downloaded={}", request.downloaded));
        url.push_str(&format!("&left={}", request.left));

        if request.compact {
            url.push_str("&compact=1");
        }

        let event_str = request.event.to_http_string();
        if !event_str.is_empty() {
            url.push_str(&format!("&event={}", event_str));
        }

        if let Some(numwant) = request.numwant {
            url.push_str(&format!("&numwant={}", numwant));
        }

        if let Some(key) = request.key {
            url.push_str(&format!("&key={}", key));
        }

        if let Some(ref tracker_id) = request.tracker_id {
            url.push_str(&format!("&trackerid={}", tracker_id));
        }

        // Make the request
        let response = self.http_client.get(&url).send().await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::Other,
                format!("Tracker request failed: {}", e),
            )
        })?;

        if !response.status().is_success() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("Tracker returned status: {}", response.status()),
            ));
        }

        let body = response.bytes().await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::Other,
                format!("Failed to read tracker response: {}", e),
            )
        })?;

        // Parse bencoded response
        self.parse_http_response(&body)
    }

    /// Parse HTTP tracker response
    fn parse_http_response(&self, data: &[u8]) -> Result<AnnounceResponse> {
        let value = BencodeValue::parse_exact(data).map_err(|_| {
            EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "Invalid tracker response encoding",
            )
        })?;

        let dict = value.as_dict().ok_or_else(|| {
            EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "Tracker response must be a dictionary",
            )
        })?;

        // Check for failure
        if let Some(failure) = dict.get(b"failure reason".as_slice()) {
            if let Some(msg) = failure.as_string() {
                return Err(EngineError::protocol(
                    ProtocolErrorKind::TrackerError,
                    format!("Tracker error: {}", msg),
                ));
            }
        }

        // Parse interval and clamp to safe range
        let raw_interval = dict
            .get(b"interval".as_slice())
            .and_then(|v| v.as_uint())
            .ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::TrackerError,
                    "Missing 'interval' in tracker response",
                )
            })? as u32;
        let interval = raw_interval.clamp(MIN_ANNOUNCE_INTERVAL, MAX_ANNOUNCE_INTERVAL);

        let min_interval = dict
            .get(b"min interval".as_slice())
            .and_then(|v| v.as_uint())
            .map(|v| (v as u32).clamp(MIN_ANNOUNCE_INTERVAL, MAX_ANNOUNCE_INTERVAL));

        let tracker_id = dict
            .get(b"tracker id".as_slice())
            .and_then(|v| v.as_string())
            .map(String::from);

        let complete = dict
            .get(b"complete".as_slice())
            .and_then(|v| v.as_uint())
            .map(|v| v as u32);

        let incomplete = dict
            .get(b"incomplete".as_slice())
            .and_then(|v| v.as_uint())
            .map(|v| v as u32);

        let warning_message = dict
            .get(b"warning message".as_slice())
            .and_then(|v| v.as_string())
            .map(String::from);

        // Parse peers (can be compact or dictionary format)
        let mut peers = self.parse_peers(dict.get(b"peers".as_slice()))?;

        // Parse IPv6 peers (BEP 7)
        let peers6 = self.parse_peers_ipv6(dict.get(b"peers6".as_slice()))?;
        peers.extend(peers6);

        Ok(AnnounceResponse {
            interval,
            min_interval,
            tracker_id,
            complete,
            incomplete,
            peers,
            warning_message,
        })
    }

    /// Parse peers from tracker response
    fn parse_peers(&self, value: Option<&BencodeValue>) -> Result<Vec<PeerAddr>> {
        let Some(value) = value else {
            return Ok(Vec::new());
        };

        match value {
            // Compact format: 6 bytes per peer (4 IP + 2 port)
            BencodeValue::Bytes(data) => {
                if data.len() % 6 != 0 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::TrackerError,
                        "Invalid compact peers length",
                    ));
                }

                let peers = data
                    .chunks_exact(6)
                    .map(|chunk| {
                        let ip = format!("{}.{}.{}.{}", chunk[0], chunk[1], chunk[2], chunk[3]);
                        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                        PeerAddr {
                            ip,
                            port,
                            peer_id: None,
                        }
                    })
                    .collect();

                Ok(peers)
            }

            // Dictionary format
            BencodeValue::List(list) => {
                let mut peers = Vec::new();

                for item in list {
                    let dict = item.as_dict().ok_or_else(|| {
                        EngineError::protocol(
                            ProtocolErrorKind::TrackerError,
                            "Peer entry must be a dictionary",
                        )
                    })?;

                    let ip = dict
                        .get(b"ip".as_slice())
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| {
                            EngineError::protocol(
                                ProtocolErrorKind::TrackerError,
                                "Peer missing 'ip'",
                            )
                        })?
                        .to_string();

                    let port = dict
                        .get(b"port".as_slice())
                        .and_then(|v| v.as_uint())
                        .ok_or_else(|| {
                            EngineError::protocol(
                                ProtocolErrorKind::TrackerError,
                                "Peer missing 'port'",
                            )
                        })? as u16;

                    let peer_id = dict.get(b"peer id".as_slice()).and_then(|v| {
                        v.as_bytes().and_then(|b| {
                            if b.len() == 20 {
                                let mut arr = [0u8; 20];
                                arr.copy_from_slice(b);
                                Some(arr)
                            } else {
                                None
                            }
                        })
                    });

                    peers.push(PeerAddr { ip, port, peer_id });
                }

                Ok(peers)
            }

            _ => Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "Invalid peers format",
            )),
        }
    }

    /// Parse IPv6 peers from tracker response (BEP 7)
    ///
    /// Compact format: 18 bytes per peer (16 bytes IPv6 address + 2 bytes port)
    fn parse_peers_ipv6(&self, value: Option<&BencodeValue>) -> Result<Vec<PeerAddr>> {
        let Some(value) = value else {
            return Ok(Vec::new());
        };

        match value {
            BencodeValue::Bytes(data) => {
                if data.len() % 18 != 0 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::TrackerError,
                        "Invalid compact peers6 length",
                    ));
                }

                let peers = data
                    .chunks_exact(18)
                    .map(|chunk| {
                        let mut octets = [0u8; 16];
                        octets.copy_from_slice(&chunk[..16]);
                        let ip = std::net::Ipv6Addr::from(octets).to_string();
                        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
                        PeerAddr {
                            ip,
                            port,
                            peer_id: None,
                        }
                    })
                    .collect();

                Ok(peers)
            }
            _ => Ok(Vec::new()), // Ignore non-compact IPv6 peers
        }
    }

    /// Announce to a UDP tracker (BEP 15)
    pub async fn announce_udp(
        &self,
        tracker_url: &str,
        request: &AnnounceRequest,
    ) -> Result<AnnounceResponse> {
        // Parse UDP URL
        let url = tracker_url.strip_prefix("udp://").ok_or_else(|| {
            EngineError::protocol(ProtocolErrorKind::TrackerError, "Invalid UDP tracker URL")
        })?;

        // Remove any path component
        let host_port = url.split('/').next().unwrap_or(url);

        // Resolve address (async DNS lookup)
        let addr = tokio::net::lookup_host(host_port)
            .await
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::DnsResolution,
                    format!("Failed to resolve tracker: {}", e),
                )
            })?
            .next()
            .ok_or_else(|| {
                EngineError::network(
                    NetworkErrorKind::DnsResolution,
                    "No addresses found for tracker",
                )
            })?;

        // Create UDP socket
        let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::Other,
                format!("Failed to create UDP socket: {}", e),
            )
        })?;

        socket.connect(addr).await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::ConnectionRefused,
                format!("Failed to connect to tracker: {}", e),
            )
        })?;

        // Step 1: Connect
        let connection_id = self.udp_connect(&socket).await?;

        // Step 2: Announce
        self.udp_announce(&socket, connection_id, request).await
    }

    /// UDP connect request
    async fn udp_connect(&self, socket: &UdpSocket) -> Result<i64> {
        let transaction_id: i32 = rand::rng().random();

        // Build connect request
        // 64-bit protocol ID, 32-bit action (0 = connect), 32-bit transaction ID
        let mut request = Vec::with_capacity(16);
        request.extend_from_slice(&UDP_PROTOCOL_ID.to_be_bytes());
        request.extend_from_slice(&0u32.to_be_bytes()); // Action: connect
        request.extend_from_slice(&transaction_id.to_be_bytes());

        // Send and receive with timeout
        socket.send(&request).await.map_err(|e| {
            EngineError::network(NetworkErrorKind::Other, format!("UDP send failed: {}", e))
        })?;

        let mut response = [0u8; 16];
        let len = timeout(self.timeout, socket.recv(&mut response))
            .await
            .map_err(|_| {
                EngineError::network(NetworkErrorKind::Timeout, "UDP tracker connect timeout")
            })?
            .map_err(|e| {
                EngineError::network(NetworkErrorKind::Other, format!("UDP recv failed: {}", e))
            })?;

        if len < 16 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "UDP connect response too short",
            ));
        }

        // Parse response
        let action = i32::from_be_bytes([response[0], response[1], response[2], response[3]]);
        let resp_transaction_id =
            i32::from_be_bytes([response[4], response[5], response[6], response[7]]);
        let connection_id = i64::from_be_bytes([
            response[8],
            response[9],
            response[10],
            response[11],
            response[12],
            response[13],
            response[14],
            response[15],
        ]);

        if action != 0 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("UDP connect error: action {}", action),
            ));
        }

        if resp_transaction_id != transaction_id {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "UDP transaction ID mismatch",
            ));
        }

        Ok(connection_id)
    }

    /// UDP announce request
    async fn udp_announce(
        &self,
        socket: &UdpSocket,
        connection_id: i64,
        request: &AnnounceRequest,
    ) -> Result<AnnounceResponse> {
        let transaction_id: i32 = rand::rng().random();

        // Build announce request (98 bytes)
        let mut req = Vec::with_capacity(98);
        req.extend_from_slice(&connection_id.to_be_bytes()); // 0-7: connection_id
        req.extend_from_slice(&1u32.to_be_bytes()); // 8-11: action (1 = announce)
        req.extend_from_slice(&transaction_id.to_be_bytes()); // 12-15: transaction_id
        req.extend_from_slice(&request.info_hash); // 16-35: info_hash
        req.extend_from_slice(&request.peer_id); // 36-55: peer_id
        req.extend_from_slice(&request.downloaded.to_be_bytes()); // 56-63: downloaded
        req.extend_from_slice(&request.left.to_be_bytes()); // 64-71: left
        req.extend_from_slice(&request.uploaded.to_be_bytes()); // 72-79: uploaded
        req.extend_from_slice(&request.event.to_udp_id().to_be_bytes()); // 80-83: event
        req.extend_from_slice(&0u32.to_be_bytes()); // 84-87: IP (0 = default)
        req.extend_from_slice(&request.key.unwrap_or(0).to_be_bytes()); // 88-91: key
        req.extend_from_slice(&request.numwant.unwrap_or(u32::MAX).to_be_bytes()); // 92-95: numwant
        req.extend_from_slice(&request.port.to_be_bytes()); // 96-97: port

        // Send request
        socket.send(&req).await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::Other,
                format!("UDP announce send failed: {}", e),
            )
        })?;

        // Receive response (20 bytes header + 6 bytes per peer)
        // 4096 bytes supports ~678 peers ((4096 - 20) / 6), preventing silent truncation
        let mut response = [0u8; 4096];
        let len = timeout(self.timeout, socket.recv(&mut response))
            .await
            .map_err(|_| {
                EngineError::network(NetworkErrorKind::Timeout, "UDP tracker announce timeout")
            })?
            .map_err(|e| {
                EngineError::network(NetworkErrorKind::Other, format!("UDP recv failed: {}", e))
            })?;

        // Minimum response is 8 bytes (action + transaction_id) for error responses
        // Announce responses need 20 bytes minimum
        if len < 8 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "UDP announce response too short (< 8 bytes)",
            ));
        }

        // Parse response header
        let action = i32::from_be_bytes([response[0], response[1], response[2], response[3]]);
        let resp_transaction_id =
            i32::from_be_bytes([response[4], response[5], response[6], response[7]]);

        if action == 3 {
            // Error response - message starts at byte 8 (may be empty)
            let error_msg = if len > 8 {
                String::from_utf8_lossy(&response[8..len]).to_string()
            } else {
                String::from("(no message)")
            };
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("UDP tracker error: {}", error_msg),
            ));
        }

        if action != 1 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("UDP announce unexpected action: {}", action),
            ));
        }

        // Announce response needs at least 20 bytes
        if len < 20 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "UDP announce response too short (< 20 bytes)",
            ));
        }

        if resp_transaction_id != transaction_id {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "UDP transaction ID mismatch",
            ));
        }

        let raw_interval =
            u32::from_be_bytes([response[8], response[9], response[10], response[11]]);
        // Clamp interval to safe range (prevents tracker spam and stale peers)
        let interval = raw_interval.clamp(MIN_ANNOUNCE_INTERVAL, MAX_ANNOUNCE_INTERVAL);
        let incomplete =
            u32::from_be_bytes([response[12], response[13], response[14], response[15]]);
        let complete = u32::from_be_bytes([response[16], response[17], response[18], response[19]]);

        // Parse peers (6 bytes each: 4 IP + 2 port)
        let peers_data = &response[20..len];
        let peers = peers_data
            .chunks_exact(6)
            .map(|chunk| {
                let ip = format!("{}.{}.{}.{}", chunk[0], chunk[1], chunk[2], chunk[3]);
                let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                PeerAddr {
                    ip,
                    port,
                    peer_id: None,
                }
            })
            .collect();

        Ok(AnnounceResponse {
            interval,
            min_interval: None,
            tracker_id: None,
            complete: Some(complete),
            incomplete: Some(incomplete),
            peers,
            warning_message: None,
        })
    }

    /// Announce to a WebSocket tracker (wss:// or ws://)
    ///
    /// Implements the WebTorrent tracker protocol which uses JSON messages
    /// over WebSocket for announce and response.
    async fn announce_ws(
        &self,
        tracker_url: &str,
        request: &AnnounceRequest,
    ) -> Result<AnnounceResponse> {
        // Connect to WebSocket tracker with timeout
        let (mut ws_stream, _) = timeout(self.timeout, connect_async(tracker_url))
            .await
            .map_err(|_| {
                EngineError::network(NetworkErrorKind::Timeout, "WebSocket connection timeout")
            })?
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::ConnectionRefused,
                    format!("WebSocket connection failed: {}", e),
                )
            })?;

        // Build JSON request
        let ws_request = WsAnnounceRequest {
            action: "announce",
            info_hash: hex::encode(request.info_hash),
            peer_id: hex::encode(request.peer_id),
            uploaded: request.uploaded,
            downloaded: request.downloaded,
            left: request.left,
            event: match request.event {
                AnnounceEvent::None => None,
                AnnounceEvent::Started => Some("started"),
                AnnounceEvent::Stopped => Some("stopped"),
                AnnounceEvent::Completed => Some("completed"),
            },
            numwant: request.numwant,
            port: request.port,
            offers: None,
        };

        // Serialize and send request
        let json = serde_json::to_string(&ws_request).map_err(|e| {
            EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("Failed to serialize request: {}", e),
            )
        })?;

        ws_stream
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::Other,
                    format!("WebSocket send failed: {}", e),
                )
            })?;

        // Receive response with timeout
        let response = timeout(self.timeout, ws_stream.next())
            .await
            .map_err(|_| {
                EngineError::network(NetworkErrorKind::Timeout, "WebSocket response timeout")
            })?
            .ok_or_else(|| {
                EngineError::network(NetworkErrorKind::ConnectionReset, "WebSocket closed")
            })?
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::Other,
                    format!("WebSocket recv failed: {}", e),
                )
            })?;

        // Parse response
        let text = match response {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => {
                return Err(EngineError::network(
                    NetworkErrorKind::ConnectionReset,
                    "WebSocket closed by tracker",
                ))
            }
            _ => {
                return Err(EngineError::protocol(
                    ProtocolErrorKind::TrackerError,
                    "Expected text message from tracker",
                ))
            }
        };

        let ws_response: WsAnnounceResponse = serde_json::from_str(&text).map_err(|e| {
            EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("Invalid tracker response JSON: {}", e),
            )
        })?;

        // Check for failure
        if let Some(failure) = ws_response.failure_reason {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("Tracker error: {}", failure),
            ));
        }

        // Parse peers
        let peers = match ws_response.peers {
            Some(WsPeers::Dict(list)) => list
                .into_iter()
                .map(|p| PeerAddr {
                    ip: p.ip,
                    port: p.port,
                    peer_id: p.peer_id.and_then(|s| {
                        hex::decode(&s).ok().and_then(|b| {
                            if b.len() == 20 {
                                let mut arr = [0u8; 20];
                                arr.copy_from_slice(&b);
                                Some(arr)
                            } else {
                                None
                            }
                        })
                    }),
                })
                .collect(),
            Some(WsPeers::Compact(encoded)) => {
                // Decode base64 compact peers (6 bytes per peer: 4 IP + 2 port)
                let data = BASE64.decode(&encoded).map_err(|_| {
                    EngineError::protocol(
                        ProtocolErrorKind::TrackerError,
                        "Invalid base64 in compact peers",
                    )
                })?;

                if data.len() % 6 != 0 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::TrackerError,
                        "Invalid compact peers length",
                    ));
                }

                data.chunks_exact(6)
                    .map(|c| PeerAddr {
                        ip: format!("{}.{}.{}.{}", c[0], c[1], c[2], c[3]),
                        port: u16::from_be_bytes([c[4], c[5]]),
                        peer_id: None,
                    })
                    .collect()
            }
            None => Vec::new(),
        };

        let interval = ws_response
            .interval
            .unwrap_or(1800)
            .clamp(MIN_ANNOUNCE_INTERVAL, MAX_ANNOUNCE_INTERVAL);

        Ok(AnnounceResponse {
            interval,
            min_interval: None,
            tracker_id: None,
            complete: ws_response.complete,
            incomplete: ws_response.incomplete,
            peers,
            warning_message: ws_response.warning_message,
        })
    }

    /// Scrape a tracker for torrent stats
    pub async fn scrape(
        &self,
        tracker_url: &str,
        info_hashes: &[Sha1Hash],
    ) -> Result<ScrapeResponse> {
        if tracker_url.starts_with("http://") || tracker_url.starts_with("https://") {
            self.scrape_http(tracker_url, info_hashes).await
        } else if tracker_url.starts_with("udp://") {
            self.scrape_udp(tracker_url, info_hashes).await
        } else {
            Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("Unsupported tracker protocol: {}", tracker_url),
            ))
        }
    }

    /// HTTP scrape
    async fn scrape_http(
        &self,
        tracker_url: &str,
        info_hashes: &[Sha1Hash],
    ) -> Result<ScrapeResponse> {
        // Convert announce URL to scrape URL
        let scrape_url = if tracker_url.contains("/announce") {
            tracker_url.replace("/announce", "/scrape")
        } else {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "Cannot determine scrape URL from announce URL",
            ));
        };

        // Build URL with info_hash parameters
        let mut url = scrape_url;
        for (i, hash) in info_hashes.iter().enumerate() {
            if i == 0 {
                url.push('?');
            } else {
                url.push('&');
            }
            url.push_str("info_hash=");
            for byte in hash {
                url.push_str(&format!("%{:02X}", byte));
            }
        }

        // Make request
        let response = self.http_client.get(&url).send().await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::Other,
                format!("Scrape request failed: {}", e),
            )
        })?;

        if !response.status().is_success() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("Scrape returned status: {}", response.status()),
            ));
        }

        let body = response.bytes().await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::Other,
                format!("Failed to read scrape response: {}", e),
            )
        })?;

        // Parse response
        let value = BencodeValue::parse_exact(&body).map_err(|_| {
            EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "Invalid scrape response encoding",
            )
        })?;

        let dict = value.as_dict().ok_or_else(|| {
            EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "Scrape response must be a dictionary",
            )
        })?;

        let files_dict = dict.get(b"files".as_slice()).and_then(|v| v.as_dict());

        let mut files = Vec::new();

        if let Some(fd) = files_dict {
            for (hash_bytes, info_value) in fd {
                if hash_bytes.len() != 20 {
                    continue;
                }

                let mut info_hash = [0u8; 20];
                info_hash.copy_from_slice(hash_bytes);

                if let Some(info_dict) = info_value.as_dict() {
                    let complete = info_dict
                        .get(b"complete".as_slice())
                        .and_then(|v| v.as_uint())
                        .unwrap_or(0) as u32;

                    let incomplete = info_dict
                        .get(b"incomplete".as_slice())
                        .and_then(|v| v.as_uint())
                        .unwrap_or(0) as u32;

                    let downloaded = info_dict
                        .get(b"downloaded".as_slice())
                        .and_then(|v| v.as_uint())
                        .unwrap_or(0) as u32;

                    let name = info_dict
                        .get(b"name".as_slice())
                        .and_then(|v| v.as_string())
                        .map(String::from);

                    files.push(ScrapeInfo {
                        info_hash,
                        complete,
                        incomplete,
                        downloaded,
                        name,
                    });
                }
            }
        }

        Ok(ScrapeResponse { files })
    }

    /// UDP scrape
    async fn scrape_udp(
        &self,
        tracker_url: &str,
        info_hashes: &[Sha1Hash],
    ) -> Result<ScrapeResponse> {
        // Parse UDP URL
        let url = tracker_url.strip_prefix("udp://").ok_or_else(|| {
            EngineError::protocol(ProtocolErrorKind::TrackerError, "Invalid UDP tracker URL")
        })?;

        let host_port = url.split('/').next().unwrap_or(url);

        // Resolve address (async DNS lookup)
        let addr = tokio::net::lookup_host(host_port)
            .await
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::DnsResolution,
                    format!("Failed to resolve tracker: {}", e),
                )
            })?
            .next()
            .ok_or_else(|| {
                EngineError::network(
                    NetworkErrorKind::DnsResolution,
                    "No addresses found for tracker",
                )
            })?;

        let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::Other,
                format!("Failed to create UDP socket: {}", e),
            )
        })?;

        socket.connect(addr).await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::ConnectionRefused,
                format!("Failed to connect to tracker: {}", e),
            )
        })?;

        // Connect
        let connection_id = self.udp_connect(&socket).await?;

        // Scrape request
        let transaction_id: i32 = rand::rng().random();

        let mut req = Vec::with_capacity(16 + 20 * info_hashes.len());
        req.extend_from_slice(&connection_id.to_be_bytes());
        req.extend_from_slice(&2u32.to_be_bytes()); // Action: scrape
        req.extend_from_slice(&transaction_id.to_be_bytes());

        for hash in info_hashes {
            req.extend_from_slice(hash);
        }

        socket.send(&req).await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::Other,
                format!("UDP scrape send failed: {}", e),
            )
        })?;

        // Response: 8 bytes header + 12 bytes per torrent
        let mut response = [0u8; 1024];
        let len = timeout(self.timeout, socket.recv(&mut response))
            .await
            .map_err(|_| EngineError::network(NetworkErrorKind::Timeout, "UDP scrape timeout"))?
            .map_err(|e| {
                EngineError::network(NetworkErrorKind::Other, format!("UDP recv failed: {}", e))
            })?;

        if len < 8 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "UDP scrape response too short",
            ));
        }

        let action = i32::from_be_bytes([response[0], response[1], response[2], response[3]]);
        let resp_transaction_id =
            i32::from_be_bytes([response[4], response[5], response[6], response[7]]);

        if action != 2 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                format!("UDP scrape unexpected action: {}", action),
            ));
        }

        if resp_transaction_id != transaction_id {
            return Err(EngineError::protocol(
                ProtocolErrorKind::TrackerError,
                "UDP transaction ID mismatch",
            ));
        }

        // Parse results (12 bytes each: 4 complete + 4 downloaded + 4 incomplete)
        let results = &response[8..len];
        let mut files = Vec::new();

        for (i, chunk) in results.chunks_exact(12).enumerate() {
            if i >= info_hashes.len() {
                break;
            }

            let complete = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let downloaded = u32::from_be_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let incomplete = u32::from_be_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);

            files.push(ScrapeInfo {
                info_hash: info_hashes[i],
                complete,
                incomplete,
                downloaded,
                name: None,
            });
        }

        Ok(ScrapeResponse { files })
    }
}

impl Default for TrackerClient {
    fn default() -> Self {
        Self::new().expect("Failed to create default TrackerClient (TLS initialization failed)")
    }
}

/// Generate a random peer ID in Azureus-style
///
/// Format: -<2-char client><4-char version>-<12 random bytes>
/// Example: -GD0001-xxxxxxxxxxxx
pub fn generate_peer_id() -> [u8; 20] {
    let mut peer_id = [0u8; 20];

    // Client identifier: -GD (Gosh Downloader)
    peer_id[0] = b'-';
    peer_id[1] = b'G';
    peer_id[2] = b'D';

    // Version: 0001
    peer_id[3] = b'0';
    peer_id[4] = b'0';
    peer_id[5] = b'0';
    peer_id[6] = b'1';

    peer_id[7] = b'-';

    // Random bytes for uniqueness
    rand::rng().fill(&mut peer_id[8..]);

    peer_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_peer_id() {
        let peer_id = generate_peer_id();
        assert_eq!(peer_id.len(), 20);
        assert_eq!(&peer_id[0..8], b"-GD0001-");
    }

    #[test]
    fn test_announce_event() {
        assert_eq!(AnnounceEvent::None.to_http_string(), "");
        assert_eq!(AnnounceEvent::Started.to_http_string(), "started");
        assert_eq!(AnnounceEvent::Stopped.to_http_string(), "stopped");
        assert_eq!(AnnounceEvent::Completed.to_http_string(), "completed");

        assert_eq!(AnnounceEvent::None.to_udp_id(), 0);
        assert_eq!(AnnounceEvent::Completed.to_udp_id(), 1);
        assert_eq!(AnnounceEvent::Started.to_udp_id(), 2);
        assert_eq!(AnnounceEvent::Stopped.to_udp_id(), 3);
    }

    #[test]
    fn test_peer_addr_to_socket() {
        let peer = PeerAddr {
            ip: "127.0.0.1".to_string(),
            port: 6881,
            peer_id: None,
        };

        let addr = peer.to_socket_addr().unwrap();
        assert_eq!(addr.port(), 6881);
    }

    #[test]
    fn test_parse_compact_peers_ipv6() {
        let client = TrackerClient::new().unwrap();

        // Compact IPv6 format: 18 bytes per peer (16 IPv6 + 2 port)
        // ::1 port 6881 and 2001:db8::1 port 8080
        let mut data = Vec::new();
        // ::1
        data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        data.extend_from_slice(&0x1AE1u16.to_be_bytes()); // port 6881
                                                          // 2001:db8::1
        data.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        data.extend_from_slice(&0x1F90u16.to_be_bytes()); // port 8080

        let value = BencodeValue::Bytes(data);
        let peers = client.parse_peers_ipv6(Some(&value)).unwrap();

        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].ip, "::1");
        assert_eq!(peers[0].port, 6881);
        assert_eq!(peers[1].ip, "2001:db8::1");
        assert_eq!(peers[1].port, 8080);
    }

    #[test]
    fn test_parse_compact_peers() {
        let client = TrackerClient::new().unwrap();

        // Compact format: 6 bytes per peer
        let data = vec![
            127, 0, 0, 1, 0x1A, 0xE1, // 127.0.0.1:6881
            192, 168, 1, 1, 0x1A, 0xE2, // 192.168.1.1:6882
        ];

        let value = BencodeValue::Bytes(data);
        let peers = client.parse_peers(Some(&value)).unwrap();

        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].ip, "127.0.0.1");
        assert_eq!(peers[0].port, 6881);
        assert_eq!(peers[1].ip, "192.168.1.1");
        assert_eq!(peers[1].port, 6882);
    }
}
