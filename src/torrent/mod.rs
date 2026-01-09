//! BitTorrent Module
//!
//! This module handles BitTorrent protocol downloads including:
//! - Torrent file parsing (metainfo)
//! - Magnet URI handling
//! - Tracker communication (HTTP/UDP)
//! - Peer wire protocol
//! - Piece management with SHA-1 verification
//! - DHT peer discovery (BEP 5)
//! - Peer Exchange (BEP 11)
//! - Local Peer Discovery (BEP 14)
//! - Choking algorithm

pub mod bencode;
pub mod choking;
pub mod dht;
pub mod lpd;
pub mod magnet;
pub mod metadata;
pub mod metainfo;
pub mod mse;
pub mod peer;
pub mod pex;
pub mod piece;
pub mod tracker;
pub mod transport;
pub mod utp;
pub mod webseed;

// Re-export commonly used types
pub use bencode::BencodeValue;
pub use choking::{ChokingConfig, ChokingDecision, ChokingManager, PeerStats};
pub use dht::{DhtClient, DhtManager};
pub use lpd::{LocalPeer, LpdManager, LpdService};
pub use magnet::MagnetUri;
pub use metadata::{MetadataFetcher, MetadataMessage, MetadataMessageType, METADATA_EXTENSION_NAME, OUR_METADATA_EXTENSION_ID};
pub use metainfo::{FileInfo, Info, Metainfo, Sha1Hash};
pub use peer::{ConnectionState, PeerConnection, PeerMessage, BLOCK_SIZE, OUR_PEX_EXTENSION_ID};
pub use pex::{ExtensionHandshake, PexMessage, PexState, PEX_EXTENSION_NAME};
pub use piece::{BlockRequest, PendingPiece, PieceManager, PieceProgress};
pub use webseed::{WebSeed, WebSeedConfig, WebSeedEvent, WebSeedManager, WebSeedState, WebSeedType};
pub use mse::{EncryptedStream, EncryptionPolicy, MseConfig, PeerStream, connect_with_mse};
pub use transport::{PeerTransport, TcpTransport, TransportType};
pub use utp::{UtpConfig, UtpMux, UtpSocket};
pub use tracker::{
    AnnounceEvent, AnnounceRequest, AnnounceResponse, PeerAddr, ScrapeInfo, ScrapeRequest,
    ScrapeResponse, TrackerClient,
};

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tokio::sync::{broadcast, Semaphore};

use crate::error::Result;
use crate::types::{DownloadEvent, DownloadId, DownloadProgress};
use pex::parse_extension_handshake;

/// Configuration for torrent downloads
#[derive(Debug, Clone)]
pub struct TorrentConfig {
    /// Maximum number of peers per torrent
    pub max_peers: usize,
    /// Port range for incoming connections
    pub listen_port_range: (u16, u16),
    /// Enable DHT (Phase 4)
    pub enable_dht: bool,
    /// Enable Peer Exchange (Phase 4)
    pub enable_pex: bool,
    /// Enable Local Peer Discovery (Phase 4)
    pub enable_lpd: bool,
    /// Seed ratio limit (stop seeding after this ratio)
    pub seed_ratio: Option<f64>,
    /// Maximum upload speed (bytes/sec, 0 = unlimited)
    pub max_upload_speed: u64,
    /// Maximum download speed (bytes/sec, 0 = unlimited)
    pub max_download_speed: u64,
    /// Announce interval override (0 = use tracker's)
    pub announce_interval: u64,
    /// Request timeout for blocks
    pub request_timeout: Duration,
    /// Keep-alive interval
    pub keepalive_interval: Duration,
    /// Maximum outstanding piece requests per peer
    pub max_pending_requests: usize,
    /// DHT bootstrap nodes
    pub dht_bootstrap_nodes: Vec<String>,
    /// Peer loop tick interval in milliseconds.
    /// Controls how frequently the peer loop checks for state changes and cleanup.
    pub tick_interval_ms: u64,
    /// Peer connection attempt interval in seconds.
    /// Controls how frequently we attempt to connect to new peers.
    pub connect_interval_secs: u64,
    /// Choking algorithm update interval in seconds.
    /// Controls how frequently we recalculate which peers to unchoke.
    pub choking_interval_secs: u64,
}

impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            max_peers: 50,
            listen_port_range: (6881, 6889),
            enable_dht: true,
            enable_pex: true,
            enable_lpd: true,
            seed_ratio: None,
            max_upload_speed: 0,
            max_download_speed: 0,
            announce_interval: 0,
            request_timeout: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(120),
            max_pending_requests: 16,
            dht_bootstrap_nodes: vec![
                "router.bittorrent.com:6881".to_string(),
                "router.utorrent.com:6881".to_string(),
                "dht.transmissionbt.com:6881".to_string(),
                "dht.aelitis.com:6881".to_string(),
            ],
            tick_interval_ms: 100,
            connect_interval_secs: 5,
            choking_interval_secs: 10,
        }
    }
}

/// State of a torrent download
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    /// Checking existing files
    Checking,
    /// Downloading metadata (for magnet links)
    Metadata,
    /// Downloading pieces
    Downloading,
    /// Seeding (complete)
    Seeding,
    /// Paused
    Paused,
    /// Stopped
    Stopped,
    /// Error
    Error,
}

