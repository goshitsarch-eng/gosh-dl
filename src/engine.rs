//! Download Engine - Main coordinator
//!
//! The `DownloadEngine` is the primary entry point for the library.
//! It manages all downloads, coordinates between HTTP and BitTorrent
//! engines, handles persistence, and emits events.

use crate::config::EngineConfig;
use crate::error::{EngineError, Result};
#[cfg(feature = "http")]
use crate::http::{HttpDownloader, SegmentedDownload};
use crate::priority_queue::{DownloadPriority, PriorityQueue};
use crate::scheduler::{BandwidthLimits, BandwidthScheduler};
#[cfg(feature = "storage")]
use crate::storage::SqliteStorage;
use crate::storage::{Segment, Storage};
#[cfg(feature = "torrent")]
use crate::torrent::{MagnetUri, Metainfo, TorrentConfig, TorrentDownloader};
use crate::types::{
    DownloadEvent, DownloadId, DownloadOptions, DownloadState, DownloadStatus, GlobalStats,
};
#[cfg(any(feature = "http", feature = "torrent"))]
use crate::types::{DownloadKind, DownloadMetadata, DownloadProgress};
#[cfg(feature = "torrent")]
use crate::types::{TorrentFile, TorrentStatusInfo};

#[cfg(any(feature = "http", feature = "torrent"))]
use chrono::Utc;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
#[cfg(feature = "torrent")]
use std::time::Duration;
use tokio::sync::broadcast;
#[cfg(feature = "http")]
use url::Url;

/// Maximum number of events to buffer
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Internal representation of a managed download
struct ManagedDownload {
    status: DownloadStatus,
    handle: Option<DownloadHandle>,
    /// Cached HTTP segment state for in-memory pause/resume (no storage needed)
    #[cfg(feature = "http")]
    cached_segments: Option<Vec<Segment>>,
}

/// Handle to control a running download
#[allow(dead_code)]
enum DownloadHandle {
    #[cfg(feature = "http")]
    Http(HttpDownloadHandle),
    #[cfg(feature = "torrent")]
    Torrent(TorrentDownloadHandle),
}

/// Handle for an HTTP download task
#[cfg(feature = "http")]
struct HttpDownloadHandle {
    cancel_token: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<Result<()>>,
    /// Reference to segmented download for persistence (if using segmented download).
    /// Wrapped in RwLock so it can be populated from inside the spawned task.
    segmented_download: Arc<RwLock<Option<Arc<SegmentedDownload>>>>,
}

/// Handle for a torrent download
#[cfg(feature = "torrent")]
struct TorrentDownloadHandle {
    downloader: Arc<TorrentDownloader>,
    task: tokio::task::JoinHandle<Result<()>>,
    progress_task: tokio::task::JoinHandle<()>,
}

/// The main download engine
pub struct DownloadEngine {
    /// Weak self-reference for spawning background tasks from `&self` methods
    self_ref: Weak<Self>,

    /// Configuration
    config: RwLock<EngineConfig>,

    /// All managed downloads
    downloads: RwLock<HashMap<DownloadId, ManagedDownload>>,

    /// HTTP downloader
    #[cfg(feature = "http")]
    http: Arc<HttpDownloader>,

    /// Event broadcaster
    event_tx: broadcast::Sender<DownloadEvent>,

    /// Priority queue for limiting and ordering concurrent downloads
    priority_queue: Arc<PriorityQueue>,

    /// Bandwidth scheduler for time-based limits
    scheduler: Arc<RwLock<BandwidthScheduler>>,

    /// Shutdown flag
    shutdown: tokio_util::sync::CancellationToken,

    /// Persistent storage for download state
    storage: Option<Arc<dyn Storage>>,
}

impl DownloadEngine {
    /// Obtain a strong `Arc<Self>` reference for spawning background tasks.
    fn arc(&self) -> Result<Arc<Self>> {
        self.self_ref.upgrade().ok_or(EngineError::Shutdown)
    }

    /// Create a new download engine with the given configuration
    pub async fn new(config: EngineConfig) -> Result<Arc<Self>> {
        // Validate configuration
        config.validate()?;

        // Create event channel
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        // Create HTTP downloader
        #[cfg(feature = "http")]
        let http = Arc::new(HttpDownloader::new(&config)?);

        // Create priority queue for concurrent download limiting
        let priority_queue = PriorityQueue::new(config.max_concurrent_downloads);

        // Create bandwidth scheduler with configured rules
        let scheduler = Arc::new(RwLock::new(BandwidthScheduler::new(
            config.schedule_rules.clone(),
            BandwidthLimits {
                download: config.global_download_limit,
                upload: config.global_upload_limit,
            },
        )));

        // Initialize persistent storage
        #[cfg(feature = "storage")]
        let storage: Option<Arc<dyn Storage>> = if let Some(ref db_path) = config.database_path {
            match SqliteStorage::new(db_path).await {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    tracing::warn!("Failed to initialize database storage: {}. Downloads will not be persisted.", e);
                    None
                }
            }
        } else {
            None
        };
        #[cfg(not(feature = "storage"))]
        let storage: Option<Arc<dyn Storage>> = None;

