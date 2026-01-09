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
//! ```rust,no_run
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
pub mod http;
pub mod priority_queue;
pub mod scheduler;
pub mod storage;
pub mod torrent;
pub mod types;

// Re-exports for convenience
pub use config::{AllocationMode, EngineConfig, HttpConfig, TorrentConfig};
pub use engine::DownloadEngine;
pub use error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result, StorageErrorKind};
pub use types::{
    DownloadEvent, DownloadId, DownloadKind, DownloadMetadata, DownloadOptions, DownloadProgress,
    DownloadState, DownloadStatus, GlobalStats, PeerInfo, TorrentFile, TorrentInfo,
    TorrentStatusInfo,
};

// Storage exports
pub use storage::{MemoryStorage, Segment, SegmentState, SqliteStorage, Storage};

// Priority queue exports
pub use priority_queue::{DownloadPriority, PriorityQueue, PriorityQueueStats};

// Scheduler exports
pub use scheduler::{BandwidthLimits, BandwidthScheduler, ScheduleRule};

// HTTP module exports
pub use http::{
    ConnectionPool, HttpDownloader, ResumeInfo, RetryPolicy, SegmentedDownload, ServerCapabilities,
    SpeedCalculator,
};
