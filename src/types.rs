//! Core types for gosh-dl
//!
//! This module contains all the fundamental data types used throughout
//! the download engine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Unique identifier for a download
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DownloadId(Uuid);

impl DownloadId {
    /// Create a new random download ID
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Create from an existing UUID
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Get the underlying UUID
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Generate an aria2-compatible GID (16-char hex string)
    /// This is for backwards compatibility with existing frontend
    pub fn to_gid(&self) -> String {
        hex::encode(&self.0.as_bytes()[0..8])
    }

    /// Try to parse from a GID string
    pub fn from_gid(gid: &str) -> Option<Self> {
        if gid.len() != 16 {
            return None;
        }
        let bytes = hex::decode(gid).ok()?;
        if bytes.len() != 8 {
            return None;
        }
        // Create a UUID with the GID bytes + zeros
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes[0..8].copy_from_slice(&bytes);
        Some(Self(Uuid::from_bytes(uuid_bytes)))
    }
}

impl Default for DownloadId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for DownloadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_gid())
    }
}

/// Type of download
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadKind {
    /// HTTP/HTTPS download
    Http,
    /// BitTorrent download from .torrent file
    Torrent,
    /// BitTorrent download from magnet URI
    Magnet,
}

/// Current state of a download
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum DownloadState {
    /// Waiting in queue
    Queued,
    /// Connecting to server/peers
    Connecting,
    /// Actively downloading
    Downloading,
    /// Seeding (torrent only)
    Seeding,
    /// Paused by user
    Paused,
    /// Successfully completed
    Completed,
    /// Failed with error
    Error {
        kind: String,
        message: String,
        retryable: bool,
    },
}

impl DownloadState {
    /// Check if download is active (downloading or seeding)
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Downloading | Self::Seeding | Self::Connecting)
    }

    /// Check if download is finished (completed or error)
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Completed | Self::Error { .. })
    }

    /// Convert to aria2-compatible status string
    pub fn to_aria2_status(&self) -> &'static str {
        match self {
            Self::Queued => "waiting",
            Self::Connecting => "active",
            Self::Downloading => "active",
            Self::Seeding => "active",
            Self::Paused => "paused",
            Self::Completed => "complete",
            Self::Error { .. } => "error",
        }
    }
}

/// Progress information for a download
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadProgress {
    /// Total size in bytes (may be unknown initially)
    pub total_size: Option<u64>,
    /// Bytes downloaded so far
    pub completed_size: u64,
    /// Current download speed in bytes/sec
    pub download_speed: u64,
    /// Current upload speed in bytes/sec (torrent only)
    pub upload_speed: u64,
    /// Number of active connections
    pub connections: u32,
    /// Number of seeders (torrent only)
    pub seeders: u32,
    /// Number of peers (torrent only)
    pub peers: u32,
    /// Estimated time remaining in seconds
    pub eta_seconds: Option<u64>,
}

impl DownloadProgress {
    /// Calculate progress percentage (0.0 - 100.0)
    pub fn percentage(&self) -> f64 {
        match self.total_size {
            Some(total) if total > 0 => (self.completed_size as f64 / total as f64) * 100.0,
            _ => 0.0,
        }
    }
}

