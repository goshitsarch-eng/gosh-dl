//! Download options and priority types
//!
//! Types for configuring individual downloads.

use super::checksum::ExpectedChecksum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Priority levels for downloads
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(i8)]
pub enum DownloadPriority {
    /// Low priority - downloads last
    Low = -1,
    /// Normal priority - default for most downloads
    #[default]
    Normal = 0,
    /// High priority - downloads before normal
    High = 1,
    /// Critical priority - downloads first
    Critical = 2,
}

impl std::fmt::Display for DownloadPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Normal => write!(f, "normal"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

impl std::str::FromStr for DownloadPriority {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "low" | "-1" => Ok(Self::Low),
            "normal" | "0" => Ok(Self::Normal),
            "high" | "1" => Ok(Self::High),
            "critical" | "2" => Ok(Self::Critical),
            _ => Err(format!("Invalid priority: {}", s)),
        }
    }
}

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