        let engine = Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            config: RwLock::new(config),
            downloads: RwLock::new(HashMap::new()),
            #[cfg(feature = "http")]
            http,
            event_tx,
            priority_queue,
            scheduler,
            shutdown: tokio_util::sync::CancellationToken::new(),
            storage,
        });

        // Load persisted downloads from database
        engine.load_persisted_downloads().await?;

        // Start background persistence task
        Self::start_persistence_task(Arc::clone(&engine));

        // Start bandwidth scheduler update task
        Self::start_scheduler_task(Arc::clone(&engine));

        Ok(engine)
    }

    /// Start background task that periodically persists active download states.
    ///
    /// This ensures that if the process crashes, downloads can be resumed
    /// from approximately where they left off.
    fn start_persistence_task(engine: Arc<Self>) {
        if engine.storage.is_none() {
            return; // No storage configured
        }

        let shutdown = engine.shutdown.clone();
        tokio::spawn(async move {
            const PERSISTENCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
            let mut interval = tokio::time::interval(PERSISTENCE_INTERVAL);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(e) = engine.persist_active_downloads().await {
                            tracing::warn!("Failed to persist active downloads: {}", e);
                        }
                    }
                    _ = shutdown.cancelled() => {
                        // Final persistence on shutdown
                        if let Err(e) = engine.persist_active_downloads().await {
                            tracing::warn!("Failed to persist downloads on shutdown: {}", e);
                        }
                        break;
                    }
                }
            }
        });
    }

    /// Start background task that updates bandwidth limits based on schedule.
    ///
    /// This checks the schedule rules every minute and updates the current
    /// bandwidth limits if they have changed.
    fn start_scheduler_task(engine: Arc<Self>) {
        let shutdown = engine.shutdown.clone();
        tokio::spawn(async move {
            const SCHEDULER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
            let mut interval = tokio::time::interval(SCHEDULER_INTERVAL);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        engine.scheduler.read().update();
                    }
                    _ = shutdown.cancelled() => {
                        break;
                    }
                }
            }
        });
    }

    /// Persist all active (non-completed, non-error) downloads to storage.
    async fn persist_active_downloads(&self) -> Result<()> {
        let storage = match &self.storage {
            Some(s) => s,
            None => return Ok(()),
        };

        // Collect active downloads and their segment info
        let active_downloads: Vec<(DownloadStatus, Option<Vec<crate::storage::Segment>>)> = {
            let downloads = self.downloads.read();
            downloads
                .values()
                .filter(|d| d.status.state.is_active())
                .map(|d| {
                    let segments = match &d.handle {
                        #[cfg(feature = "http")]
                        Some(DownloadHandle::Http(h)) => h
                            .segmented_download
                            .read()
                            .as_ref()
                            .map(|sd| sd.segments_with_progress()),
                        _ => None,
                    };
                    (d.status.clone(), segments)
                })
                .collect()
        };

        // Save each active download and its segments
        for (status, segments_opt) in active_downloads {
            if let Err(e) = storage.save_download(&status).await {
                tracing::debug!("Failed to persist download {}: {}", status.id, e);
            }

            // Save segments if this is a segmented HTTP download
            if let Some(segments) = segments_opt {
                if let Err(e) = storage.save_segments(status.id, &segments).await {
                    tracing::debug!("Failed to persist segments for {}: {}", status.id, e);
                }
            }
        }

        Ok(())
    }

    /// Load persisted downloads from database on startup
    async fn load_persisted_downloads(&self) -> Result<()> {
        let storage = match &self.storage {
            Some(s) => s,
            None => return Ok(()), // No storage configured
        };

        let persisted = storage.load_all().await?;

        for status in persisted {
            // Skip completed downloads - they don't need resumption
            if matches!(status.state, DownloadState::Completed) {
                continue;
            }

            // For active/downloading states, mark as paused (crashed mid-download)
            let restored_state = match &status.state {
                DownloadState::Downloading | DownloadState::Connecting => DownloadState::Paused,
                DownloadState::Seeding => DownloadState::Paused, // Torrents that were seeding
                other => other.clone(),
            };

            let mut restored_status = status.clone();
            restored_status.state = restored_state;
            // Reset speeds (they're stale)
            restored_status.progress.download_speed = 0;
            restored_status.progress.upload_speed = 0;
            restored_status.progress.connections = 0;

            // Insert into downloads map
            self.downloads.write().insert(
                status.id,
                ManagedDownload {
                    status: restored_status,
                    handle: None,
                    #[cfg(feature = "http")]
                    cached_segments: None,
                },
            );

            tracing::info!(
                "Restored download {} ({}) in state {:?}",
                status.id,
                status.metadata.name,
                status.state
            );
        }

        Ok(())
    }

    #[cfg(feature = "torrent")]
    fn build_torrent_status_info(metainfo: &Metainfo) -> TorrentStatusInfo {
        TorrentStatusInfo {
            files: metainfo
                .info
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| TorrentFile {
                    index: i,
                    path: f.path.clone(),
                    size: f.length,
                    completed: 0,
                    selected: true,
                })
                .collect(),
            piece_length: metainfo.info.piece_length,
            pieces_count: metainfo.info.pieces.len(),
            private: metainfo.info.private,
        }
    }

    /// Add an HTTP/HTTPS download
    #[cfg(feature = "http")]
    pub async fn add_http(&self, url: &str, options: DownloadOptions) -> Result<DownloadId> {
        // Validate URL
        let parsed_url = Url::parse(url)
            .map_err(|e| EngineError::invalid_input("url", format!("Invalid URL: {}", e)))?;

        // Only allow http and https
        match parsed_url.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(EngineError::invalid_input(
                    "url",
                    format!("Unsupported scheme: {}", scheme),
                ));
            }
        }

        // Generate download ID
        let id = DownloadId::new();

        // Determine save directory
        let save_dir = options
            .save_dir
            .clone()
            .unwrap_or_else(|| self.config.read().download_dir.clone());

        // Extract filename from URL or options
        let filename = options.filename.clone().or_else(|| {
            parsed_url
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        });

        let name = filename.clone().unwrap_or_else(|| "download".to_string());

        // Create download status
        let status = DownloadStatus {
            id,
            kind: DownloadKind::Http,
            state: DownloadState::Queued,
            priority: options.priority,
            progress: DownloadProgress::default(),
            metadata: DownloadMetadata {
                name,
                url: Some(url.to_string()),
                magnet_uri: None,
                info_hash: None,
                save_dir,
                filename,
                user_agent: options.user_agent.clone(),
                referer: options.referer.clone(),
                headers: options.headers.clone(),
                cookies: options.cookies.clone().unwrap_or_default(),
                checksum: options.checksum.clone(),
                mirrors: options.mirrors.clone(),
                etag: None,
                last_modified: None,
            },
            torrent_info: None,
            peers: None,
            created_at: Utc::now(),
            completed_at: None,
        };

        // Insert into downloads map
        {
            let mut downloads = self.downloads.write();
            downloads.insert(
                id,
                ManagedDownload {
                    status: status.clone(),
                    handle: None,
                    #[cfg(feature = "http")]
                    cached_segments: None,
                },
            );
        }

        // Persist to database
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.save_download(&status).await {
                tracing::warn!("Failed to persist new download {}: {}", id, e);
            }
        }

        // Emit event
        let _ = self.event_tx.send(DownloadEvent::Added { id });

        // Start the download (no saved segments for new downloads)
        self.start_download(id, url.to_string(), options, None)
            .await?;

        Ok(id)
    }

    /// Add a BitTorrent download from torrent file data
    #[cfg(feature = "torrent")]
    pub async fn add_torrent(
        &self,
        torrent_data: &[u8],
        options: DownloadOptions,
    ) -> Result<DownloadId> {
        // Parse torrent file
        let metainfo = Metainfo::parse(torrent_data)?;

        // Generate download ID
        let id = DownloadId::new();

        // Determine save directory
        let save_dir = options
            .save_dir
            .clone()
            .unwrap_or_else(|| self.config.read().download_dir.clone());

        // Create download status
        let status = DownloadStatus {
            id,
            kind: DownloadKind::Torrent,
            state: DownloadState::Queued,
            priority: options.priority,
            progress: DownloadProgress::default(),
            metadata: DownloadMetadata {
                name: metainfo.info.name.clone(),
                url: None,
                magnet_uri: None,
                info_hash: Some(hex::encode(metainfo.info_hash)),
                save_dir: save_dir.clone(),
                filename: Some(metainfo.info.name.clone()),
                user_agent: options.user_agent.clone(),
                referer: None,
                headers: Vec::new(),
                cookies: Vec::new(),
                checksum: None,
                mirrors: Vec::new(),
                etag: None,
                last_modified: None,
            },
            torrent_info: Some(Self::build_torrent_status_info(&metainfo)),
            peers: Some(Vec::new()),
            created_at: Utc::now(),
            completed_at: None,
        };

        // Insert into downloads map
        {
            let mut downloads = self.downloads.write();
            downloads.insert(
                id,
                ManagedDownload {
                    status: status.clone(),
                    handle: None,
                    #[cfg(feature = "http")]
                    cached_segments: None,
                },
            );
        }

        // Persist to database
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.save_download(&status).await {
                tracing::warn!("Failed to persist new torrent download {}: {}", id, e);
            }
            // Save raw torrent data for crash recovery
            if let Err(e) = storage.save_torrent_data(id, torrent_data).await {
                tracing::warn!("Failed to persist torrent data for {}: {}", id, e);
            }
        }

        // Emit event
        let _ = self.event_tx.send(DownloadEvent::Added { id });

        // Start the torrent download
        self.start_torrent(id, metainfo, save_dir, options).await?;

        Ok(id)
    }

    /// Add a BitTorrent download from a magnet URI
    #[cfg(feature = "torrent")]
    pub async fn add_magnet(
        &self,
        magnet_uri: &str,
        options: DownloadOptions,
    ) -> Result<DownloadId> {
        // Parse magnet URI
        let magnet = MagnetUri::parse(magnet_uri)?;

        // Generate download ID
        let id = DownloadId::new();

        // Determine save directory
        let save_dir = options
            .save_dir
            .clone()
            .unwrap_or_else(|| self.config.read().download_dir.clone());

        // Create download status
        let status = DownloadStatus {
            id,
            kind: DownloadKind::Magnet,
            state: DownloadState::Queued,
            priority: options.priority,
            progress: DownloadProgress::default(),
            metadata: DownloadMetadata {
                name: magnet.name(),
                url: None,
                magnet_uri: Some(magnet_uri.to_string()),
                info_hash: Some(hex::encode(magnet.info_hash)),
                save_dir: save_dir.clone(),
                filename: magnet.display_name.clone(),
                user_agent: options.user_agent.clone(),
                referer: None,
                headers: Vec::new(),
                cookies: Vec::new(),
                checksum: None,
                mirrors: Vec::new(),
                etag: None,
                last_modified: None,
            },
            torrent_info: None, // Will be populated when metadata is received
            peers: Some(Vec::new()),
            created_at: Utc::now(),
            completed_at: None,
        };

        // Insert into downloads map
        {
            let mut downloads = self.downloads.write();
            downloads.insert(
                id,
                ManagedDownload {
                    status: status.clone(),
                    handle: None,
                    #[cfg(feature = "http")]
                    cached_segments: None,
                },
            );
        }

        // Persist to database
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.save_download(&status).await {
                tracing::warn!("Failed to persist new magnet download {}: {}", id, e);
            }
        }

        // Emit event
        let _ = self.event_tx.send(DownloadEvent::Added { id });

        // Start the magnet download
        self.start_magnet(id, magnet, save_dir, options).await?;

        Ok(id)
    }

    /// Start a torrent download task
    #[cfg(feature = "torrent")]
    async fn start_torrent(
        &self,
        id: DownloadId,
        metainfo: Metainfo,
        save_dir: std::path::PathBuf,
        options: DownloadOptions,
    ) -> Result<()> {
        let config = self.config.read();
        let torrent_config = TorrentConfig {
            max_peers: config.max_peers,
            enable_dht: config.enable_dht,
            enable_pex: config.enable_pex,
            enable_lpd: config.enable_lpd,
            seed_ratio: Some(config.seed_ratio),
            max_upload_speed: config.global_upload_limit.unwrap_or(0),
            max_download_speed: config.global_download_limit.unwrap_or(0),
            ..TorrentConfig::default()
        };
        drop(config);

        let downloader = Arc::new(TorrentDownloader::from_torrent(
            id,
            metainfo,
            save_dir,
            torrent_config,
            self.event_tx.clone(),
        )?);

        // Apply selected files for partial download
        if let Some(ref selected) = options.selected_files {
            downloader.set_selected_files(Some(selected));
        }

        // Apply sequential download mode if requested
        if let Some(sequential) = options.sequential {
            downloader.set_sequential(sequential);
        }

        let downloader_clone = Arc::clone(&downloader);
        let engine = self.arc()?;

        // Update state
        self.update_state(id, DownloadState::Connecting)?;

        let task = tokio::spawn(async move {
            // Start the download (announces to trackers, verifies existing pieces)
            if let Err(e) = Arc::clone(&downloader_clone).start().await {
                let error_msg = e.to_string();
                engine.update_state(
                    id,
                    DownloadState::Error {
                        kind: format!("{:?}", e),
                        message: error_msg.clone(),
                        retryable: e.is_retryable(),
                    },
                )?;
                let _ = engine.event_tx.send(DownloadEvent::Failed {
                    id,
                    error: error_msg,
                    retryable: e.is_retryable(),
                });
                return Ok(());
            }

            // Update state to downloading
            engine.update_state(id, DownloadState::Downloading)?;
            let _ = engine.event_tx.send(DownloadEvent::Started { id });

            // Run the peer connection loop
            let downloader_ref = Arc::clone(&downloader_clone);
            if let Err(e) = downloader_clone.run_peer_loop().await {
                let error_msg = e.to_string();
                engine.update_state(
                    id,
                    DownloadState::Error {
                        kind: format!("{:?}", e),
                        message: error_msg.clone(),
                        retryable: e.is_retryable(),
                    },
                )?;
                let _ = engine.event_tx.send(DownloadEvent::Failed {
                    id,
                    error: error_msg,
                    retryable: e.is_retryable(),
                });
            } else if downloader_ref.is_complete() {
                // Torrent completed successfully
                let should_complete = {
                    let mut downloads = engine.downloads.write();
                    if let Some(download) = downloads.get_mut(&id) {
                        if download.status.state == DownloadState::Paused {
                            false
                        } else {
                            download.status.state = DownloadState::Completed;
                            download.status.completed_at = Some(Utc::now());
                            true
                        }
                    } else {
                        false
                    }
                };

                if should_complete {
                    let _ = engine.event_tx.send(DownloadEvent::Completed { id });
                }
            }

            Ok(())
        });

        let progress_task =
            Self::spawn_torrent_progress_task(self.arc()?, id, Arc::clone(&downloader));

        // Store the handle
        {
            let mut downloads = self.downloads.write();
            if let Some(download) = downloads.get_mut(&id) {
                download.handle = Some(DownloadHandle::Torrent(TorrentDownloadHandle {
                    downloader,
                    task,
                    progress_task,
                }));
            }
        }

        Ok(())
    }

    /// Start a magnet download task
    #[cfg(feature = "torrent")]
    async fn start_magnet(
        &self,
        id: DownloadId,
        magnet: MagnetUri,
        save_dir: std::path::PathBuf,
        options: DownloadOptions,
    ) -> Result<()> {
        let config = self.config.read();
        let torrent_config = TorrentConfig {
            max_peers: config.max_peers,
            enable_dht: config.enable_dht,
            enable_pex: config.enable_pex,
            enable_lpd: config.enable_lpd,
            seed_ratio: Some(config.seed_ratio),
            max_upload_speed: config.global_upload_limit.unwrap_or(0),
            max_download_speed: config.global_download_limit.unwrap_or(0),
            ..TorrentConfig::default()
        };
        drop(config);

        let downloader = Arc::new(TorrentDownloader::from_magnet(
            id,
            magnet,
            save_dir,
            torrent_config,
            self.event_tx.clone(),
        )?);

        // Apply sequential download mode if requested
        if let Some(sequential) = options.sequential {
            downloader.set_sequential(sequential);
        }

        let downloader_clone = Arc::clone(&downloader);
        let engine = self.arc()?;

        // Update state
        self.update_state(id, DownloadState::Connecting)?;

        let task = tokio::spawn(async move {
            // Start the download (announces to trackers)
            if let Err(e) = Arc::clone(&downloader_clone).start().await {
                let error_msg = e.to_string();
                engine.update_state(
                    id,
                    DownloadState::Error {
                        kind: format!("{:?}", e),
                        message: error_msg.clone(),
                        retryable: e.is_retryable(),
                    },
                )?;
                let _ = engine.event_tx.send(DownloadEvent::Failed {
                    id,
                    error: error_msg,
                    retryable: e.is_retryable(),
                });
                return Ok(());
            }

            // Update state - for magnets, we're initially fetching metadata
            engine.update_state(id, DownloadState::Downloading)?;
            let _ = engine.event_tx.send(DownloadEvent::Started { id });

            // Run the peer connection loop (handles both downloading and metadata fetching for magnets)
            let downloader_ref = Arc::clone(&downloader_clone);
            if let Err(e) = downloader_clone.run_peer_loop().await {
                let error_msg = e.to_string();
                engine.update_state(
                    id,
                    DownloadState::Error {
                        kind: format!("{:?}", e),
                        message: error_msg.clone(),
                        retryable: e.is_retryable(),
                    },
                )?;
                let _ = engine.event_tx.send(DownloadEvent::Failed {
                    id,
                    error: error_msg,
                    retryable: e.is_retryable(),
                });
            } else if downloader_ref.is_complete() {
                // Magnet download completed successfully
                let should_complete = {
                    let mut downloads = engine.downloads.write();
                    if let Some(download) = downloads.get_mut(&id) {
                        if download.status.state == DownloadState::Paused {
                            false
                        } else {
                            download.status.state = DownloadState::Completed;
                            download.status.completed_at = Some(Utc::now());
                            true
                        }
                    } else {
                        false
                    }
                };

                if should_complete {
                    let _ = engine.event_tx.send(DownloadEvent::Completed { id });
                }
            }

            Ok(())
        });

        let progress_task =
            Self::spawn_torrent_progress_task(self.arc()?, id, Arc::clone(&downloader));

        // Store the handle
        {
            let mut downloads = self.downloads.write();
            if let Some(download) = downloads.get_mut(&id) {
                download.handle = Some(DownloadHandle::Torrent(TorrentDownloadHandle {
                    downloader,
                    task,
                    progress_task,
                }));
            }
        }

        Ok(())
    }

    #[cfg(feature = "torrent")]
    fn spawn_torrent_progress_task(
        engine: Arc<Self>,
        id: DownloadId,
        downloader: Arc<TorrentDownloader>,
    ) -> tokio::task::JoinHandle<()> {
        let shutdown = engine.shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {}
                }

                let progress = downloader.progress();
                let metainfo = downloader.metainfo();
                let (send_progress, persist_torrent_data) = {
                    let mut downloads = engine.downloads.write();
                    let download = match downloads.get_mut(&id) {
                        Some(download) => download,
                        None => break,
                    };

                    if matches!(
                        download.status.state,
                        DownloadState::Error { .. } | DownloadState::Completed
                    ) {
                        break;
                    }

                    let mut needs_persist = false;
                    if let Some(ref metainfo) = metainfo {
                        if download.status.torrent_info.is_none() {
                            download.status.torrent_info =
                                Some(Self::build_torrent_status_info(metainfo));
                            // Magnet metadata just arrived — persist torrent data
                            if download.status.kind == DownloadKind::Magnet {
                                needs_persist = true;
                            }
                        }
                        if download.status.metadata.name != metainfo.info.name {
                            download.status.metadata.name = metainfo.info.name.clone();
                        }
                        if download.status.metadata.filename.as_deref()
                            != Some(metainfo.info.name.as_str())
                        {
                            download.status.metadata.filename = Some(metainfo.info.name.clone());
                        }
                    }

                    download.status.progress = progress.clone();
                    (download.status.state.is_active(), needs_persist)
                };

                // Persist torrent data for magnet crash recovery (outside the lock)
                if persist_torrent_data {
                    if let Some(ref storage) = engine.storage {
                        if let Some(raw_data) = downloader.raw_torrent_data() {
                            if let Err(e) = storage.save_torrent_data(id, &raw_data).await {
                                tracing::warn!(
                                    "Failed to persist magnet torrent data for {}: {}",
                                    id,
                                    e
                                );
                            }
                        }
                    }
                }

                if send_progress {
                    let _ = engine
                        .event_tx
                        .send(DownloadEvent::Progress { id, progress });
                }
            }
        })
    }

    /// Start a download task
    #[cfg(feature = "http")]
    async fn start_download(
        &self,
        id: DownloadId,
        url: String,
        options: DownloadOptions,
        saved_segments: Option<Vec<crate::storage::Segment>>,
    ) -> Result<()> {
        let engine = self.arc()?;
        let http = Arc::clone(&self.http);
        let priority_queue = Arc::clone(&self.priority_queue);
        let priority = options.priority;
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let cancel_token_clone = cancel_token.clone();

        // Create shared reference for segmented download (populated by download_segmented)
        let segmented_ref: Arc<RwLock<Option<Arc<SegmentedDownload>>>> =
            Arc::new(RwLock::new(None));
        let segmented_ref_for_task = Arc::clone(&segmented_ref);

        // Update state to connecting
        self.update_state(id, DownloadState::Queued)?;

        let task = tokio::spawn(async move {
            // Acquire priority queue permit for concurrent limit
            let _permit = priority_queue.acquire(id, priority).await;

            // Check if cancelled before starting
            if cancel_token_clone.is_cancelled() {
                return Ok(());
            }

            // Update state to connecting then downloading
            engine.update_state(id, DownloadState::Connecting)?;
            engine.update_state(id, DownloadState::Downloading)?;
            let _ = engine.event_tx.send(DownloadEvent::Started { id });

            // Get save path and options
            let (save_dir, filename, user_agent, referer, headers, cookies, checksum) = {
                let downloads = engine.downloads.read();
                let download = downloads
                    .get(&id)
                    .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
                (
                    download.status.metadata.save_dir.clone(),
                    download.status.metadata.filename.clone(),
                    download.status.metadata.user_agent.clone(),
                    download.status.metadata.referer.clone(),
                    download.status.metadata.headers.clone(),
                    download.status.metadata.cookies.clone(),
                    download.status.metadata.checksum.clone(),
                )
            };

            // Create progress callback
            let engine_clone = Arc::clone(&engine);
            let progress_callback = move |progress: DownloadProgress| {
                // Update progress in download status
                {
                    let mut downloads = engine_clone.downloads.write();
                    if let Some(download) = downloads.get_mut(&id) {
                        download.status.progress = progress.clone();
                    }
                }
                // Emit progress event
                let _ = engine_clone
                    .event_tx
                    .send(DownloadEvent::Progress { id, progress });
            };

            // Get config for segmented downloads
            let (max_connections, min_segment_size) = {
                let config = engine.config.read();
                (config.max_connections_per_download, config.min_segment_size)
            };

            // Perform the download (uses segmented if server supports it)
            let cookies_opt = if cookies.is_empty() {
                None
            } else {
                Some(cookies.as_slice())
            };
            let result = http
                .download_segmented(
                    &url,
                    &save_dir,
                    filename.as_deref(),
                    user_agent.as_deref(),
                    referer.as_deref(),
                    &headers,
                    cookies_opt,
                    checksum.as_ref(),
                    max_connections,
                    min_segment_size,
                    cancel_token_clone.clone(),
                    saved_segments,
                    progress_callback,
                    Some(segmented_ref_for_task),
                )
                .await;

            match result {
                Ok((final_path, _segmented_download)) => {
                    // Update status to completed (but not if paused - race condition)
                    let should_complete = {
                        let mut downloads = engine.downloads.write();
                        if let Some(download) = downloads.get_mut(&id) {
                            // Don't overwrite Paused state - user paused before completion
                            if download.status.state == DownloadState::Paused {
                                false
                            } else {
                                download.status.state = DownloadState::Completed;
                                download.status.completed_at = Some(Utc::now());
                                download.status.metadata.filename = final_path
                                    .file_name()
                                    .map(|s| s.to_string_lossy().to_string());
                                true
                            }
                        } else {
                            false
                        }
                    };

                    if should_complete {
                        // Clean up saved segments from storage
                        if let Some(ref storage) = engine.storage {
                            if let Err(e) = storage.delete_segments(id).await {
                                tracing::debug!("Failed to clean up segments for {}: {}", id, e);
                            }
                        }

                        let _ = engine.event_tx.send(DownloadEvent::Completed { id });
                    }
                }
                Err(e) if cancel_token_clone.is_cancelled() => {
                    // Cancelled, already handled
                    let _ = e;
                }
                Err(e) => {
                    let retryable = e.is_retryable();
                    let error_msg = e.to_string();

                    // Update status to error
                    engine.update_state(
                        id,
                        DownloadState::Error {
                            kind: format!("{:?}", e),
                            message: error_msg.clone(),
                            retryable,
                        },
                    )?;

                    let _ = engine.event_tx.send(DownloadEvent::Failed {
                        id,
                        error: error_msg,
                        retryable,
                    });
                }
            }

            Ok(())
        });

        // Store the handle
        {
            let mut downloads = self.downloads.write();
            if let Some(download) = downloads.get_mut(&id) {
                download.handle = Some(DownloadHandle::Http(HttpDownloadHandle {
                    cancel_token,
                    task,
                    segmented_download: segmented_ref,
                }));
            }
        }

        Ok(())
    }

    /// Pause a download
    pub async fn pause(&self, id: DownloadId) -> Result<()> {
        let (status_to_save, segments_to_save) = {
            let mut downloads = self.downloads.write();
            let download = downloads
                .get_mut(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

            // Check if can be paused
            if !download.status.state.is_active() {
                return Err(EngineError::InvalidState {
                    action: "pause",
                    current_state: format!("{:?}", download.status.state),
                });
            }

            // Extract segments before taking the handle (for HTTP resume)
            let segments: Option<Vec<Segment>> = match &download.handle {
                #[cfg(feature = "http")]
                Some(DownloadHandle::Http(h)) => h
                    .segmented_download
                    .read()
                    .as_ref()
                    .map(|sd| sd.segments_with_progress()),
                _ => None,
            };

            // Cancel the task
            if let Some(handle) = download.handle.take() {
                match handle {
                    #[cfg(feature = "http")]
                    DownloadHandle::Http(h) => {
                        h.cancel_token.cancel();
                        // Don't await the task here to avoid blocking
                    }
                    #[cfg(feature = "torrent")]
                    DownloadHandle::Torrent(h) => {
                        h.downloader.pause();
                        download.handle = Some(DownloadHandle::Torrent(h));
                        // Don't await the task
                    }
                }
            }

            // Update state
            let old_state = download.status.state.clone();
            download.status.state = DownloadState::Paused;

            // Emit events
            let _ = self.event_tx.send(DownloadEvent::StateChanged {
                id,
                old_state,
                new_state: DownloadState::Paused,
            });
            let _ = self.event_tx.send(DownloadEvent::Paused { id });

            // Cache segments in memory for storage-less pause/resume
            #[cfg(feature = "http")]
            {
                download.cached_segments = segments.clone();
            }

            (download.status.clone(), segments)
        };

        // Persist to database
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.save_download(&status_to_save).await {
                tracing::warn!("Failed to persist paused download {}: {}", id, e);
            }
            // Save HTTP segments for resume
            if let Some(segments) = segments_to_save {
                if let Err(e) = storage.save_segments(id, &segments).await {
                    tracing::warn!(
                        "Failed to persist segments for paused download {}: {}",
                        id,
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// Resume a paused download
    pub async fn resume(&self, id: DownloadId) -> Result<()> {
        // Get download info and determine type
        #[allow(unused_variables)]
        let (kind, url, options, has_torrent_handle) = {
            let downloads = self.downloads.read();
            let download = downloads
                .get(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

            // Check if can be resumed
            if download.status.state != DownloadState::Paused {
                return Err(EngineError::InvalidState {
                    action: "resume",
                    current_state: format!("{:?}", download.status.state),
                });
            }

            #[cfg(feature = "torrent")]
            let has_torrent_handle = matches!(download.handle, Some(DownloadHandle::Torrent(_)));
            #[cfg(not(feature = "torrent"))]
            let has_torrent_handle = false;

            let options = DownloadOptions {
                save_dir: Some(download.status.metadata.save_dir.clone()),
                filename: download.status.metadata.filename.clone(),
                user_agent: download.status.metadata.user_agent.clone(),
                referer: download.status.metadata.referer.clone(),
                headers: download.status.metadata.headers.clone(),
                cookies: if download.status.metadata.cookies.is_empty() {
                    None
                } else {
                    Some(download.status.metadata.cookies.clone())
                },
                ..Default::default()
            };

            (
                download.status.kind,
                download.status.metadata.url.clone(),
                options,
                has_torrent_handle,
            )
        };

        #[allow(unreachable_code)]
        {
            match kind {
                #[cfg(feature = "http")]
                DownloadKind::Http => {
                    // HTTP: restart download with saved segments
                    let url = url.ok_or_else(|| {
                        EngineError::Internal("HTTP download missing URL".to_string())
                    })?;

                    // Load saved segments from storage if available
                    let mut saved_segments = if let Some(ref storage) = self.storage {
                        match storage.load_segments(id).await {
                            Ok(segments) if !segments.is_empty() => {
                                tracing::debug!(
                                    "Loaded {} saved segments for download {}",
                                    segments.len(),
                                    id
                                );
                                Some(segments)
                            }
                            Ok(_) => None,
                            Err(e) => {
                                tracing::debug!("Failed to load segments for {}: {}", id, e);
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Fall back to in-memory cached segments (for storage-less pause/resume)
                    if saved_segments.is_none() {
                        let mut downloads = self.downloads.write();
                        if let Some(download) = downloads.get_mut(&id) {
                            saved_segments = download.cached_segments.take();
                            if saved_segments.is_some() {
                                tracing::debug!(
                                    "Using cached segments for download {}",
                                    id
                                );
                            }
                        }
                    }

                    self.start_download(id, url, options, saved_segments)
                        .await?;
                }
                #[cfg(feature = "torrent")]
                DownloadKind::Torrent | DownloadKind::Magnet => {
                    if has_torrent_handle {
                        // Live handle exists — just unpause
                        let mut downloads = self.downloads.write();
                        if let Some(download) = downloads.get_mut(&id) {
                            if let Some(DownloadHandle::Torrent(ref h)) = download.handle {
                                h.downloader.resume();
                                download.status.state = DownloadState::Downloading;
                            }
                        }
                    } else {
                        // No live handle (crash recovery) — try reconstructing from stored data
                        let torrent_data = if let Some(ref storage) = self.storage {
                            storage.load_torrent_data(id).await.unwrap_or(None)
                        } else {
                            None
                        };

                        if let Some(data) = torrent_data {
                            let metainfo = Metainfo::parse(&data)?;
                            let save_dir = {
                                let downloads = self.downloads.read();
                                downloads
                                    .get(&id)
                                    .map(|d| d.status.metadata.save_dir.clone())
                                    .unwrap_or_else(|| self.config.read().download_dir.clone())
                            };
                            self.start_torrent(id, metainfo, save_dir, options).await?;
                        } else if let Some(ref magnet_uri) = {
                            let downloads = self.downloads.read();
                            downloads
                                .get(&id)
                                .and_then(|d| d.status.metadata.magnet_uri.clone())
                        } {
                            // Fall back to magnet URI (will re-fetch metadata from peers)
                            let magnet = MagnetUri::parse(magnet_uri)?;
                            let save_dir = {
                                let downloads = self.downloads.read();
                                downloads
                                    .get(&id)
                                    .map(|d| d.status.metadata.save_dir.clone())
                                    .unwrap_or_else(|| self.config.read().download_dir.clone())
                            };
                            self.start_magnet(id, magnet, save_dir, options).await?;
                        } else {
                            return Err(EngineError::Internal(
                                "Torrent download has no handle and no stored data for recovery"
                                    .to_string(),
                            ));
                        }
                    }
                }
                #[allow(unreachable_patterns)]
                _ => {
                    return Err(EngineError::Internal(format!(
                        "Feature not enabled for download kind {:?}",
                        kind
                    )));
                }
            }

            let _ = self.event_tx.send(DownloadEvent::Resumed { id });
        }

        Ok(())
    }

    /// Cancel a download and optionally delete files
    pub async fn cancel(&self, id: DownloadId, delete_files: bool) -> Result<()> {
        let (handle, save_path) = {
            let mut downloads = self.downloads.write();
            let download = downloads
                .remove(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

            let save_path = if delete_files {
                Some(
                    download.status.metadata.save_dir.join(
                        download
                            .status
                            .metadata
                            .filename
                            .as_deref()
                            .unwrap_or("download"),
                    ),
                )
            } else {
                None
            };

            (download.handle, save_path)
        };

        // Cancel the task if running
        if let Some(handle) = handle {
            match handle {
                #[cfg(feature = "http")]
                DownloadHandle::Http(h) => {
                    h.cancel_token.cancel();
                }
                #[cfg(feature = "torrent")]
                DownloadHandle::Torrent(h) => {
                    drop(h.downloader.stop());
                    h.progress_task.abort();
                    h.task.abort();
                }
            }
        }

        // Delete files and segments if requested
        if let Some(path) = save_path {
            if path.exists() {
                if path.is_dir() {
                    // Multi-file torrent: remove entire directory
                    tokio::fs::remove_dir_all(&path).await.ok();
                } else {
                    // Single file: remove the file
                    tokio::fs::remove_file(&path).await.ok();
                }
            }
            // Also try to remove partial file
            let partial_path = path.with_extension("part");
            if partial_path.exists() {
                tokio::fs::remove_file(&partial_path).await.ok();
            }
        }

        // Clean up saved segments and download record from storage
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.delete_segments(id).await {
                tracing::debug!(
                    "Failed to clean up segments for cancelled download {}: {}",
                    id,
                    e
                );
            }
            if let Err(e) = storage.delete_download(id).await {
                tracing::debug!("Failed to delete download record for {}: {}", id, e);
            }
        }

        let _ = self.event_tx.send(DownloadEvent::Removed { id });

        Ok(())
    }

    /// Get the status of a download
    pub fn status(&self, id: DownloadId) -> Option<DownloadStatus> {
        self.downloads.read().get(&id).map(|d| d.status.clone())
    }

    /// List all downloads
    pub fn list(&self) -> Vec<DownloadStatus> {
        self.downloads
            .read()
            .values()
            .map(|d| d.status.clone())
            .collect()
    }

    /// Get active downloads
    pub fn active(&self) -> Vec<DownloadStatus> {
        self.downloads
            .read()
            .values()
            .filter(|d| d.status.state.is_active())
            .map(|d| d.status.clone())
            .collect()
    }

    /// Get waiting/queued downloads
    pub fn waiting(&self) -> Vec<DownloadStatus> {
        self.downloads
            .read()
            .values()
            .filter(|d| matches!(d.status.state, DownloadState::Queued))
            .map(|d| d.status.clone())
            .collect()
    }

    /// Get stopped downloads (paused, completed, error)
    pub fn stopped(&self) -> Vec<DownloadStatus> {
        self.downloads
            .read()
            .values()
            .filter(|d| {
                matches!(
                    d.status.state,
                    DownloadState::Paused | DownloadState::Completed | DownloadState::Error { .. }
                )
            })
            .map(|d| d.status.clone())
            .collect()
    }

    /// Get global statistics
    pub fn global_stats(&self) -> GlobalStats {
        let downloads = self.downloads.read();
        let mut stats = GlobalStats::default();

        for download in downloads.values() {
            match &download.status.state {
                DownloadState::Downloading | DownloadState::Seeding | DownloadState::Connecting => {
                    stats.num_active += 1;
                    stats.download_speed += download.status.progress.download_speed;
                    stats.upload_speed += download.status.progress.upload_speed;
                }
                DownloadState::Queued => {
                    stats.num_waiting += 1;
                }
                DownloadState::Paused | DownloadState::Completed | DownloadState::Error { .. } => {
                    stats.num_stopped += 1;
                }
            }
        }

        stats
    }

    /// Subscribe to download events
    pub fn subscribe(&self) -> broadcast::Receiver<DownloadEvent> {
        self.event_tx.subscribe()
    }

    /// Update engine configuration
    pub fn set_config(&self, config: EngineConfig) -> Result<()> {
        config.validate()?;

        // Update concurrent download limit
        // Note: This doesn't affect currently running downloads

        *self.config.write() = config;
        Ok(())
    }

    /// Get current configuration
    pub fn get_config(&self) -> EngineConfig {
        self.config.read().clone()
    }

    /// Set the priority of a download
    ///
    /// This affects the order in which downloads acquire slots when queued.
    /// If the download is already active, the priority is updated but
    /// won't affect scheduling until the download is paused and resumed.
    ///
    /// The priority change is persisted to storage immediately (non-blocking).
    pub fn set_priority(&self, id: DownloadId, priority: DownloadPriority) -> Result<()> {
        // Update in downloads map and get status for persistence
        let status_to_save = {
            let mut downloads = self.downloads.write();
            let download = downloads
                .get_mut(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
            download.status.priority = priority;
            download.status.clone()
        };

        // Update in priority queue (affects scheduling if waiting)
        self.priority_queue.set_priority(id, priority);

        // Persist in background (fire-and-forget)
        if let Some(storage) = self.storage.as_ref().map(Arc::clone) {
            tokio::spawn(async move {
                if let Err(e) = storage.save_download(&status_to_save).await {
                    tracing::debug!("Failed to persist priority change for {}: {}", id, e);
                }
            });
        }

        Ok(())
    }

    /// Get the current priority of a download
    pub fn get_priority(&self, id: DownloadId) -> Option<DownloadPriority> {
        self.downloads.read().get(&id).map(|d| d.status.priority)
    }

    /// Get current bandwidth limits (accounting for schedule rules)
    pub fn get_bandwidth_limits(&self) -> BandwidthLimits {
        self.scheduler.read().get_limits()
    }

    /// Update the bandwidth schedule rules
    ///
    /// The new rules take effect immediately after evaluation.
    pub fn set_schedule_rules(&self, rules: Vec<crate::scheduler::ScheduleRule>) {
        self.scheduler.write().set_rules(rules);
    }

    /// Get the current schedule rules
    pub fn get_schedule_rules(&self) -> Vec<crate::scheduler::ScheduleRule> {
        self.scheduler.read().rules().to_vec()
    }

    /// Graceful shutdown
    pub async fn shutdown(&self) -> Result<()> {
        // Signal shutdown
        self.shutdown.cancel();

        // Cancel all active downloads
        let handles: Vec<_> = {
            let mut downloads = self.downloads.write();
            downloads
                .values_mut()
                .filter_map(|d| d.handle.take())
                .collect()
        };

        for handle in handles {
            match handle {
                #[cfg(feature = "http")]
                DownloadHandle::Http(h) => {
                    h.cancel_token.cancel();
                    // Wait for task to finish (with timeout)
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h.task).await;
                }
                #[cfg(feature = "torrent")]
                DownloadHandle::Torrent(h) => {
                    drop(h.downloader.stop());
                    h.progress_task.abort();
                    // Wait for task to finish (with timeout)
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h.task).await;
                }
            }
        }

        Ok(())
    }

    /// Helper to update download state
    fn update_state(&self, id: DownloadId, new_state: DownloadState) -> Result<()> {
        let mut downloads = self.downloads.write();
        let download = downloads
            .get_mut(&id)
            .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

        let old_state = download.status.state.clone();
        download.status.state = new_state.clone();

        let _ = self.event_tx.send(DownloadEvent::StateChanged {
            id,
            old_state,
            new_state,
        });

        Ok(())
    }
}

impl Drop for DownloadEngine {
    fn drop(&mut self) {
        // Signal shutdown on drop
        self.shutdown.cancel();
    }
}