/// Torrent download coordinator
#[allow(dead_code)]
pub struct TorrentDownloader {
    /// Download ID
    id: DownloadId,
    /// Metainfo (None for magnet links until metadata received)
    metainfo: RwLock<Option<Arc<Metainfo>>>,
    /// Magnet URI (if started from magnet)
    magnet: Option<MagnetUri>,
    /// Info hash
    info_hash: Sha1Hash,
    /// Save directory
    save_dir: PathBuf,
    /// Configuration
    config: TorrentConfig,
    /// Piece manager
    piece_manager: RwLock<Option<Arc<PieceManager>>>,
    /// Tracker client
    tracker_client: TrackerClient,
    /// Current state
    state: RwLock<TorrentState>,
    /// Connected peers
    peers: RwLock<HashMap<SocketAddr, PeerInfo>>,
    /// Known peer addresses (from trackers, DHT, etc.)
    known_peers: RwLock<HashSet<SocketAddr>>,
    /// Event sender
    event_tx: broadcast::Sender<DownloadEvent>,
    /// Shutdown flag
    shutdown: AtomicBool,
    /// Statistics
    stats: TorrentStats,
    /// Peer connection semaphore
    peer_semaphore: Semaphore,
    /// Metadata fetcher for magnet links (BEP 9)
    metadata_fetcher: Option<Arc<MetadataFetcher>>,
    /// Background discovery task handles (DHT, LPD, etc.)
    discovery_tasks: RwLock<Vec<tokio::task::JoinHandle<()>>>,
    /// Choking manager for peer unchoke decisions
    choking_manager: RwLock<ChokingManager>,
    /// Shared peer stats for choking algorithm (addr -> stats)
    shared_peer_stats: Arc<RwLock<HashMap<SocketAddr, PeerStats>>>,
    /// Choking decisions (addr -> should_be_unchoked)
    choking_decisions: Arc<RwLock<HashMap<SocketAddr, bool>>>,
    /// WebSeed manager (initialized when metainfo available and webseeds exist)
    webseed_manager: RwLock<Option<Arc<WebSeedManager>>>,
    /// WebSeed event receiver task handle
    webseed_task: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

/// Information about a connected peer
#[derive(Debug)]
#[allow(dead_code)]
struct PeerInfo {
    /// Socket address
    addr: SocketAddr,
    /// Peer ID
    peer_id: Option<[u8; 20]>,
    /// Client name
    client: Option<String>,
    /// Connection established time
    connected_at: Instant,
    /// Download speed (bytes/sec)
    download_speed: u64,
    /// Upload speed (bytes/sec)
    upload_speed: u64,
    /// Total downloaded
    downloaded: u64,
    /// Total uploaded
    uploaded: u64,
    /// Is choking us
    choking: bool,
    /// Is interested in us
    interested: bool,
}

/// Torrent statistics
#[allow(dead_code)]
struct TorrentStats {
    downloaded: AtomicU64,
    uploaded: AtomicU64,
    download_speed: AtomicU64,
    upload_speed: AtomicU64,
    peers_connected: AtomicU64,
    seeders: AtomicU64,
    leechers: AtomicU64,
}

impl TorrentStats {
    fn new() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            download_speed: AtomicU64::new(0),
            upload_speed: AtomicU64::new(0),
            peers_connected: AtomicU64::new(0),
            seeders: AtomicU64::new(0),
            leechers: AtomicU64::new(0),
        }
    }
}

impl TorrentDownloader {
    /// Create a new torrent downloader from a .torrent file
    pub fn from_torrent(
        id: DownloadId,
        metainfo: Metainfo,
        save_dir: PathBuf,
        config: TorrentConfig,
        event_tx: broadcast::Sender<DownloadEvent>,
    ) -> Result<Self> {
        let info_hash = metainfo.info_hash;
        let metainfo = Arc::new(metainfo);
        let piece_manager = Arc::new(PieceManager::new(metainfo.clone(), save_dir.clone()));

        Ok(Self {
            id,
            metainfo: RwLock::new(Some(metainfo)),
            magnet: None,
            info_hash,
            save_dir,
            config: config.clone(),
            piece_manager: RwLock::new(Some(piece_manager)),
            tracker_client: TrackerClient::new(),
            state: RwLock::new(TorrentState::Checking),
            peers: RwLock::new(HashMap::new()),
            known_peers: RwLock::new(HashSet::new()),
            event_tx,
            shutdown: AtomicBool::new(false),
            stats: TorrentStats::new(),
            peer_semaphore: Semaphore::new(config.max_peers),
            metadata_fetcher: None, // Not needed for .torrent files
            discovery_tasks: RwLock::new(Vec::new()),
            choking_manager: RwLock::new(ChokingManager::new(ChokingConfig::default())),
            shared_peer_stats: Arc::new(RwLock::new(HashMap::new())),
            choking_decisions: Arc::new(RwLock::new(HashMap::new())),
            webseed_manager: RwLock::new(None),
            webseed_task: RwLock::new(None),
        })
    }

    /// Create a new torrent downloader from a magnet URI
    pub fn from_magnet(
        id: DownloadId,
        magnet: MagnetUri,
        save_dir: PathBuf,
        config: TorrentConfig,
        event_tx: broadcast::Sender<DownloadEvent>,
    ) -> Result<Self> {
        let info_hash = magnet.info_hash;

        Ok(Self {
            id,
            metainfo: RwLock::new(None),
            magnet: Some(magnet.clone()),
            info_hash,
            save_dir,
            config: config.clone(),
            piece_manager: RwLock::new(None),
            tracker_client: TrackerClient::new(),
            state: RwLock::new(TorrentState::Metadata),
            peers: RwLock::new(HashMap::new()),
            known_peers: RwLock::new(HashSet::new()),
            event_tx,
            shutdown: AtomicBool::new(false),
            stats: TorrentStats::new(),
            peer_semaphore: Semaphore::new(config.max_peers),
            metadata_fetcher: Some(Arc::new(MetadataFetcher::new(info_hash))),
            discovery_tasks: RwLock::new(Vec::new()),
            choking_manager: RwLock::new(ChokingManager::new(ChokingConfig::default())),
            shared_peer_stats: Arc::new(RwLock::new(HashMap::new())),
            choking_decisions: Arc::new(RwLock::new(HashMap::new())),
            webseed_manager: RwLock::new(None),
            webseed_task: RwLock::new(None),
        })
    }

    /// Get the download ID
    pub fn id(&self) -> DownloadId {
        self.id
    }

    /// Set selected files for partial download.
    ///
    /// Only pieces that contain data from the selected files will be downloaded.
    /// If `file_indices` is empty or None, all files will be downloaded.
    pub fn set_selected_files(&self, file_indices: Option<&[usize]>) {
        if let Some(ref pm) = *self.piece_manager.read() {
            pm.set_selected_files(file_indices);
        }
    }

    /// Enable or disable sequential download mode.
    /// When enabled, pieces are downloaded in order for streaming support.
    pub fn set_sequential(&self, sequential: bool) {
        if let Some(ref pm) = *self.piece_manager.read() {
            pm.set_sequential(sequential);
        }
    }

    /// Get the info hash
    pub fn info_hash(&self) -> &Sha1Hash {
        &self.info_hash
    }