/// Metadata about a download
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadMetadata {
    /// Display name
    pub name: String,
    /// Original URL (for HTTP)
    pub url: Option<String>,
    /// Magnet URI (for magnet links)
    pub magnet_uri: Option<String>,
    /// Info hash (for torrents)
    pub info_hash: Option<String>,
    /// Save directory
    pub save_dir: PathBuf,
    /// Output filename (may differ from name for multi-file torrents)
    pub filename: Option<String>,
    /// Custom user agent
    pub user_agent: Option<String>,
    /// Custom referer
    pub referer: Option<String>,
    /// Custom headers
    pub headers: Vec<(String, String)>,
    /// Cookies for authenticated downloads
    #[serde(default)]
    pub cookies: Vec<String>,
    /// Expected checksum for verification (HTTP only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<ExpectedChecksum>,
    /// Mirror/fallback URLs (HTTP only)
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// ETag for resume validation (HTTP only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Last-Modified for resume validation (HTTP only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

/// Full status of a download
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStatus {
    /// Unique identifier
    pub id: DownloadId,
    /// Type of download
    pub kind: DownloadKind,
    /// Current state
    pub state: DownloadState,
    /// Download priority
    #[serde(default)]
    pub priority: crate::priority_queue::DownloadPriority,
    /// Progress information
    pub progress: DownloadProgress,
    /// Metadata
    pub metadata: DownloadMetadata,
    /// Torrent-specific info (only for torrent/magnet downloads)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torrent_info: Option<TorrentStatusInfo>,
    /// Connected peers (only for torrent downloads)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers: Option<Vec<PeerInfo>>,
    /// When the download was created
    pub created_at: DateTime<Utc>,
    /// When the download completed (if completed)
    pub completed_at: Option<DateTime<Utc>>,
}

/// Torrent status information embedded in DownloadStatus
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentStatusInfo {
    /// Files in the torrent
    pub files: Vec<TorrentFile>,
    /// Piece length
    pub piece_length: u64,
    /// Number of pieces
    pub pieces_count: usize,
    /// Is private torrent
    pub private: bool,
}

impl DownloadStatus {
    /// Get GID for frontend compatibility
    pub fn gid(&self) -> String {
        self.id.to_gid()
    }
}

use crate::http::ExpectedChecksum;
use crate::priority_queue::DownloadPriority;

/// Options for adding a new download
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadOptions {
    /// Download priority (affects queue ordering)
    #[serde(default)]
    pub priority: DownloadPriority,
    /// Directory to save files
    pub save_dir: Option<PathBuf>,
    /// Output filename
    pub filename: Option<String>,
    /// Custom user agent
    pub user_agent: Option<String>,
    /// Referer header
    pub referer: Option<String>,
    /// Additional headers
    pub headers: Vec<(String, String)>,
    /// Cookies for authenticated downloads (e.g., ["session=abc123", "token=xyz"])
    pub cookies: Option<Vec<String>>,
    /// Expected checksum for verification after download (e.g., MD5 or SHA256)
    pub checksum: Option<ExpectedChecksum>,
    /// Mirror/fallback URLs for redundancy (tried in order on failure)
    pub mirrors: Vec<String>,
    /// Max connections for this download
    pub max_connections: Option<usize>,
    /// Max download speed (bytes/sec)
    pub max_download_speed: Option<u64>,
    /// Max upload speed (bytes/sec, torrent only)
    pub max_upload_speed: Option<u64>,
    /// Seed ratio limit (torrent only)
    pub seed_ratio: Option<f64>,
    /// Selected file indices (torrent only)
    pub selected_files: Option<Vec<usize>>,
    /// Sequential download mode (torrent only) - downloads pieces in order for streaming
    pub sequential: Option<bool>,
}

/// Events emitted by the download engine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DownloadEvent {
    /// Download was added
    Added { id: DownloadId },
    /// Download started
    Started { id: DownloadId },
    /// Progress update
    Progress {
        id: DownloadId,
        progress: DownloadProgress,
    },
    /// State changed
    StateChanged {
        id: DownloadId,
        old_state: DownloadState,
        new_state: DownloadState,
    },
    /// Download completed successfully
    Completed { id: DownloadId },
    /// Download failed
    Failed {
        id: DownloadId,
        error: String,
        retryable: bool,
    },
    /// Download was removed
    Removed { id: DownloadId },
    /// Download was paused
    Paused { id: DownloadId },
    /// Download was resumed
    Resumed { id: DownloadId },
}

/// Global statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalStats {
    /// Total download speed
    pub download_speed: u64,
    /// Total upload speed
    pub upload_speed: u64,
    /// Number of active downloads
    pub num_active: usize,
    /// Number of waiting downloads
    pub num_waiting: usize,
    /// Number of stopped downloads
    pub num_stopped: usize,
}

/// Information about a file in a torrent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFile {
    /// File index
    pub index: usize,
    /// File path within torrent
    pub path: PathBuf,
    /// File size in bytes
    pub size: u64,
    /// Whether this file is selected for download
    pub selected: bool,
    /// Bytes completed for this file
    pub completed: u64,
}

/// Parsed torrent metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentInfo {
    /// Info hash (hex string)
    pub info_hash: String,
    /// Torrent name
    pub name: String,
    /// Total size in bytes
    pub total_size: u64,
    /// Piece length
    pub piece_length: u64,
    /// Number of pieces
    pub num_pieces: usize,
    /// Files in the torrent
    pub files: Vec<TorrentFile>,
    /// Announce URL
    pub announce: Option<String>,
    /// Announce list
    pub announce_list: Vec<Vec<String>>,
    /// Creation date
    pub creation_date: Option<DateTime<Utc>>,
    /// Comment
    pub comment: Option<String>,
    /// Created by
    pub created_by: Option<String>,
}

/// Peer information (for torrents)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Peer ID (if known)
    pub id: Option<String>,
    /// IP address
    pub ip: String,
    /// Port
    pub port: u16,
    /// Client name (if known)
    pub client: Option<String>,
    /// Download speed from this peer
    pub download_speed: u64,
    /// Upload speed to this peer
    pub upload_speed: u64,
    /// Progress of peer (0.0 - 1.0)
    pub progress: f64,
    /// Whether we're choking them
    pub am_choking: bool,
    /// Whether they're choking us
    pub peer_choking: bool,
}

// Helper for hex encoding (used by DownloadId)
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, ()> {
        if !s.len().is_multiple_of(2) {
            return Err(());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
            .collect()
    }
}
