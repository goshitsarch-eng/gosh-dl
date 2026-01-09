//! Storage Module
//!
//! This module handles persistent storage for download state and session data.
//! Uses SQLite with WAL mode for crash-safe atomic commits.

pub mod sqlite;

pub use sqlite::SqliteStorage;

use crate::error::Result;
use crate::types::{DownloadId, DownloadStatus};
use async_trait::async_trait;

/// Segment state for HTTP multi-connection downloads
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentState {
    /// Segment is waiting to be downloaded
    Pending,
    /// Segment is currently being downloaded
    Downloading,
    /// Segment completed successfully
    Completed,
    /// Segment failed and may be retried
    Failed { error: String, retries: u32 },
}

/// Represents a download segment for multi-connection HTTP downloads
#[derive(Debug, Clone)]
pub struct Segment {
    /// Segment index (0-based)
    pub index: usize,
    /// Start byte offset (inclusive)
    pub start: u64,
    /// End byte offset (inclusive)
    pub end: u64,
    /// Bytes downloaded for this segment
    pub downloaded: u64,
    /// Current state
    pub state: SegmentState,
}

impl Segment {
    /// Create a new pending segment
    pub fn new(index: usize, start: u64, end: u64) -> Self {
        Self {
            index,
            start,
            end,
            downloaded: 0,
            state: SegmentState::Pending,
        }
    }

    /// Get the total size of this segment
    pub fn size(&self) -> u64 {
        self.end - self.start + 1
    }

    /// Check if segment is complete
    pub fn is_complete(&self) -> bool {
        self.state == SegmentState::Completed
    }

    /// Get remaining bytes to download
    pub fn remaining(&self) -> u64 {
        self.size().saturating_sub(self.downloaded)
    }
}

/// Storage trait for persisting download state
///
/// Implementations of this trait handle storing and retrieving download
/// state to allow resume after crashes or restarts.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Save or update a download's status
    async fn save_download(&self, status: &DownloadStatus) -> Result<()>;

    /// Load a download by ID
    async fn load_download(&self, id: DownloadId) -> Result<Option<DownloadStatus>>;

    /// Load all downloads
    async fn load_all(&self) -> Result<Vec<DownloadStatus>>;

    /// Delete a download record
    async fn delete_download(&self, id: DownloadId) -> Result<()>;

    /// Save segment state for an HTTP download
    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> Result<()>;

    /// Load segment state for an HTTP download
    async fn load_segments(&self, id: DownloadId) -> Result<Vec<Segment>>;

    /// Delete segment state for a download
    async fn delete_segments(&self, id: DownloadId) -> Result<()>;

    /// Check if database is healthy
    async fn health_check(&self) -> Result<()>;

    /// Compact/vacuum the database
    async fn compact(&self) -> Result<()>;
}

/// In-memory storage for testing
#[derive(Debug, Default)]
pub struct MemoryStorage {
    downloads: parking_lot::RwLock<std::collections::HashMap<DownloadId, DownloadStatus>>,
    segments: parking_lot::RwLock<std::collections::HashMap<DownloadId, Vec<Segment>>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn save_download(&self, status: &DownloadStatus) -> Result<()> {
        self.downloads.write().insert(status.id, status.clone());
        Ok(())
    }

    async fn load_download(&self, id: DownloadId) -> Result<Option<DownloadStatus>> {
        Ok(self.downloads.read().get(&id).cloned())
    }

    async fn load_all(&self) -> Result<Vec<DownloadStatus>> {
        Ok(self.downloads.read().values().cloned().collect())
    }

    async fn delete_download(&self, id: DownloadId) -> Result<()> {
        self.downloads.write().remove(&id);
        self.segments.write().remove(&id);
        Ok(())
    }

    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> Result<()> {
        self.segments.write().insert(id, segments.to_vec());
        Ok(())
    }

    async fn load_segments(&self, id: DownloadId) -> Result<Vec<Segment>> {
        Ok(self.segments.read().get(&id).cloned().unwrap_or_default())
    }

    async fn delete_segments(&self, id: DownloadId) -> Result<()> {
        self.segments.write().remove(&id);
        Ok(())
    }

    async fn health_check(&self) -> Result<()> {
        Ok(())
    }

    async fn compact(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DownloadKind, DownloadMetadata, DownloadProgress, DownloadState};
    use chrono::Utc;
    use std::path::PathBuf;

    fn create_test_status() -> DownloadStatus {
        DownloadStatus {
            id: DownloadId::new(),
            kind: DownloadKind::Http,
            state: DownloadState::Downloading,
            priority: crate::priority_queue::DownloadPriority::Normal,
            progress: DownloadProgress::default(),
            metadata: DownloadMetadata {
                name: "test.zip".to_string(),
                url: Some("https://example.com/test.zip".to_string()),
                magnet_uri: None,
                info_hash: None,
                save_dir: PathBuf::from("/tmp"),
                filename: Some("test.zip".to_string()),
                user_agent: None,
                referer: None,
                headers: vec![],
                cookies: Vec::new(),
                checksum: None,
                mirrors: Vec::new(),
                etag: None,
                last_modified: None,
            },
            torrent_info: None,
            peers: None,
            created_at: Utc::now(),
            completed_at: None,
        }
    }

    #[tokio::test]
    async fn test_memory_storage() {
        let storage = MemoryStorage::new();
        let status = create_test_status();
        let id = status.id;

        // Save
        storage.save_download(&status).await.unwrap();

        // Load
        let loaded = storage.load_download(id).await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().id, id);

        // Load all
        let all = storage.load_all().await.unwrap();
        assert_eq!(all.len(), 1);

        // Delete
        storage.delete_download(id).await.unwrap();
        let loaded = storage.load_download(id).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_segment_storage() {
        let storage = MemoryStorage::new();
        let id = DownloadId::new();

        let segments = vec![
            Segment::new(0, 0, 999),
            Segment::new(1, 1000, 1999),
            Segment::new(2, 2000, 2999),
        ];

        // Save segments
        storage.save_segments(id, &segments).await.unwrap();

        // Load segments
        let loaded = storage.load_segments(id).await.unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].start, 0);
        assert_eq!(loaded[1].start, 1000);
        assert_eq!(loaded[2].start, 2000);

        // Delete segments
        storage.delete_segments(id).await.unwrap();
        let loaded = storage.load_segments(id).await.unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_segment_size() {
        let segment = Segment::new(0, 0, 999);
        assert_eq!(segment.size(), 1000);

        let segment = Segment::new(1, 1000, 1999);
        assert_eq!(segment.size(), 1000);
    }
}