    /// Get the info hash as hex string
    pub fn info_hash_hex(&self) -> String {
        self.info_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    /// Get the current state
    pub fn state(&self) -> TorrentState {
        *self.state.read()
    }

    /// Get the name (from metainfo or magnet)
    pub fn name(&self) -> String {
        if let Some(ref metainfo) = *self.metainfo.read() {
            metainfo.info.name.clone()
        } else if let Some(ref magnet) = self.magnet {
            magnet.name()
        } else {
            self.info_hash_hex()
        }
    }

    /// Get metainfo if available.
    pub fn metainfo(&self) -> Option<Arc<Metainfo>> {
        self.metainfo.read().clone()
    }

    /// Get progress information
    pub fn progress(&self) -> DownloadProgress {
        let (download_speed, upload_speed) = self.aggregate_transfer_rates();
        self.stats
            .download_speed
            .store(download_speed, Ordering::Relaxed);
        self.stats
            .upload_speed
            .store(upload_speed, Ordering::Relaxed);

        let pm_guard = self.piece_manager.read();

        let (completed_size, total_size) = if let Some(ref pm) = *pm_guard {
            let progress = pm.progress();
            (progress.verified_bytes, progress.total_size)
        } else {
            (0, 0)
        };

        DownloadProgress {
            total_size: if total_size > 0 { Some(total_size) } else { None },
            completed_size,
            download_speed: self.stats.download_speed.load(Ordering::Relaxed),
            upload_speed: self.stats.upload_speed.load(Ordering::Relaxed),
            connections: self.stats.peers_connected.load(Ordering::Relaxed) as u32,
            seeders: self.stats.seeders.load(Ordering::Relaxed) as u32,
            peers: self.stats.leechers.load(Ordering::Relaxed) as u32,
            eta_seconds: self.calculate_eta(),
        }
    }

    fn aggregate_transfer_rates(&self) -> (u64, u64) {
        let stats = self.shared_peer_stats.read();
        let mut download_speed = 0u64;
        let mut upload_speed = 0u64;

        for peer_stats in stats.values() {
            download_speed = download_speed.saturating_add(peer_stats.download_rate);
            upload_speed = upload_speed.saturating_add(peer_stats.upload_rate);
        }

        (download_speed, upload_speed)
    }

    /// Calculate ETA in seconds
    fn calculate_eta(&self) -> Option<u64> {
        let pm_guard = self.piece_manager.read();
        let pm = pm_guard.as_ref()?;

        let progress = pm.progress();
        let remaining = progress.bytes_remaining();

        if remaining == 0 {
            return Some(0);
        }

        let speed = self.stats.download_speed.load(Ordering::Relaxed);
        if speed == 0 {
            return None;
        }

        Some(remaining / speed)
    }

    /// Start the download
    pub async fn start(self: Arc<Self>) -> Result<()> {
        // Verify existing files if we have metainfo
        // Clone the Arc to avoid holding the lock across await
        let pm_clone = self.piece_manager.read().clone();
        if let Some(pm) = pm_clone {
            *self.state.write() = TorrentState::Checking;

            let valid = pm.verify_existing().await?;
            tracing::info!(
                "Verified {} existing pieces for torrent {}",
                valid,
                self.info_hash_hex()
            );

            if pm.is_complete() {
                *self.state.write() = TorrentState::Seeding;
            } else {
                *self.state.write() = TorrentState::Downloading;
            }
        }

        // Announce to trackers
        self.announce_to_trackers(AnnounceEvent::Started).await?;

        // Spawn DHT discovery loop
        if self.dht_enabled() {
            let dl = Arc::clone(&self);
            let handle = tokio::spawn(async move {
                if let Err(e) = dl.run_dht_discovery().await {
                    tracing::warn!("DHT discovery error: {}", e);
                }
            });
            self.discovery_tasks.write().push(handle);
        }

        // Spawn LPD discovery loop
        if self.lpd_enabled() {
            let dl = Arc::clone(&self);
            let handle = tokio::spawn(async move {
                if let Err(e) = dl.run_lpd_discovery().await {
                    tracing::warn!("LPD discovery error: {}", e);
                }
            });
            self.discovery_tasks.write().push(handle);
        }

        // Start webseed downloads if available
        self.start_webseeds().await;

        Ok(())
    }

    /// Initialize and start webseed downloads if metainfo has webseeds
    async fn start_webseeds(&self) {
        // Get metainfo and piece manager (both needed for webseeds)
        let metainfo = self.metainfo.read().clone();
        let piece_manager = self.piece_manager.read().clone();

        let (metainfo, piece_manager) = match (metainfo, piece_manager) {
            (Some(m), Some(p)) => (m, p),
            _ => return, // No metainfo or piece manager yet
        };

        // Check if torrent has webseeds
        if !metainfo.has_webseeds() {
            tracing::debug!(
                "No webseeds for torrent {}",
                self.info_hash_hex()
            );
            return;
        }

        tracing::info!(
            "Starting webseed downloads for torrent {} with {} seeds",
            self.info_hash_hex(),
            metainfo.all_webseeds().len()
        );

        // Create webseed manager
        let config = WebSeedConfig::default();
        let (manager, mut event_rx) = match WebSeedManager::new(metainfo.clone(), piece_manager.clone(), config) {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(
                    "Failed to create WebSeedManager for torrent {}: {}",
                    self.info_hash_hex(),
                    e
                );
                return;
            }
        };
        let manager = Arc::new(manager);

        // Store manager reference
        *self.webseed_manager.write() = Some(Arc::clone(&manager));

        // Spawn webseed download task
        let manager_clone = Arc::clone(&manager);
        let webseed_handle = tokio::spawn(async move {
            if let Err(e) = manager_clone.run().await {
                tracing::warn!("WebSeed manager error: {}", e);
            }
        });

        // Spawn event handler task
        let piece_manager_clone = piece_manager.clone();
        let _event_tx = self.event_tx.clone(); // For future progress events
        let _info_hash_hex = self.info_hash_hex();
        let event_handle = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    WebSeedEvent::PieceComplete { piece_index, data, source_url } => {
                        // Write piece to disk (already verified in webseed manager)
                        match piece_manager_clone.write_piece_from_webseed(piece_index, &data).await {
                            Ok(()) => {
                                tracing::debug!(
                                    "WebSeed piece {} saved from {}",
                                    piece_index,
                                    source_url
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to save webseed piece {}: {}",
                                    piece_index,
                                    e
                                );
                            }
                        }
                    }
                    WebSeedEvent::PieceFailed { piece_index, source_url, error, .. } => {
                        tracing::debug!(
                            "WebSeed piece {} failed from {}: {}",
                            piece_index,
                            source_url,
                            error
                        );
                    }
                    WebSeedEvent::SpeedUpdate { source_url, speed } => {
                        tracing::trace!(
                            "WebSeed {} speed: {} bytes/sec",
                            source_url,
                            speed
                        );
                    }
                }
            }
        });

        // Store the task handles
        *self.webseed_task.write() = Some(webseed_handle);
        self.discovery_tasks.write().push(event_handle);
    }

    /// Announce to all known trackers
    async fn announce_to_trackers(&self, event: AnnounceEvent) -> Result<()> {
        let trackers = self.get_tracker_urls();

        if trackers.is_empty() {
            tracing::warn!(
                "No trackers available for torrent {}",
                self.info_hash_hex()
            );
            return Ok(());
        }

        // Get progress data in a block so the lock is dropped before await
        let (downloaded, left) = {
            let pm_guard = self.piece_manager.read();
            if let Some(ref pm) = *pm_guard {
                let progress = pm.progress();
                (progress.verified_bytes, progress.bytes_remaining())
            } else if let Some(ref magnet) = self.magnet {
                (0, magnet.exact_length.unwrap_or(1))
            } else {
                (0, 0)
            }
        };

        let request = AnnounceRequest {
            info_hash: self.info_hash,
            peer_id: *self.tracker_client.peer_id(),
            port: self.config.listen_port_range.0,
            uploaded: self.stats.uploaded.load(Ordering::Relaxed),
            downloaded,
            left,
            event,
            compact: true,
            numwant: Some(self.config.max_peers as u32),
            key: None,
            tracker_id: None,
        };

        for tracker_url in trackers {
            match self.tracker_client.announce(&tracker_url, &request).await {
                Ok(response) => {
                    tracing::info!(
                        "Announced to {}: {} peers, interval {}s",
                        tracker_url,
                        response.peers.len(),
                        response.interval
                    );

                    // Update stats
                    if let Some(complete) = response.complete {
                        self.stats.seeders.store(complete as u64, Ordering::Relaxed);
                    }
                    if let Some(incomplete) = response.incomplete {
                        self.stats
                            .leechers
                            .store(incomplete as u64, Ordering::Relaxed);
                    }

                    // Add peers to known list
                    let mut known = self.known_peers.write();
                    for peer in response.peers {
                        if let Some(addr) = peer.to_socket_addr() {
                            known.insert(addr);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to announce to {}: {}", tracker_url, e);
                }
            }
        }

        Ok(())
    }

    /// Get tracker URLs
    fn get_tracker_urls(&self) -> Vec<String> {
        if let Some(ref metainfo) = *self.metainfo.read() {
            metainfo.all_trackers()
        } else if let Some(ref magnet) = self.magnet {
            magnet.trackers.clone()
        } else {
            Vec::new()
        }
    }

    /// Pause the download
    pub fn pause(&self) {
        *self.state.write() = TorrentState::Paused;
        // Disconnect all peers and stop requesting
    }

    /// Resume the download
    pub fn resume(&self) {
        let current = *self.state.read();
        if current == TorrentState::Paused {
            // Determine new state based on progress
            let pm_guard = self.piece_manager.read();
            if let Some(ref pm) = *pm_guard {
                if pm.is_complete() {
                    *self.state.write() = TorrentState::Seeding;
                } else {
                    *self.state.write() = TorrentState::Downloading;
                }
            }
        }
    }

    /// Stop the download
    pub async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        *self.state.write() = TorrentState::Stopped;

        // Abort all discovery tasks (DHT, LPD, etc.)
        {
            let mut tasks = self.discovery_tasks.write();
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }

        // Announce stopped
        self.announce_to_trackers(AnnounceEvent::Stopped).await?;

        Ok(())
    }

    /// Check if download is complete
    pub fn is_complete(&self) -> bool {
        let pm_guard = self.piece_manager.read();
        pm_guard.as_ref().map(|pm| pm.is_complete()).unwrap_or(false)
    }

    /// Get number of connected peers
    pub fn peer_count(&self) -> usize {
        self.peers.read().len()
    }

    /// Get list of known peer addresses
    pub fn known_peer_addresses(&self) -> Vec<SocketAddr> {
        self.known_peers.read().iter().cloned().collect()
    }

    /// Check if this is a private torrent.
    ///
    /// Private torrents should not use DHT, PEX, or LPD (BEP 27).
    pub fn is_private(&self) -> bool {
        self.metainfo
            .read()
            .as_ref()
            .map(|m| m.info.private)
            .unwrap_or(false)
    }

    /// Add discovered peers to the known peers list.
    ///
    /// This is used by DHT, PEX, and LPD to add discovered peers.
    pub fn add_known_peers(&self, peers: impl IntoIterator<Item = SocketAddr>) {
        let mut known = self.known_peers.write();
        for peer in peers {
            known.insert(peer);
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &TorrentConfig {
        &self.config
    }

    /// Check if DHT is enabled for this torrent.
    pub fn dht_enabled(&self) -> bool {
        self.config.enable_dht && !self.is_private()
    }

    /// Check if PEX is enabled for this torrent.
    pub fn pex_enabled(&self) -> bool {
        self.config.enable_pex && !self.is_private()
    }

    /// Check if LPD is enabled for this torrent.
    pub fn lpd_enabled(&self) -> bool {
        self.config.enable_lpd && !self.is_private()
    }

    /// Run the main peer connection loop.
    /// This spawns tasks to connect to peers and download pieces.
    pub async fn run_peer_loop(self: Arc<Self>) -> Result<()> {
        // Use configurable intervals from TorrentConfig
        let tick_duration = Duration::from_millis(self.config.tick_interval_ms);
        let connect_duration = Duration::from_secs(self.config.connect_interval_secs);
        let choking_duration = Duration::from_secs(self.config.choking_interval_secs);
        let max_pending_per_peer = self.config.max_pending_requests;

        let mut tick_interval = tokio::time::interval(tick_duration);
        let mut connect_interval = tokio::time::interval(connect_duration);
        let mut choking_interval = tokio::time::interval(choking_duration);

        // Active peer connections (addr -> connection task handle)
        let active_connections: Arc<RwLock<HashMap<SocketAddr, tokio::task::JoinHandle<()>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        loop {
            tokio::select! {
                _ = tick_interval.tick() => {
                    // Check for shutdown
                    if self.shutdown.load(Ordering::SeqCst) {
                        break;
                    }

                    // Check if state allows downloading
                    let state = *self.state.read();
                    if state == TorrentState::Paused || state == TorrentState::Stopped {
                        continue;
                    }

                    // Clean up finished connection tasks and their stats
                    {
                        let mut conns = active_connections.write();
                        let disconnected: Vec<SocketAddr> = conns
                            .iter()
                            .filter(|(_, handle)| handle.is_finished())
                            .map(|(addr, _)| *addr)
                            .collect();

                        for addr in &disconnected {
                            conns.remove(addr);
                            // Clean up peer stats and choking decisions
                            self.shared_peer_stats.write().remove(addr);
                            self.choking_decisions.write().remove(addr);
                            self.choking_manager.write().peer_disconnected(addr);
                        }
                    }
                }

                _ = connect_interval.tick() => {
                    // Try to connect to more peers if below max
                    let current_count = active_connections.read().len();
                    if current_count < self.config.max_peers {
                        self.connect_to_new_peers(
                            Arc::clone(&active_connections),
                            max_pending_per_peer,
                        ).await;
                    }
                }

                _ = choking_interval.tick() => {
                    // Run choking algorithm to decide which peers to unchoke
                    self.run_choking_algorithm();
                }
            }

            // Check if download is complete
            let is_complete = {
                let pm_guard = self.piece_manager.read();
                pm_guard.as_ref().map(|pm| pm.is_complete()).unwrap_or(false)
            };

            if is_complete {
                let current_state = *self.state.read();
                if current_state != TorrentState::Seeding {
                    *self.state.write() = TorrentState::Seeding;
                    // Announce completion
                    let _ = self.announce_to_trackers(AnnounceEvent::Completed).await;
                    tracing::info!("Download complete for {}", self.name());
                }

                // Check seed ratio - stop seeding if reached
                if let Some(target_ratio) = self.config.seed_ratio {
                    if target_ratio > 0.0 {
                        let uploaded = self.stats.uploaded.load(Ordering::Relaxed);
                        let downloaded = self.stats.downloaded.load(Ordering::Relaxed);

                        // Calculate current ratio (avoid division by zero)
                        let current_ratio = if downloaded > 0 {
                            uploaded as f64 / downloaded as f64
                        } else if uploaded > 0 {
                            f64::INFINITY
                        } else {
                            0.0
                        };

                        if current_ratio >= target_ratio {
                            tracing::info!(
                                "Seed ratio reached ({:.2} >= {:.2}), stopping torrent {}",
                                current_ratio,
                                target_ratio,
                                self.name()
                            );
                            *self.state.write() = TorrentState::Stopped;
                            // Announce stopped
                            let _ = self.announce_to_trackers(AnnounceEvent::Stopped).await;
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup: abort all connection tasks
        for (_, handle) in active_connections.write().drain() {
            handle.abort();
        }

        Ok(())
    }

    /// Run the choking algorithm to decide which peers to unchoke.
    fn run_choking_algorithm(&self) {
        let is_seeding = *self.state.read() == TorrentState::Seeding;
        let peer_stats = self.shared_peer_stats.read().clone();

        if peer_stats.is_empty() {
            return;
        }

        let decisions = self.choking_manager.write().recalculate(&peer_stats, is_seeding);

        if decisions.is_empty() {
            return;
        }

        // Update choking decisions map
        let mut choking_map = self.choking_decisions.write();
        for decision in decisions {
            match decision {
                ChokingDecision::Unchoke(addr) => {
                    tracing::debug!("Choking algorithm: unchoking {}", addr);
                    choking_map.insert(addr, true);
                }
                ChokingDecision::Choke(addr) => {
                    tracing::debug!("Choking algorithm: choking {}", addr);
                    choking_map.insert(addr, false);
                }
            }
        }
    }

    /// Connect to new peers from the known_peers list
    async fn connect_to_new_peers(
        self: &Arc<Self>,
        active_connections: Arc<RwLock<HashMap<SocketAddr, tokio::task::JoinHandle<()>>>>,
        max_pending_per_peer: usize,
    ) {
        const MAX_CONNECT_PER_ROUND: usize = 5;

        // Get peers we're not connected to
        let candidates: Vec<SocketAddr> = {
            let known = self.known_peers.read();
            let active = active_connections.read();
            known
                .iter()
                .filter(|addr| !active.contains_key(*addr))
                .take(MAX_CONNECT_PER_ROUND)
                .cloned()
                .collect()
        };

        let num_pieces = match self.metainfo.read().as_ref() {
            Some(metainfo) => metainfo.info.pieces.len(),
            None => {
                if self.metadata_fetcher.is_none() {
                    tracing::debug!("No metainfo available, skipping peer connections");
                    return;
                }
                0
            }
        };
        let peer_id = *self.tracker_client.peer_id();
        let info_hash = self.info_hash;

        for addr in candidates {
            // Check if we're at the connection limit
            let current_connections = active_connections.read().len();
            if current_connections >= self.config.max_peers {
                break;
            }

            let downloader = Arc::clone(self);
            let active_conns = Arc::clone(&active_connections);
            let shared_stats = Arc::clone(&self.shared_peer_stats);
            let choking_decisions = Arc::clone(&self.choking_decisions);

            let task = tokio::spawn(async move {
                match Self::run_single_peer_connection(
                    downloader,
                    addr,
                    info_hash,
                    peer_id,
                    num_pieces,
                    max_pending_per_peer,
                    shared_stats,
                    choking_decisions,
                ).await {
                    Ok(()) => {
                        tracing::debug!("Peer connection {} ended normally", addr);
                    }
                    Err(e) => {
                        tracing::debug!("Peer connection {} failed: {}", addr, e);
                    }
                }

                // Remove from active connections
                active_conns.write().remove(&addr);
            });

            active_connections.write().insert(addr, task);
        }
    }

    /// Run a connection to a single peer
    #[allow(clippy::too_many_arguments)]
    async fn run_single_peer_connection(
        downloader: Arc<Self>,
        addr: SocketAddr,
        info_hash: Sha1Hash,
        peer_id: [u8; 20],
        num_pieces: usize,
        max_pending: usize,
        shared_stats: Arc<RwLock<HashMap<SocketAddr, PeerStats>>>,
        choking_decisions: Arc<RwLock<HashMap<SocketAddr, bool>>>,
    ) -> Result<()> {
        // Connect to peer
        let metadata_only = downloader.metadata_fetcher.is_some() && num_pieces == 0;
        let mut conn = PeerConnection::connect(addr, info_hash, peer_id, num_pieces).await?;
        tracing::info!("Connected to peer {}", addr);

        downloader.stats.peers_connected.fetch_add(1, Ordering::Relaxed);

        // Send extension handshake if supported
        if conn.supports_extensions() {
            let metadata_id = downloader
                .metadata_fetcher
                .as_ref()
                .map(|_| OUR_METADATA_EXTENSION_ID);
            conn.send_extension_handshake(metadata_id, None).await.ok();
        }

        // Initialize PEX state for this connection
        let mut pex_state = if downloader.pex_enabled() {
            Some(PexState::new(OUR_PEX_EXTENSION_ID))
        } else {
            None
        };

        // Send our bitfield
        let bitfield_opt = {
            let pm_guard = downloader.piece_manager.read();
            pm_guard.as_ref().map(|pm| pm.bitfield())
        };
        if let Some(bitfield) = bitfield_opt {
            conn.send_bitfield(&bitfield).await.ok();
        }

        // Send interested
        conn.interested().await?;

        // Track pending requests
        let mut pending_requests: HashSet<(u32, u32, u32)> = HashSet::new();
        let mut last_stats_update = Instant::now();
        let mut last_downloaded: u64 = 0;
        let mut last_uploaded: u64 = 0;

        // Initialize peer stats entry
        {
            let mut stats = shared_stats.write();
            stats.insert(addr, PeerStats {
                addr,
                download_rate: 0,
                upload_rate: 0,
                peer_interested: conn.peer_interested(),
                am_interested: conn.am_interested(),
                is_unchoked: !conn.am_choking(),
                is_seeder: false,
            });
        }

        loop {
            // Check shutdown
            if downloader.shutdown.load(Ordering::SeqCst) {
                break;
            }

            // Check state
            let state = *downloader.state.read();
            if state == TorrentState::Paused || state == TorrentState::Stopped {
                break;
            }

            // Receive message (avoid canceling reads to prevent stream desync)
            match conn.recv().await {
                Ok(msg) => {
                    match msg {
                        PeerMessage::KeepAlive => {}

                        PeerMessage::Choke => {
                            // Peer choked us, clear pending
                            pending_requests.clear();
                        }

                        PeerMessage::Unchoke => {
                            // Can request pieces now
                        }

                        PeerMessage::Have { piece_index: _ } => {
                            // Peer has a new piece - already handled internally by conn
                        }

                        PeerMessage::Bitfield { .. } => {
                            // Already handled internally by conn
                        }

                        PeerMessage::Piece { index, begin, block } => {
                            // Remove from pending
                            pending_requests.remove(&(index, begin, block.len() as u32));

                            // Add block to piece manager
                            let add_result = {
                                let pm_guard = downloader.piece_manager.read();
                                if let Some(ref pm) = *pm_guard {
                                    Some((pm.add_block(index, begin, block.clone()), Arc::clone(pm)))
                                } else {
                                    None
                                }
                            };

                            if let Some((result, pm)) = add_result {
                                match result {
                                    Ok(complete) => {
                                        // Update stats
                                        downloader.stats.downloaded.fetch_add(
                                            block.len() as u64,
                                            Ordering::Relaxed
                                        );

                                        if complete {
                                            // Verify and save the piece (now pm is owned, not borrowed)
                                            match pm.verify_and_save(index).await {
                                                Ok(true) => {
                                                    tracing::debug!("Piece {} verified and saved", index);
                                                }
                                                Ok(false) => {
                                                    tracing::warn!("Piece {} failed verification", index);
                                                }
                                                Err(e) => {
                                                    tracing::error!("Error saving piece {}: {}", index, e);
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("Error adding block: {}", e);
                                    }
                                }
                            }
                        }

                        PeerMessage::Request { index, begin, length } => {
                            // Peer is requesting a block from us (for seeding)

                            // Check if we're choking this peer - ignore request if so
                            if conn.am_choking() {
                                tracing::trace!(
                                    "Ignoring request from {} - peer is choked (piece={}, offset={}, len={})",
                                    addr, index, begin, length
                                );
                                continue;
                            }

                            // Read the block from disk
                            // Clone the Arc to avoid holding the RwLock guard across await
                            let pm_opt = downloader.piece_manager.read().clone();
                            let block_result = match pm_opt {
                                Some(ref pm) => Some(pm.read_block(index, begin, length).await),
                                None => None,
                            };

                            match block_result {
                                Some(Ok(block)) => {
                                    // Send the piece to the peer
                                    match conn.send_piece(index, begin, block.clone()).await {
                                        Ok(()) => {
                                            // Update upload statistics
                                            downloader.stats.uploaded.fetch_add(
                                                block.len() as u64,
                                                Ordering::Relaxed
                                            );
                                            tracing::trace!(
                                                "Sent block to {}: piece={}, offset={}, len={}",
                                                addr, index, begin, length
                                            );
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "Failed to send piece to {}: {}",
                                                addr, e
                                            );
                                        }
                                    }
                                }
                                Some(Err(e)) => {
                                    // We don't have the piece or invalid request
                                    tracing::debug!(
                                        "Cannot serve request from {}: {} (piece={}, offset={}, len={})",
                                        addr, e, index, begin, length
                                    );
                                    // Per BitTorrent protocol, just ignore invalid requests
                                }
                                None => {
                                    tracing::debug!("No piece manager available to serve request from {}", addr);
                                }
                            }
                        }

                        PeerMessage::Extended { id, payload } => {
                            // Handle extension messages
                            if id == 0 {
                                // Extension handshake
                                if let Ok(handshake) = parse_extension_handshake(&payload) {
                                    if let Some(pex_id) = handshake.extensions.get(PEX_EXTENSION_NAME) {
                                        tracing::debug!("Peer {} supports PEX (id={})", addr, pex_id);
                                        // Update PEX state with peer's extension ID
                                        if let Some(ref mut state) = pex_state {
                                            state.set_peer_extension_id(*pex_id);
                                        }
                                    }
                                    // Check for ut_metadata support
                                    if let Some(metadata_id) = handshake.extensions.get(METADATA_EXTENSION_NAME) {
                                        tracing::debug!("Peer {} supports ut_metadata (id={})", addr, metadata_id);
                                        // If we need metadata, request it
                                        if let Some(ref fetcher) = downloader.metadata_fetcher {
                                            if !fetcher.is_complete().await {
                                                let needed = fetcher.get_needed_pieces().await;
                                                for piece in needed.into_iter().take(2) {
                                                    let msg = MetadataMessage::request(piece);
                                                    if conn.send_extension_message(*metadata_id, msg.encode()).await.is_ok() {
                                                        fetcher.mark_requested(piece).await;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else if id == OUR_PEX_EXTENSION_ID {
                                // PEX message
                                downloader.process_pex_message(&payload);
                            } else if id == OUR_METADATA_EXTENSION_ID {
                                // ut_metadata message
                                if let Some(ref fetcher) = downloader.metadata_fetcher {
                                    if let Ok(msg) = MetadataMessage::parse(&payload) {
                                        if let Ok(complete) = fetcher.process_message(msg).await {
                                            if complete {
                                                // Metadata complete! Initialize piece manager
                                                if let Ok(Some(metainfo)) = fetcher.parse_metainfo().await {
                                                    tracing::info!("Metadata received for {}", downloader.name());
                                                    let metainfo = Arc::new(metainfo);
                                                    let pm = Arc::new(PieceManager::new(
                                                        metainfo.clone(),
                                                        downloader.save_dir.clone()
                                                    ));
                                                    *downloader.metainfo.write() = Some(metainfo);
                                                    *downloader.piece_manager.write() = Some(pm);
                                                    *downloader.state.write() = TorrentState::Downloading;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        _ => {}
                    }
                }

                Err(e) => {
                    // Connection error
                    tracing::debug!("Peer {} recv error: {}", addr, e);
                    break;
                }
            }

            if metadata_only && downloader.metainfo.read().is_some() {
                tracing::debug!(
                    "Metadata available for {}, reconnecting peer {}",
                    downloader.name(),
                    addr
                );
                break;
            }

            // Request more blocks if we have capacity and peer is unchoked
            if !conn.peer_choking() && pending_requests.len() < max_pending {
                // Get blocks to request while holding the lock briefly
                let blocks_to_request: Vec<BlockRequest> = {
                    let pm_guard = downloader.piece_manager.read();
                    if let Some(ref pm) = *pm_guard {
                        // Check for endgame mode (10 or fewer pieces remaining)
                        let endgame_pieces = pm.endgame_pieces();
                        if !endgame_pieces.is_empty() {
                            let mut pending = pm.pending_pieces();
                            for piece_idx in endgame_pieces {
                                if !pending.contains(&piece_idx)
                                    && !pm.have_piece(piece_idx as usize)
                                    && pm.start_piece(piece_idx).is_some()
                                {
                                    pending.insert(piece_idx);
                                }
                            }

                            // In endgame mode: request all pending blocks that the peer has
                            // This allows requesting same blocks from multiple peers
                            pm.endgame_requests()
                                .into_iter()
                                .filter(|req| conn.peer_has_piece(req.piece as usize))
                                .take(max_pending - pending_requests.len())
                                .collect()
                        } else {
                            // Normal mode: use rarest-first piece selection
                            if let Some(piece_idx) = pm.select_piece(conn.peer_pieces()) {
                                if pm.start_piece(piece_idx).is_some() {
                                    pm.get_block_requests(piece_idx)
                                        .into_iter()
                                        .take(max_pending - pending_requests.len())
                                        .collect()
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    }
                };

                // Now send requests without holding the lock
                for block in blocks_to_request {
                    let key = (block.piece, block.offset, block.length);
                    // In endgame mode we allow requesting blocks we've already requested
                    // from other peers, but not from the same peer
                    if !pending_requests.contains(&key)
                        && conn.request_block(block.piece, block.offset, block.length).await.is_ok()
                    {
                        pending_requests.insert(key);
                    }
                }
            }

            // Send PEX message if enabled, supported, and interval elapsed
            if let Some(ref mut state) = pex_state {
                if state.is_supported() && state.can_send() {
                    // Get current known peers, excluding this peer
                    let current_peers: HashSet<SocketAddr> = {
                        let peers = downloader.known_peers.read();
                        peers.iter().filter(|p| **p != addr).cloned().collect()
                    };

                    if let Some(pex_msg) = state.build_message(&current_peers) {
                        if let Err(e) = conn.send_pex(&pex_msg).await {
                            tracing::debug!("Failed to send PEX to {}: {}", addr, e);
                        } else {
                            tracing::debug!("Sent PEX message to {} with {} added peers", addr, pex_msg.added.len());
                        }
                    }
                }
            }

            // Update peer stats periodically (every second)
            if last_stats_update.elapsed() >= Duration::from_secs(1) {
                let elapsed_secs = last_stats_update.elapsed().as_secs_f64();
                let current_downloaded = conn.downloaded();
                let current_uploaded = conn.uploaded();

                let download_rate = if elapsed_secs > 0.0 {
                    ((current_downloaded - last_downloaded) as f64 / elapsed_secs) as u64
                } else {
                    0
                };
                let upload_rate = if elapsed_secs > 0.0 {
                    ((current_uploaded - last_uploaded) as f64 / elapsed_secs) as u64
                } else {
                    0
                };

                last_downloaded = current_downloaded;
                last_uploaded = current_uploaded;
                last_stats_update = Instant::now();

                // Update shared stats for choking algorithm
                {
                    let mut stats = shared_stats.write();
                    if let Some(peer_stats) = stats.get_mut(&addr) {
                        peer_stats.download_rate = download_rate;
                        peer_stats.upload_rate = upload_rate;
                        peer_stats.peer_interested = conn.peer_interested();
                        peer_stats.am_interested = conn.am_interested();
                        peer_stats.is_unchoked = !conn.am_choking();
                    }
                }
            }

            // Check and apply choking decisions from the choking manager
            // Get the decision first, then drop the lock before awaiting
            let choking_decision: Option<bool> = {
                let decisions = choking_decisions.read();
                decisions.get(&addr).copied()
            };

            if let Some(should_unchoke) = choking_decision {
                let currently_unchoked = !conn.am_choking();
                if should_unchoke && !currently_unchoked {
                    // Need to unchoke this peer
                    if conn.unchoke().await.is_ok() {
                        tracing::debug!("Unchoked peer {}", addr);
                    }
                } else if !should_unchoke && currently_unchoked {
                    // Need to choke this peer
                    if conn.choke().await.is_ok() {
                        tracing::debug!("Choked peer {}", addr);
                    }
                }
            }

        }

        // Clean up peer stats on exit
        shared_stats.write().remove(&addr);
        choking_decisions.write().remove(&addr);

        downloader.stats.peers_connected.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    /// Run DHT peer discovery in the background.
    ///
    /// Periodically queries the DHT for peers and adds them to known_peers.
    pub async fn run_dht_discovery(self: Arc<Self>) -> Result<()> {
        if !self.dht_enabled() {
            tracing::debug!("DHT disabled for torrent {}", self.name());
            return Ok(());
        }

        let listen_port = self.config.listen_port_range.0;
        let bootstrap_nodes = &self.config.dht_bootstrap_nodes;

        // Use custom bootstrap nodes if configured, otherwise use defaults
        let dht_client = if bootstrap_nodes.is_empty() {
            match DhtClient::new(listen_port) {
                Ok(client) => Arc::new(client),
                Err(e) => {
                    tracing::warn!("Failed to create DHT client: {}", e);
                    return Ok(());
                }
            }
        } else {
            match DhtClient::with_bootstrap(listen_port, bootstrap_nodes) {
                Ok(client) => Arc::new(client),
                Err(e) => {
                    tracing::warn!("Failed to create DHT client with custom bootstrap: {}", e);
                    return Ok(());
                }
            }
        };

        const DHT_LOOKUP_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes
        const DHT_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1800); // 30 minutes

        let mut lookup_timer = tokio::time::interval(DHT_LOOKUP_INTERVAL);
        let mut announce_timer = tokio::time::interval(DHT_ANNOUNCE_INTERVAL);

        // Initial announce
        if let Err(e) = dht_client.announce(&self.info_hash) {
            tracing::debug!("DHT announce failed: {}", e);
        }

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }

            let state = *self.state.read();
            if state == TorrentState::Stopped {
                break;
            }

            tokio::select! {
                _ = lookup_timer.tick() => {
                    // Skip lookup if paused
                    if state == TorrentState::Paused {
                        continue;
                    }

                    // Find peers from DHT
                    let peers = dht_client.find_peers_timeout(&self.info_hash, Duration::from_secs(30)).await;
                    if !peers.is_empty() {
                        tracing::debug!("DHT found {} peers for {}", peers.len(), self.name());
                        self.add_known_peers(peers);
                    }
                }

                _ = announce_timer.tick() => {
                    // Re-announce to DHT
                    if let Err(e) = dht_client.announce(&self.info_hash) {
                        tracing::debug!("DHT re-announce failed: {}", e);
                    }
                }
            }
        }

        dht_client.shutdown();
        Ok(())
    }

    /// Run LPD (Local Peer Discovery) in the background.
    ///
    /// Announces to the local network and listens for other peers.
    pub async fn run_lpd_discovery(self: Arc<Self>) -> Result<()> {
        if !self.lpd_enabled() {
            tracing::debug!("LPD disabled for torrent {}", self.name());
            return Ok(());
        }

        let listen_port = self.config.listen_port_range.0;
        let lpd_service = match LpdService::new(listen_port).await {
            Ok(service) => Arc::new(service),
            Err(e) => {
                tracing::warn!("Failed to create LPD service: {}", e);
                return Ok(());
            }
        };

        // Track this torrent
        lpd_service.track(self.info_hash).await;

        // Start listening for announcements
        let mut lpd_rx = lpd_service.listen();

        const LPD_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes
        let mut announce_timer = tokio::time::interval(LPD_ANNOUNCE_INTERVAL);

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }

            let state = *self.state.read();
            if state == TorrentState::Stopped {
                break;
            }

            tokio::select! {
                result = lpd_rx.recv() => {
                    match result {
                        Ok(local_peer) => {
                            // Only add peers for our torrent
                            if local_peer.info_hash == self.info_hash {
                                tracing::debug!("LPD discovered peer {} for {}", local_peer.addr, self.name());
                                self.add_known_peers(std::iter::once(local_peer.addr));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Missed some messages, continue
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }

                _ = announce_timer.tick() => {
                    // Skip announce if paused
                    if state == TorrentState::Paused {
                        continue;
                    }

                    // Announce to local network
                    if let Err(e) = lpd_service.announce(&self.info_hash).await {
                        tracing::debug!("LPD announce failed: {}", e);
                    }
                }
            }
        }

        lpd_service.shutdown();
        Ok(())
    }

    /// Run periodic tracker re-announcements.
    pub async fn run_tracker_reannounce(self: Arc<Self>) -> Result<()> {
        // Default interval, may be overridden by tracker response
        let interval = if self.config.announce_interval > 0 {
            Duration::from_secs(self.config.announce_interval)
        } else {
            Duration::from_secs(1800) // 30 minutes default
        };

        let mut timer = tokio::time::interval(interval);
        // Skip first tick (we already announced on start)
        timer.tick().await;

        loop {
            timer.tick().await;

            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }

            let state = *self.state.read();
            if state == TorrentState::Stopped {
                break;
            }

            if state == TorrentState::Paused {
                continue;
            }

            // Re-announce to trackers
            if let Err(e) = self.announce_to_trackers(AnnounceEvent::None).await {
                tracing::debug!("Tracker re-announce failed: {}", e);
            }
        }

        Ok(())
    }

    /// Process a PEX (Peer Exchange) message from a peer.
    ///
    /// Returns new peers discovered from the message.
    pub fn process_pex_message(&self, payload: &[u8]) -> Vec<SocketAddr> {
        if !self.pex_enabled() {
            return vec![];
        }

        match PexMessage::parse(payload) {
            Ok(msg) => {
                let new_peers: Vec<SocketAddr> = {
                    let known = self.known_peers.read();
                    msg.all_added()
                        .into_iter()
                        .filter(|addr| !known.contains(addr))
                        .collect()
                };

                if !new_peers.is_empty() {
                    tracing::debug!("PEX received {} new peers for {}", new_peers.len(), self.name());
                    self.add_known_peers(new_peers.iter().cloned());
                }

                new_peers
            }
            Err(e) => {
                tracing::debug!("Failed to parse PEX message: {}", e);
                vec![]
            }
        }
    }

    /// Get the peer ID used for tracker announcements.
    pub fn peer_id(&self) -> [u8; 20] {
        *self.tracker_client.peer_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_torrent_config_default() {
        let config = TorrentConfig::default();
        assert_eq!(config.max_peers, 50);
        assert_eq!(config.listen_port_range, (6881, 6889));
        assert!(config.enable_dht);
    }

    #[test]
    fn test_torrent_state() {
        assert_ne!(TorrentState::Downloading, TorrentState::Seeding);
        assert_eq!(TorrentState::Paused, TorrentState::Paused);
    }
}
