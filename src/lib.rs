//! # gosh-dl
//!
//! A fast, safe, and reliable download engine written in Rust.
//!
//! ## Features
//!
//! - **HTTP/HTTPS Downloads**: Multi-connection segmented downloads with resume support
//! - **BitTorrent**: Full protocol support including DHT, PEX, and LPD
//! - **Cross-platform**: Works on Linux, macOS, and Windows
//! - **Memory-safe**: Written in Rust with no unsafe code in core paths
//! - **Async**: Built on Tokio for efficient concurrent downloads
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use gosh_dl::{DownloadEngine, EngineConfig, DownloadOptions};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create engine with default config
//!     let config = EngineConfig::default();
//!     let engine = DownloadEngine::new(config).await?;
//!
//!     // Add a download
//!     let id = engine.add_http(
//!         "https://example.com/file.zip",
//!         DownloadOptions::default(),
//!     ).await?;
//!
//!     // Subscribe to events
//!     let mut events = engine.subscribe();
//!     while let Ok(event) = events.recv().await {
//!         println!("Event: {:?}", event);
//!     }
//!
//!     Ok(())
//! }
//! ```

// Modules
pub mod config;
pub mod engine;
pub mod error;
#[cfg(feature = "http")]
pub mod http;
pub(crate) mod priority_queue;
pub mod protocol;
pub(crate) mod scheduler;
pub mod storage;
#[cfg(feature = "torrent")]
pub mod torrent;
pub(crate) mod types;

// Re-exports for convenience
pub use config::{AllocationMode, EngineConfig, HttpConfig, TorrentConfig};
pub use engine::DownloadEngine;
pub use error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result, StorageErrorKind};
pub use protocol::{ProtocolError, ProtocolResult};
pub use types::{
    DownloadEvent, DownloadId, DownloadKind, DownloadMetadata, DownloadOptions, DownloadProgress,
    DownloadState, DownloadStatus, GlobalStats, PeerInfo, TorrentFile, TorrentInfo,
    TorrentStatusInfo,
};

// Storage exports
#[cfg(feature = "storage")]
pub use storage::SqliteStorage;
pub use storage::{MemoryStorage, Segment, SegmentState, Storage};

// Priority queue exports
pub use priority_queue::{DownloadPriority, PriorityQueue, PriorityQueueStats};

// Scheduler exports
pub use scheduler::{BandwidthLimits, BandwidthScheduler, ScheduleRule};

// HTTP module exports
#[cfg(feature = "http")]
pub use http::{
    ConnectionPool, HttpDownloader, ResumeInfo, RetryPolicy, SegmentedDownload, ServerCapabilities,
    SpeedCalculator,
};
