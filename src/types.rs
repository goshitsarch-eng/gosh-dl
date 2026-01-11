//! Core types for gosh-dl
//!
//! This module re-exports all protocol types for backward compatibility.
//! New code should prefer importing from `crate::protocol` directly.

// Re-export all types from protocol module for backward compatibility
pub use crate::protocol::{
    // Core types
    DownloadId,
    DownloadKind,
    DownloadProgress,
    DownloadState,
    // Options
    DownloadOptions,
    DownloadPriority,
    // Status types
    DownloadMetadata,
    DownloadStatus,
    GlobalStats,
    // Events
    DownloadEvent,
    // Torrent types
    PeerInfo,
    TorrentFile,
    TorrentInfo,
    TorrentStatusInfo,
    // Checksum types
    ChecksumAlgorithm,
    ExpectedChecksum,
    // Scheduler types
    BandwidthLimits,
    ScheduleRule,
};
