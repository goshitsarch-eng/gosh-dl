//! Download options and priority types
//!
//! Types for configuring individual downloads.

use super::checksum::ExpectedChecksum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What to do when an HTTP download finishes and a file with its final name
/// already exists.
///
/// HTTP downloads are written to `<name>.part` and renamed onto `<name>`
/// once complete. The name can be taken by then: by a file that was there
/// all along, or by one created while the download was running.
///
/// Set engine-wide with
/// [`DownloadEngine::set_file_conflict_policy`](crate::DownloadEngine::set_file_conflict_policy).
/// Torrents write straight to their final paths and are not affected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum FileConflictPolicy {
    /// Replace the existing file. This is the default, and the only
    /// behaviour of releases before 0.7.0.
    #[default]
    Overwrite = 0,
    /// Keep the existing file and save the download next to it under the
    /// first free numbered name: `report.pdf` -> `report (1).pdf`. The
    /// download's `metadata.filename` reports the name actually used.
    Rename = 1,
    /// Keep the existing file and fail the download with a storage error of
    /// kind `AlreadyExists`. The finished data stays in the `.part` file;
    /// once the application has moved the conflicting file out of the way,
    /// `resume()` completes the download (segmented downloads only rename;
    /// single-connection downloads fetch the file again).
    Fail = 2,
}

impl FileConflictPolicy {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Rename,
            2 => Self::Fail,
            _ => Self::Overwrite,
        }
    }
}

/// Priority levels for downloads
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
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
    /// Create the download in `Paused` state without starting a worker.
    /// Call `DownloadEngine::resume` to start it. Recursive HTTP discovery
    /// still runs, but discovered file transfers remain paused.
    #[serde(default)]
    pub start_paused: bool,
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

impl DownloadOptions {
    /// Create new download options with defaults
    pub fn new() -> Self {
        Self::default()
    }

    /// Set download priority
    pub fn priority(mut self, priority: DownloadPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Set the directory to save files
    pub fn save_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.save_dir = Some(dir.into());
        self
    }

    /// Set the output filename
    pub fn filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    /// Set a custom user agent
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }

    /// Set the referer header
    pub fn referer(mut self, referer: impl Into<String>) -> Self {
        self.referer = Some(referer.into());
        self
    }

    /// Add a custom header
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Set cookies for authenticated downloads
    pub fn cookies(mut self, cookies: Vec<String>) -> Self {
        self.cookies = Some(cookies);
        self
    }

    /// Set expected checksum for verification
    pub fn checksum(mut self, checksum: ExpectedChecksum) -> Self {
        self.checksum = Some(checksum);
        self
    }

    /// Add a mirror/fallback URL
    pub fn mirror(mut self, url: impl Into<String>) -> Self {
        self.mirrors.push(url.into());
        self
    }

    /// Set max connections for this download
    pub fn max_connections(mut self, n: usize) -> Self {
        self.max_connections = Some(n);
        self
    }

    /// Set max download speed in bytes/sec
    pub fn max_download_speed(mut self, bps: u64) -> Self {
        self.max_download_speed = Some(bps);
        self
    }

    /// Set max upload speed in bytes/sec (torrent only)
    pub fn max_upload_speed(mut self, bps: u64) -> Self {
        self.max_upload_speed = Some(bps);
        self
    }

    /// Set seed ratio limit (torrent only)
    pub fn seed_ratio(mut self, ratio: f64) -> Self {
        self.seed_ratio = Some(ratio);
        self
    }

    /// Set selected file indices (torrent only)
    pub fn selected_files(mut self, indices: Vec<usize>) -> Self {
        self.selected_files = Some(indices);
        self
    }

    /// Enable sequential download mode for streaming (torrent only)
    pub fn sequential(mut self, enabled: bool) -> Self {
        self.sequential = Some(enabled);
        self
    }
}
