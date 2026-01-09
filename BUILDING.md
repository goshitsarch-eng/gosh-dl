# Building gosh-dl: Native Rust Download Engine

This document provides comprehensive guidance for building and developing the gosh-dl download engine across all implementation phases.

---

## Table of Contents

1. [Overview](#overview)
2. [Prerequisites](#prerequisites)
3. [Project Structure](#project-structure)
4. [Phase 1: Core Infrastructure & Basic HTTP](#phase-1-core-infrastructure--basic-http)
5. [Phase 2: Segmented HTTP & Reliability](#phase-2-segmented-http--reliability)
6. [Phase 3: BitTorrent Core](#phase-3-bittorrent-core)
7. [Phase 4: BitTorrent P2P Features](#phase-4-bittorrent-p2p-features)
8. [Phase 5: Optimization & Distribution](#phase-5-optimization--distribution)
9. [Testing Guide](#testing-guide)
10. [API Reference](#api-reference)
11. [Migration from aria2](#migration-from-aria2)
12. [Troubleshooting](#troubleshooting)

---

## Overview

gosh-dl is a standalone Rust download engine designed to replace aria2 in the Gosh-Fetch application. It provides:

- **Safety**: Memory-safe Rust, no buffer overflows, sandboxed I/O
- **Performance**: Zero-copy I/O, async Tokio runtime, no IPC overhead
- **Reliability**: Typed errors, automatic recovery, SQLite WAL persistence
- **Portability**: Cross-platform (Linux, macOS, Windows)

### Design Principles

1. **Library-first**: Designed as a reusable crate, not tied to Gosh-Fetch
2. **Async-native**: Built on Tokio for efficient concurrent operations
3. **Type-safe**: Strong typing throughout, minimal runtime errors
4. **Testable**: Modular design with dependency injection for testing
5. **Observable**: Event-driven architecture for progress tracking

---

## Prerequisites

### Required Tools

```bash
# Rust toolchain (1.75+ recommended)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update stable

# Verify installation
rustc --version  # Should be 1.75.0 or higher
cargo --version
```

### Platform-Specific Dependencies

#### Linux (Debian/Ubuntu)
```bash
sudo apt update
sudo apt install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    libsqlite3-dev
```

#### Linux (Fedora/RHEL)
```bash
sudo dnf install -y \
    gcc \
    pkg-config \
    openssl-devel \
    sqlite-devel
```

#### Linux (Arch)
```bash
sudo pacman -S base-devel openssl sqlite
```

#### macOS
```bash
# Xcode command line tools
xcode-select --install

# OpenSSL (if needed)
brew install openssl@3
```

#### Windows
```powershell
# Install Visual Studio Build Tools
# Download from: https://visualstudio.microsoft.com/visual-cpp-build-tools/
# Select "Desktop development with C++"

# Or use winget
winget install Microsoft.VisualStudio.2022.BuildTools
```

### Development Tools (Optional but Recommended)

```bash
# Code formatting
rustup component add rustfmt

# Linting
rustup component add clippy

# Code coverage
cargo install cargo-tarpaulin

# Benchmarking
cargo install cargo-criterion

# Documentation
cargo install cargo-doc
```

---

## Project Structure

### Phase Status Summary

| Phase | Status | Description |
|-------|--------|-------------|
| Phase 1 | ✅ COMPLETE | Core Infrastructure & Basic HTTP |
| Phase 2 | ✅ COMPLETE | Segmented HTTP & Reliability |
| Phase 3 | ✅ COMPLETE | BitTorrent Core |
| Phase 4 | ✅ COMPLETE | BitTorrent P2P Features |
| Phase 5 | 🔲 NOT STARTED | Optimization & Distribution |

### Directory Layout

```
gosh-dl/
├── Cargo.toml                 # Crate manifest
├── BUILDING.md                # This file
├── README.md                  # User-facing documentation
├── LICENSE                    # MIT license
│
├── src/
│   ├── lib.rs                 # Public API, re-exports
│   ├── engine.rs              # DownloadEngine - main coordinator
│   ├── types.rs               # Core types (DownloadId, Status, Progress)
│   ├── error.rs               # Typed error hierarchy
│   ├── config.rs              # EngineConfig and sub-configs
│   │
│   ├── http/                  # HTTP download engine
│   │   ├── mod.rs             # HttpDownloader ✅
│   │   ├── segment.rs         # Segmented download logic ✅
│   │   ├── connection.rs      # Connection pooling ✅
│   │   └── resume.rs          # Resume detection ✅
│   │
│   ├── torrent/               # BitTorrent engine
│   │   ├── mod.rs             # TorrentDownloader ✅
│   │   ├── bencode.rs         # Bencode parser ✅
│   │   ├── metainfo.rs        # Torrent file parser ✅
│   │   ├── magnet.rs          # Magnet URI parser ✅
│   │   ├── tracker.rs         # HTTP/UDP tracker clients ✅
│   │   ├── peer.rs            # Peer wire protocol ✅
│   │   ├── piece.rs           # Piece management ✅
│   │   ├── dht.rs             # DHT client ✅
│   │   ├── pex.rs             # Peer Exchange ✅
│   │   ├── lpd.rs             # Local Peer Discovery ✅
│   │   └── choking.rs         # Choking algorithm ✅
│   │
│   └── storage/               # Persistence layer
│       ├── mod.rs             # Storage trait + MemoryStorage ✅
│       └── sqlite.rs          # SQLite backend ✅
│
├── tests/                     # Integration tests
│   └── integration_tests.rs   # 18 end-to-end tests using wiremock
│
├── benches/                   # Benchmarks [Phase 5]
│   └── download_bench.rs
│
└── examples/                  # Usage examples
    ├── simple_download.rs
    └── torrent_client.rs
```

---

## Phase 1: Core Infrastructure & Basic HTTP

**Status: ✅ COMPLETE**

### Goals
- Establish crate structure and public API
- Implement single-connection HTTP downloads
- Create event system for progress tracking
- Build Tauri adapter for Gosh-Fetch integration

### Files Created

| File | Purpose |
|------|---------|
| `Cargo.toml` | Dependencies and crate metadata |
| `src/lib.rs` | Public API with module re-exports |
| `src/types.rs` | Core types: `DownloadId`, `DownloadStatus`, `DownloadProgress`, `DownloadEvent` |
| `src/error.rs` | Error types: `EngineError`, `NetworkErrorKind`, `StorageErrorKind`, `ProtocolErrorKind` |
| `src/config.rs` | Configuration: `EngineConfig`, `HttpConfig`, `TorrentConfig` |
| `src/engine.rs` | Main coordinator: `DownloadEngine` |
| `src/http/mod.rs` | HTTP downloader with resume support |
| `src/storage/mod.rs` | Placeholder module |
| `src/torrent/mod.rs` | Placeholder module |

### Key Implementations

#### DownloadEngine API

```rust
pub struct DownloadEngine { /* ... */ }

impl DownloadEngine {
    // Lifecycle
    pub async fn new(config: EngineConfig) -> Result<Arc<Self>>;
    pub async fn shutdown(&self) -> Result<()>;

    // HTTP Downloads
    pub async fn add_http(&self, url: &str, opts: DownloadOptions) -> Result<DownloadId>;

    // Control
    pub async fn pause(&self, id: DownloadId) -> Result<()>;
    pub async fn resume(&self, id: DownloadId) -> Result<()>;
    pub async fn cancel(&self, id: DownloadId, delete_files: bool) -> Result<()>;

    // Status
    pub fn status(&self, id: DownloadId) -> Option<DownloadStatus>;
    pub fn list(&self) -> Vec<DownloadStatus>;
    pub fn active(&self) -> Vec<DownloadStatus>;
    pub fn global_stats(&self) -> GlobalStats;

    // Events
    pub fn subscribe(&self) -> broadcast::Receiver<DownloadEvent>;

    // Configuration
    pub fn set_config(&self, config: EngineConfig) -> Result<()>;
    pub fn get_config(&self) -> EngineConfig;
}
```

#### Error Hierarchy

```rust
pub enum EngineError {
    Network { kind: NetworkErrorKind, message: String, retryable: bool },
    Storage { kind: StorageErrorKind, path: PathBuf, message: String },
    Protocol { kind: ProtocolErrorKind, message: String },
    InvalidInput { field: &'static str, message: String },
    ResourceLimit { resource: &'static str, limit: usize },
    NotFound(String),
    AlreadyExists(String),
    InvalidState { action: &'static str, current_state: String },
    Shutdown,
    Database(String),
    Internal(String),
}

pub enum NetworkErrorKind {
    DnsResolution, ConnectionRefused, ConnectionReset, Timeout,
    Tls, HttpStatus(u16), Unreachable, TooManyRedirects, Other,
}

pub enum StorageErrorKind {
    NotFound, PermissionDenied, DiskFull, PathTraversal,
    AlreadyExists, InvalidPath, Io,
}

pub enum ProtocolErrorKind {
    InvalidUrl, RangeNotSupported, InvalidResponse, InvalidTorrent,
    InvalidMagnet, HashMismatch, TrackerError, PeerProtocol, BencodeParse,
}
```

#### HTTP Download Flow

```
add_http(url, options)
    │
    ▼
┌─────────────────────┐
│ Validate URL        │
│ Parse scheme        │
│ Check http/https    │
└─────────────────────┘
    │
    ▼
┌─────────────────────┐
│ Create DownloadId   │
│ Build metadata      │
│ Store in HashMap    │
└─────────────────────┘
    │
    ▼
┌─────────────────────┐
│ Emit Added event    │
│ Start download task │
└─────────────────────┘
    │
    ▼
┌─────────────────────┐
│ Acquire semaphore   │ ◄── Concurrent download limit
│ Update state        │
└─────────────────────┘
    │
    ▼
┌─────────────────────┐
│ HEAD request        │
│ Get Content-Length  │
│ Check Accept-Ranges │
└─────────────────────┘
    │
    ▼
┌─────────────────────┐
│ Check existing file │
│ Resume if possible  │
│ Add Range header    │
└─────────────────────┘
    │
    ▼
┌─────────────────────┐
│ GET request         │
│ Stream to file      │
│ Track progress      │
└─────────────────────┘
    │
    ▼
┌─────────────────────┐
│ Rename .part file   │
│ Emit Completed      │
└─────────────────────┘
```

### Build Verification

```bash
cd gosh-dl

# Check compilation
cargo check

# Run tests
cargo test

# Check with all features
cargo check --all-features

# Lint
cargo clippy -- -D warnings

# Format check
cargo fmt -- --check
```

### Integration with Gosh-Fetch

The `engine_adapter.rs` file in `src-tauri/src/` bridges gosh-dl to existing Tauri commands:

```rust
// src-tauri/Cargo.toml
[dependencies]
gosh-dl = { path = "../gosh-dl" }

// src-tauri/src/engine_adapter.rs
pub struct EngineAdapter {
    engine: Arc<DownloadEngine>,
}

impl EngineAdapter {
    pub async fn add_download(&self, url: String, options: Option<Aria2Options>) -> Result<String>;
    pub async fn pause(&self, gid: &str) -> Result<()>;
    pub async fn resume(&self, gid: &str) -> Result<()>;
    // ... maps to existing frontend API
}
```

---

## Phase 2: Segmented HTTP & Reliability

**Status: ✅ COMPLETE**

### Goals
- Multi-connection segmented downloads (16 parallel connections)
- SQLite-based state persistence with WAL mode
- Resume from crash/restart
- Connection pooling with health checks
- Speed limiting (token bucket algorithm)

### Files Created

| File | Purpose |
|------|---------|
| `src/http/segment.rs` | Segment management and parallel downloads |
| `src/http/connection.rs` | Connection pool with rate limiting |
| `src/http/resume.rs` | Resume capability detection |
| `src/storage/sqlite.rs` | SQLite persistence layer |
| `src/storage/mod.rs` | Storage trait, Segment types, MemoryStorage |

### Implementation Summary

**Segmented Downloads (`src/http/segment.rs`):**
- `SegmentedDownload` struct manages parallel segment downloads
- `calculate_segment_count()` determines optimal segments based on file size
- `probe_server()` checks server capabilities via HEAD request
- Automatic fallback to single-connection for small files or non-range servers
- Progress aggregation across all segments

**Connection Pool (`src/http/connection.rs`):**
- `ConnectionPool` with rate limiting using `governor` crate
- `RetryPolicy` with exponential backoff + jitter
- `SpeedCalculator` for real-time speed tracking
- `with_retry()` helper for automatic retry logic

**Resume Detection (`src/http/resume.rs`):**
- `ResumeInfo` struct with server capabilities
- `check_resume()` validates ETag/Last-Modified for safe resume
- Content-Range header parsing and validation
- Partial file cleanup utilities

**Storage (`src/storage/mod.rs`, `src/storage/sqlite.rs`):**
- `Storage` trait with async methods
- `SqliteStorage` with WAL mode for crash safety
- `MemoryStorage` for testing
- `Segment` and `SegmentState` types for segment tracking

**Engine Integration:**
- `DownloadEngine` now uses `download_segmented()` automatically
- Configured via `max_connections_per_download` and `min_segment_size`

### Tests (24 passing)
```bash
cargo test  # All 24 tests pass
```

### Segment Download Algorithm

```
HEAD Request
    │
    ▼
┌─────────────────────────────────────┐
│ Content-Length: 100MB               │
│ Accept-Ranges: bytes                │
│ ETag: "abc123"                      │
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Calculate segments                  │
│ 100MB ÷ 16 = 6.25MB per segment     │
│ min_segment_size = 1MB ✓            │
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Segment 0:  0MB -  6.25MB           │
│ Segment 1:  6.25MB - 12.5MB         │
│ Segment 2:  12.5MB - 18.75MB        │
│ ...                                 │
│ Segment 15: 93.75MB - 100MB         │
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Spawn 16 tasks with Range headers   │
│ Range: bytes=0-6553599              │
│ Range: bytes=6553600-13107199       │
│ ...                                 │
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Each task writes to correct offset  │
│ file.seek(SeekFrom::Start(offset))  │
│ file.write_all(&chunk)              │
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Aggregate progress from all         │
│ segments for UI updates             │
└─────────────────────────────────────┘
```

### Segment Data Structures

```rust
// src/http/segment.rs

pub struct SegmentedDownload {
    id: DownloadId,
    url: String,
    segments: Vec<Segment>,
    file: Arc<Mutex<File>>,
    total_size: u64,
    supports_range: bool,
    etag: Option<String>,
}

pub struct Segment {
    index: usize,
    start: u64,
    end: u64,
    downloaded: AtomicU64,
    state: RwLock<SegmentState>,
}

pub enum SegmentState {
    Pending,
    Downloading { task_id: usize },
    Completed,
    Failed { error: String, retries: u32 },
}

impl SegmentedDownload {
    /// Calculate optimal segment count based on file size and config
    pub fn calculate_segments(
        total_size: u64,
        max_connections: usize,
        min_segment_size: u64,
    ) -> usize {
        let ideal = max_connections;
        let min_segments = (total_size / min_segment_size) as usize;
        ideal.min(min_segments).max(1)
    }

    /// Start all segment downloads
    pub async fn start(&self, client: &Client) -> Result<()>;

    /// Get aggregated progress
    pub fn progress(&self) -> DownloadProgress;

    /// Pause all segments
    pub async fn pause(&self);

    /// Resume from saved state
    pub async fn resume(&self, client: &Client) -> Result<()>;
}
```

### SQLite Schema

```sql
-- src/storage/sqlite.rs

-- Downloads table
CREATE TABLE IF NOT EXISTS downloads (
    id TEXT PRIMARY KEY,                    -- UUID
    kind TEXT NOT NULL,                     -- 'http', 'torrent', 'magnet'
    state TEXT NOT NULL,                    -- JSON: DownloadState
    url TEXT,
    magnet_uri TEXT,
    info_hash TEXT,
    name TEXT NOT NULL,
    save_dir TEXT NOT NULL,
    filename TEXT,
    total_size INTEGER,
    completed_size INTEGER DEFAULT 0,

    -- HTTP specific
    etag TEXT,
    last_modified TEXT,
    supports_range INTEGER DEFAULT 0,

    -- Torrent specific
    piece_length INTEGER,
    pieces_have BLOB,                       -- Bitfield

    -- Metadata
    user_agent TEXT,
    referer TEXT,
    headers_json TEXT,                      -- JSON array
    error_message TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    completed_at TEXT
);

-- Segments table (HTTP multi-connection)
CREATE TABLE IF NOT EXISTS segments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    download_id TEXT NOT NULL,
    segment_index INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    downloaded INTEGER DEFAULT 0,
    state TEXT NOT NULL,                    -- 'pending', 'downloading', 'completed', 'failed'
    error_message TEXT,
    retries INTEGER DEFAULT 0,

    FOREIGN KEY (download_id) REFERENCES downloads(id) ON DELETE CASCADE,
    UNIQUE (download_id, segment_index)
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_downloads_state ON downloads(state);
CREATE INDEX IF NOT EXISTS idx_downloads_kind ON downloads(kind);
CREATE INDEX IF NOT EXISTS idx_segments_download ON segments(download_id);

-- Enable WAL mode for crash safety
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
```

### Storage Trait

```rust
// src/storage/mod.rs

#[async_trait]
pub trait Storage: Send + Sync {
    /// Save or update a download
    async fn save_download(&self, status: &DownloadStatus) -> Result<()>;

    /// Load a download by ID
    async fn load_download(&self, id: DownloadId) -> Result<Option<DownloadStatus>>;

    /// Load all downloads
    async fn load_all(&self) -> Result<Vec<DownloadStatus>>;

    /// Delete a download
    async fn delete_download(&self, id: DownloadId) -> Result<()>;

    /// Save segment state
    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> Result<()>;

    /// Load segment state
    async fn load_segments(&self, id: DownloadId) -> Result<Vec<Segment>>;

    /// Transaction support
    async fn transaction<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Self) -> Result<T> + Send;
}
```

### Speed Limiting

```rust
// Using governor crate for token bucket rate limiting

use governor::{Quota, RateLimiter};
use std::num::NonZeroU32;

pub struct SpeedLimiter {
    limiter: Option<RateLimiter</* ... */>>,
}

impl SpeedLimiter {
    pub fn new(bytes_per_second: Option<u64>) -> Self {
        let limiter = bytes_per_second.map(|bps| {
            let quota = Quota::per_second(NonZeroU32::new(bps as u32).unwrap());
            RateLimiter::direct(quota)
        });
        Self { limiter }
    }

    pub async fn acquire(&self, bytes: u64) {
        if let Some(ref limiter) = self.limiter {
            // Wait for permission to send `bytes`
            limiter.until_n_ready(NonZeroU32::new(bytes as u32).unwrap()).await;
        }
    }
}
```

### Retry Logic

```rust
// Exponential backoff with jitter

pub struct RetryPolicy {
    max_attempts: u32,
    initial_delay_ms: u64,
    max_delay_ms: u64,
    jitter_factor: f64,
}

impl RetryPolicy {
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base = self.initial_delay_ms * 2u64.pow(attempt.min(10));
        let capped = base.min(self.max_delay_ms);

        // Add jitter: ±25% randomness
        let jitter = (rand::random::<f64>() - 0.5) * 2.0 * self.jitter_factor;
        let with_jitter = (capped as f64 * (1.0 + jitter)) as u64;

        Duration::from_millis(with_jitter)
    }
}
```

### Build Verification

```bash
# Run Phase 2 specific tests
cargo test segment
cargo test storage
cargo test resume

# Test with real downloads (integration)
cargo test --test http_tests -- --ignored

# Benchmark segmented vs single connection
cargo bench segment
```

---

## Phase 3: BitTorrent Core

**Status: ✅ COMPLETE**

### Goals
- Bencode parser
- Torrent metainfo parsing
- Magnet URI parsing
- HTTP and UDP tracker communication
- Peer wire protocol
- Piece management with SHA-1 verification

### Files Created

| File | Purpose |
|------|---------|
| `src/torrent/bencode.rs` | Bencode serialization/deserialization with raw byte preservation |
| `src/torrent/metainfo.rs` | .torrent file parser with info_hash calculation |
| `src/torrent/magnet.rs` | Magnet URI parser (hex and base32 hash support) |
| `src/torrent/tracker.rs` | HTTP (BEP 3) and UDP (BEP 15) tracker clients |
| `src/torrent/peer.rs` | Peer wire protocol with extension support |
| `src/torrent/piece.rs` | Piece management with rarest-first selection |
| `src/torrent/mod.rs` | TorrentDownloader coordinator |

### Implementation Summary

**Bencode Parser (`src/torrent/bencode.rs`):**
- Custom parser that preserves raw bytes for info_hash calculation
- Supports integers, byte strings, lists, and dictionaries
- Validates dictionary key ordering
- Provides encoding and accessor methods

**Metainfo Parser (`src/torrent/metainfo.rs`):**
- Parses single-file and multi-file torrents
- Calculates SHA-1 info_hash from raw info dictionary
- Supports announce-list (BEP 12)
- Provides piece range and file mapping utilities

**Magnet URI Parser (`src/torrent/magnet.rs`):**
- Parses magnet URIs with info_hash (hex and base32)
- Supports display name, trackers, web seeds
- URL encoding/decoding utilities

**Tracker Client (`src/torrent/tracker.rs`):**
- HTTP tracker announce and scrape
- UDP tracker protocol (BEP 15) with connection handshake
- Compact and dictionary peer response parsing
- Azureus-style peer ID generation

**Peer Wire Protocol (`src/torrent/peer.rs`):**
- Full message encoding/decoding
- Handshake with extension negotiation
- Connection state management (choking, interested)
- Bitfield tracking for peer pieces

**Piece Manager (`src/torrent/piece.rs`):**
- Block-level piece assembly
- SHA-1 hash verification
- Rarest-first piece selection
- Endgame mode support
- File writing with proper offset handling

### Bencode Format

```
Bencode encoding:
- Integers:   i<number>e        Example: i42e
- Strings:    <length>:<data>   Example: 4:spam
- Lists:      l<items>e         Example: l4:spami42ee
- Dicts:      d<pairs>e         Example: d3:cow3:moo4:spam4:eggse

Example torrent file structure:
d
  8:announce    -> URL string
  13:announce-list -> list of lists of URLs
  7:comment    -> optional string
  10:created by -> optional string
  13:creation date -> optional integer
  4:info -> dictionary
    6:length   -> integer (single file)
    4:name     -> string
    12:piece length -> integer
    6:pieces   -> string (concatenated SHA-1 hashes)
    5:files    -> list (multi-file mode)
      d
        6:length -> integer
        4:path   -> list of strings
      e
e
```

### Bencode Parser

```rust
// src/torrent/bencode.rs

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub enum BencodeValue {
    Integer(i64),
    Bytes(Vec<u8>),
    List(Vec<BencodeValue>),
    Dict(BTreeMap<Vec<u8>, BencodeValue>),
}

impl BencodeValue {
    /// Parse bencode from bytes
    pub fn parse(data: &[u8]) -> Result<(Self, &[u8])>;

    /// Encode to bencode bytes
    pub fn encode(&self) -> Vec<u8>;

    /// Get as string (UTF-8)
    pub fn as_string(&self) -> Option<&str>;

    /// Get as integer
    pub fn as_int(&self) -> Option<i64>;

    /// Get as bytes
    pub fn as_bytes(&self) -> Option<&[u8]>;

    /// Get as list
    pub fn as_list(&self) -> Option<&[BencodeValue]>;

    /// Get as dict
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, BencodeValue>>;

    /// Get dict value by key
    pub fn get(&self, key: &str) -> Option<&BencodeValue>;
}
```

### Metainfo Parser

```rust
// src/torrent/metainfo.rs

pub struct Metainfo {
    pub info_hash: [u8; 20],
    pub info: Info,
    pub announce: Option<String>,
    pub announce_list: Vec<Vec<String>>,
    pub creation_date: Option<i64>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
}

pub struct Info {
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>,  // SHA-1 hashes
    pub files: Vec<FileInfo>,
    pub total_size: u64,
}

pub struct FileInfo {
    pub path: PathBuf,
    pub length: u64,
    pub offset: u64,  // Offset in the concatenated file stream
}

impl Metainfo {
    /// Parse from .torrent file bytes
    pub fn parse(data: &[u8]) -> Result<Self>;

    /// Calculate info_hash (SHA-1 of bencoded info dict)
    fn calculate_info_hash(info_dict: &BencodeValue) -> [u8; 20];

    /// Get piece hash for piece index
    pub fn piece_hash(&self, index: usize) -> Option<&[u8; 20]>;

    /// Get files that overlap with a piece
    pub fn files_for_piece(&self, index: usize) -> Vec<(usize, u64, u64)>;
}
```

### Magnet URI Parser

```rust
// src/torrent/magnet.rs

pub struct MagnetUri {
    pub info_hash: [u8; 20],
    pub display_name: Option<String>,
    pub trackers: Vec<String>,
    pub web_seeds: Vec<String>,
    pub exact_length: Option<u64>,
}

impl MagnetUri {
    /// Parse magnet URI
    /// magnet:?xt=urn:btih:<hash>&dn=<name>&tr=<tracker>
    pub fn parse(uri: &str) -> Result<Self>;

    /// Convert to magnet URI string
    pub fn to_string(&self) -> String;
}

// Example:
// magnet:?xt=urn:btih:HASH&dn=Name&tr=http://tracker.example.com/announce
```

### Tracker Protocol

```rust
// src/torrent/tracker.rs

pub struct TrackerClient {
    http_client: reqwest::Client,
    peer_id: [u8; 20],
}

#[derive(Debug)]
pub struct AnnounceRequest {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: AnnounceEvent,
    pub compact: bool,
}

#[derive(Debug)]
pub enum AnnounceEvent {
    None,
    Started,
    Stopped,
    Completed,
}

#[derive(Debug)]
pub struct AnnounceResponse {
    pub interval: u32,
    pub min_interval: Option<u32>,
    pub peers: Vec<PeerAddr>,
    pub complete: Option<u32>,    // Seeders
    pub incomplete: Option<u32>,  // Leechers
}

impl TrackerClient {
    /// Announce to HTTP tracker
    pub async fn announce_http(&self, url: &str, req: &AnnounceRequest) -> Result<AnnounceResponse>;

    /// Announce to UDP tracker (BEP 15)
    pub async fn announce_udp(&self, url: &str, req: &AnnounceRequest) -> Result<AnnounceResponse>;

    /// Scrape tracker for stats
    pub async fn scrape(&self, url: &str, info_hashes: &[[u8; 20]]) -> Result<ScrapeResponse>;
}
```

### Peer Wire Protocol

```rust
// src/torrent/peer.rs

pub struct PeerConnection {
    stream: TcpStream,
    peer_id: [u8; 20],
    info_hash: [u8; 20],

    // State
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,

    // Bitfield
    have_pieces: BitVec,
}

#[derive(Debug)]
pub enum PeerMessage {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have { piece_index: u32 },
    Bitfield { bitfield: Vec<u8> },
    Request { index: u32, begin: u32, length: u32 },
    Piece { index: u32, begin: u32, block: Vec<u8> },
    Cancel { index: u32, begin: u32, length: u32 },
    Port { port: u16 },  // DHT port

    // Extensions (BEP 10)
    Extended { id: u8, payload: Vec<u8> },
}

impl PeerConnection {
    /// Perform handshake
    pub async fn handshake(
        stream: TcpStream,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
    ) -> Result<Self>;

    /// Send a message
    pub async fn send(&mut self, msg: PeerMessage) -> Result<()>;

    /// Receive a message
    pub async fn recv(&mut self) -> Result<PeerMessage>;

    /// Request a block
    pub async fn request_block(&mut self, piece: u32, offset: u32, length: u32) -> Result<()>;
}

// Handshake format:
// <pstrlen><pstr><reserved><info_hash><peer_id>
// 1 byte + 19 bytes + 8 bytes + 20 bytes + 20 bytes = 68 bytes
```

### Piece Manager

```rust
// src/torrent/piece.rs

pub struct PieceManager {
    metainfo: Arc<Metainfo>,
    have: BitVec,
    pending: HashMap<u32, PendingPiece>,
    verified: AtomicU64,
}

pub struct PendingPiece {
    index: u32,
    blocks: Vec<Option<Vec<u8>>>,
    block_size: u32,
    requested_at: Instant,
}

impl PieceManager {
    /// Check if we need a piece
    pub fn need_piece(&self, index: u32) -> bool;

    /// Select next piece to download (rarest first)
    pub fn select_piece(&self, peer_has: &BitVec) -> Option<u32>;

    /// Add received block
    pub fn add_block(&mut self, index: u32, offset: u32, data: Vec<u8>) -> Result<()>;

    /// Check if piece is complete
    pub fn piece_complete(&self, index: u32) -> bool;

    /// Verify piece hash
    pub fn verify_piece(&mut self, index: u32) -> Result<bool>;

    /// Write piece to disk
    pub async fn write_piece(&self, index: u32, files: &mut [File]) -> Result<()>;

    /// Calculate completion percentage
    pub fn progress(&self) -> f64;
}
```

### Build Verification

```bash
# Run bencode tests
cargo test bencode

# Test metainfo parsing with real torrents
cargo test metainfo -- --ignored

# Test tracker communication
cargo test tracker -- --ignored

# Test peer protocol with mock peer
cargo test peer
```

---

## Phase 4: BitTorrent P2P Features

**Status: ✅ COMPLETE**

### Goals
- DHT (Distributed Hash Table) - BEP 5
- PEX (Peer Exchange) - BEP 11
- LPD (Local Peer Discovery) - BEP 14
- Choking algorithm with optimistic unchoking
- Endgame mode
- Seeding with ratio enforcement

### Files Created

| File | Purpose |
|------|---------|
| `src/torrent/dht.rs` | DHT client using mainline crate ✅ |
| `src/torrent/pex.rs` | Peer Exchange implementation ✅ |
| `src/torrent/lpd.rs` | Local Peer Discovery (multicast) ✅ |
| `src/torrent/choking.rs` | Choking algorithm ✅ |

### DHT Integration

```rust
// src/torrent/dht.rs

use mainline::Dht;

pub struct DhtClient {
    dht: Dht,
}

impl DhtClient {
    /// Create and bootstrap DHT
    pub async fn new(bootstrap_nodes: &[String]) -> Result<Self>;

    /// Announce that we have a torrent
    pub async fn announce(&self, info_hash: [u8; 20], port: u16) -> Result<()>;

    /// Find peers for a torrent
    pub async fn get_peers(&self, info_hash: [u8; 20]) -> Result<Vec<SocketAddr>>;

    /// Shutdown DHT
    pub async fn shutdown(&self);
}
```

### Peer Exchange (BEP 11)

```rust
// src/torrent/pex.rs

pub struct PexManager {
    known_peers: HashSet<SocketAddr>,
    added_since_last: Vec<SocketAddr>,
    dropped_since_last: Vec<SocketAddr>,
}

impl PexManager {
    /// Process received PEX message
    pub fn process_pex(&mut self, added: &[u8], dropped: &[u8]);

    /// Generate PEX message for peer
    pub fn generate_pex(&mut self) -> (Vec<u8>, Vec<u8>);

    /// Add a new peer
    pub fn peer_connected(&mut self, addr: SocketAddr);

    /// Remove a peer
    pub fn peer_disconnected(&mut self, addr: SocketAddr);
}
```

### Local Peer Discovery (BEP 14)

```rust
// src/torrent/lpd.rs

pub struct LpdService {
    socket: UdpSocket,
    info_hashes: HashSet<[u8; 20]>,
}

// LPD uses multicast to 239.192.152.143:6771
const LPD_MULTICAST_ADDR: &str = "239.192.152.143:6771";

impl LpdService {
    /// Start LPD service
    pub async fn new() -> Result<Self>;

    /// Announce a torrent on local network
    pub async fn announce(&self, info_hash: [u8; 20], port: u16) -> Result<()>;

    /// Listen for local announcements
    pub async fn listen(&self) -> Result<(SocketAddr, [u8; 20])>;
}

// LPD message format:
// BT-SEARCH * HTTP/1.1\r\n
// Host: 239.192.152.143:6771\r\n
// Port: <port>\r\n
// Infohash: <hex info_hash>\r\n
// \r\n
```

### Choking Algorithm

```rust
// src/torrent/choking.rs

pub struct ChokingManager {
    peers: Vec<PeerState>,
    unchoked_count: usize,
    optimistic_unchoke_index: Option<usize>,
    last_recalc: Instant,
}

impl ChokingManager {
    /// Recalculate who to choke/unchoke
    /// Called every 10 seconds
    pub fn recalculate(&mut self) -> Vec<ChokingDecision>;

    /// Optimistic unchoke rotation
    /// Called every 30 seconds
    pub fn rotate_optimistic(&mut self) -> Option<usize>;
}

// Algorithm:
// 1. Sort peers by download rate (what they give us)
// 2. Unchoke top 4 peers
// 3. Keep 1 slot for optimistic unchoke (random peer)
// 4. Rotate optimistic unchoke every 30 seconds
```

### Endgame Mode

```rust
// src/torrent/piece.rs

impl PieceManager {
    /// Check if we should enter endgame mode
    /// (when only a few pieces remain)
    pub fn should_enter_endgame(&self) -> bool {
        let remaining = self.total_pieces - self.have.count_ones();
        remaining <= 10 && remaining > 0
    }

    /// In endgame, request same blocks from multiple peers
    pub fn endgame_requests(&self) -> Vec<(u32, u32, u32)>;

    /// Cancel duplicate requests when block received
    pub fn cancel_duplicates(&self, index: u32, offset: u32) -> Vec<PeerId>;
}
```

### Build Verification

```bash
# Test DHT with real network
cargo test dht -- --ignored

# Test LPD on local network
cargo test lpd -- --ignored

# Test PEX with mock peers
cargo test pex

# Full integration test with real torrent
cargo test torrent_integration -- --ignored
```

---

## Phase 5: Optimization & Distribution

**Status: 🔲 NOT STARTED**

### Goals
- io_uring support (Linux)
- Benchmarking suite
- Memory profiling
- Documentation
- crates.io publication
- Remove aria2 from Gosh-Fetch

### io_uring (Linux)

```rust
// src/http/io_uring.rs

#[cfg(all(target_os = "linux", feature = "io-uring"))]
pub struct IoUringDownloader {
    ring: io_uring::IoUring,
    // ...
}

// Feature flag in Cargo.toml:
// [features]
// io-uring = ["tokio-uring"]
```

### Benchmarks

```rust
// benches/download_bench.rs

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};

fn bench_segmented_download(c: &mut Criterion) {
    let mut group = c.benchmark_group("segmented_download");

    for connections in [1, 4, 8, 16] {
        group.bench_with_input(
            BenchmarkId::from_parameter(connections),
            &connections,
            |b, &conns| {
                b.iter(|| {
                    // Download 100MB test file
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_segmented_download);
criterion_main!(benches);
```

### Memory Profiling

```bash
# Install heaptrack
sudo apt install heaptrack  # Debian/Ubuntu

# Run with memory profiling
heaptrack ./target/release/gosh-dl-bench

# Analyze results
heaptrack_gui heaptrack.gosh-dl-bench.*.gz
```

### Documentation

```bash
# Generate and open docs
cargo doc --no-deps --open

# Check documentation coverage
cargo doc --document-private-items

# Run doc tests
cargo test --doc
```

### Publishing to crates.io

```bash
# Verify package
cargo package --list
cargo publish --dry-run

# Publish
cargo login
cargo publish
```

### aria2 Removal Checklist

```markdown
## Files to Delete
- [ ] src-tauri/src/aria2/ (entire directory)
- [ ] src-tauri/binaries/aria2c-*

## Files to Modify
- [ ] src-tauri/src/main.rs - Remove aria2 imports
- [ ] src-tauri/src/state.rs - Use DownloadEngine instead of Aria2Supervisor
- [ ] src-tauri/src/commands/download.rs - Use EngineAdapter
- [ ] src-tauri/src/commands/torrent.rs - Use EngineAdapter
- [ ] src-tauri/src/commands/settings.rs - Update for new engine
- [ ] .github/workflows/build-*.yml - Remove aria2 download steps

## Verification
- [ ] All existing tests pass
- [ ] Frontend works without changes
- [ ] HTTP downloads work
- [ ] Torrent downloads work
- [ ] Resume works after app restart
- [ ] Settings apply correctly
```

---

## Testing Guide

### Test Summary

| Category | Count | Description |
|----------|-------|-------------|
| Unit Tests | 60 | Core functionality tests in `src/` |
| Integration Tests | 18 | End-to-end tests using wiremock |
| Doc Tests | 1 | Example code verification |
| **Total** | **79** | All tests passing |

### Running Tests

```bash
# Run all tests (unit + integration + doc tests)
cargo test

# Run only unit tests
cargo test --lib

# Run only integration tests
cargo test --test integration_tests

# Run specific module tests
cargo test http
cargo test storage
cargo test config

# Run with output visible
cargo test -- --nocapture

# Run ignored (network-dependent) tests
cargo test -- --ignored

# Run a single test
cargo test test_basic_http_download
```

### Unit Tests (24 tests)

Located in `src/` modules with `#[cfg(test)]` blocks:

| Module | Tests | Description |
|--------|-------|-------------|
| `config.rs` | 4 | Configuration validation and builder |
| `http/segment.rs` | 3 | Segment calculation and initialization |
| `http/resume.rs` | 3 | Content-Range parsing, resume validation |
| `http/connection.rs` | 3 | Retry policy, speed calculator |
| `http/mod.rs` | 2 | Content-Disposition parsing, filename extraction |
| `storage/mod.rs` | 3 | Segment types, MemoryStorage |
| `storage/sqlite.rs` | 6 | SQLite CRUD, health checks, segments |

### Integration Tests (18 tests)

Located in `tests/integration_tests.rs`, using wiremock for HTTP mocking:

| Category | Tests | Description |
|----------|-------|-------------|
| **Basic Downloads** | 3 | Basic HTTP download, custom filename, Content-Disposition |
| **Event System** | 1 | Event sequence verification (Added → Started → Completed) |
| **Concurrent Downloads** | 2 | Multiple simultaneous downloads, limit enforcement |
| **Pause/Cancel** | 2 | Pause active download, cancel with file deletion |
| **Error Handling** | 3 | 404 errors, 500 errors, invalid URLs |
| **Engine Lifecycle** | 3 | Shutdown, config update, download listing |
| **Custom Headers** | 2 | User-Agent, Referer headers |
| **Progress Tracking** | 2 | Progress updates, global statistics |

#### Integration Test Details

```rust
// tests/integration_tests.rs

// Basic download tests
test_basic_http_download         // Download file and verify content
test_download_with_custom_filename // Override output filename
test_download_content_disposition_filename // Parse filename from headers

// Event system
test_download_events_sequence    // Verify Added → Started → Progress → Completed

// Concurrent downloads
test_concurrent_downloads        // 3 simultaneous downloads
test_concurrent_limit_respected  // Verify semaphore limits

// Pause/Cancel
test_pause_download              // Pause active download
test_cancel_download             // Cancel and delete files

// Error handling
test_download_404_error          // Handle 404 Not Found
test_download_500_error          // Handle 500 Server Error
test_invalid_url                 // Reject malformed URLs

// Engine lifecycle
test_engine_shutdown             // Graceful shutdown
test_config_update               // Update config at runtime
test_list_downloads              // List active/stopped downloads

// Custom headers
test_custom_user_agent           // Set User-Agent header
test_custom_referer              // Set Referer header

// Progress
test_progress_updates            // Verify progress events
test_global_stats                // Global statistics
```

### Test Coverage

```bash
# Install tarpaulin
cargo install cargo-tarpaulin

# Generate coverage report
cargo tarpaulin --out Html

# Open report
open tarpaulin-report.html

# Coverage with specific options
cargo tarpaulin --out Html --skip-clean --ignore-tests
```

### Mock Servers with Wiremock

Integration tests use `wiremock` to simulate HTTP servers:

```rust
use wiremock::{MockServer, Mock, ResponseTemplate};
use wiremock::matchers::{method, path, header};

#[tokio::test]
async fn test_example() {
    let mock_server = MockServer::start().await;
    let test_content = b"Hello, World!";

    // Mock HEAD request (for capability detection)
    Mock::given(method("HEAD"))
        .and(path("/file.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    // Mock GET request (for actual download)
    Mock::given(method("GET"))
        .and(path("/file.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .set_body_bytes(test_content.to_vec()),
        )
        .mount(&mock_server)
        .await;

    // Use mock_server.uri() as the download URL
    let url = format!("{}/file.txt", mock_server.uri());
    // ... test download
}
```

### Async Event Waiting

Tests use a helper function to wait for specific events:

```rust
async fn wait_for_event<F>(
    rx: &mut broadcast::Receiver<DownloadEvent>,
    predicate: F,
    timeout_duration: Duration,
) -> Option<DownloadEvent>
where
    F: Fn(&DownloadEvent) -> bool,
{
    let result = timeout(timeout_duration, async {
        loop {
            match rx.recv().await {
                Ok(event) if predicate(&event) => return Some(event),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }).await;
    result.unwrap_or(None)
}

// Usage:
let completed = wait_for_event(
    &mut events,
    |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
    Duration::from_secs(10),
).await;
assert!(completed.is_some(), "Download should complete");
```

### Slow/Delayed Response Testing

Test timeout and retry behavior with delayed responses:

```rust
Mock::given(method("GET"))
    .and(path("/slow-file.bin"))
    .respond_with(
        ResponseTemplate::new(200)
            .insert_header("Content-Length", "1024")
            .set_body_bytes(vec![0u8; 1024])
            .set_delay(Duration::from_secs(5)), // 5 second delay
    )
    .mount(&mock_server)
    .await;
```

### Error Simulation

Test error handling with HTTP error responses:

```rust
// 404 Not Found
Mock::given(method("GET"))
    .and(path("/not-found.txt"))
    .respond_with(ResponseTemplate::new(404))
    .mount(&mock_server)
    .await;

// 500 Server Error
Mock::given(method("GET"))
    .and(path("/server-error.txt"))
    .respond_with(ResponseTemplate::new(500))
    .mount(&mock_server)
    .await;
```

### Test Isolation

Each integration test uses `tempfile::TempDir` for isolation:

```rust
#[tokio::test]
async fn test_example() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };

    let engine = DownloadEngine::new(config).await.expect("Failed to create engine");

    // ... run test

    engine.shutdown().await.ok();
    // temp_dir automatically cleaned up when dropped
}
```

---

## API Reference

### DownloadEngine

```rust
impl DownloadEngine {
    /// Create new engine
    pub async fn new(config: EngineConfig) -> Result<Arc<Self>>;

    /// HTTP Downloads
    pub async fn add_http(&self, url: &str, opts: DownloadOptions) -> Result<DownloadId>;

    /// Torrent Downloads (Phase 3+)
    pub async fn add_torrent(&self, data: &[u8], opts: DownloadOptions) -> Result<DownloadId>;
    pub async fn add_magnet(&self, uri: &str, opts: DownloadOptions) -> Result<DownloadId>;

    /// Control
    pub async fn pause(&self, id: DownloadId) -> Result<()>;
    pub async fn resume(&self, id: DownloadId) -> Result<()>;
    pub async fn cancel(&self, id: DownloadId, delete_files: bool) -> Result<()>;

    /// Status
    pub fn status(&self, id: DownloadId) -> Option<DownloadStatus>;
    pub fn list(&self) -> Vec<DownloadStatus>;
    pub fn active(&self) -> Vec<DownloadStatus>;
    pub fn waiting(&self) -> Vec<DownloadStatus>;
    pub fn stopped(&self) -> Vec<DownloadStatus>;
    pub fn global_stats(&self) -> GlobalStats;

    /// Events
    pub fn subscribe(&self) -> broadcast::Receiver<DownloadEvent>;

    /// Configuration
    pub fn set_config(&self, config: EngineConfig) -> Result<()>;
    pub fn get_config(&self) -> EngineConfig;

    /// Lifecycle
    pub async fn shutdown(&self) -> Result<()>;
}
```

### Events

```rust
pub enum DownloadEvent {
    Added { id: DownloadId },
    Started { id: DownloadId },
    Progress { id: DownloadId, progress: DownloadProgress },
    StateChanged { id: DownloadId, old_state: DownloadState, new_state: DownloadState },
    Completed { id: DownloadId },
    Failed { id: DownloadId, error: String, retryable: bool },
    Removed { id: DownloadId },
    Paused { id: DownloadId },
    Resumed { id: DownloadId },
}
```

### Configuration

```rust
pub struct EngineConfig {
    pub download_dir: PathBuf,
    pub max_concurrent_downloads: usize,
    pub max_connections_per_download: usize,
    pub min_segment_size: u64,
    pub global_download_limit: Option<u64>,
    pub global_upload_limit: Option<u64>,
    pub user_agent: String,
    pub enable_dht: bool,
    pub enable_pex: bool,
    pub enable_lpd: bool,
    pub max_peers: usize,
    pub seed_ratio: f64,
    pub database_path: Option<PathBuf>,
    pub http: HttpConfig,
    pub torrent: TorrentConfig,
}
```

---

## Migration from aria2

### Command Mapping

| aria2 RPC | gosh-dl API |
|-----------|-------------|
| `aria2.addUri` | `engine.add_http()` |
| `aria2.addTorrent` | `engine.add_torrent()` |
| `aria2.addMetalink` | Not implemented |
| `aria2.pause` | `engine.pause()` |
| `aria2.unpause` | `engine.resume()` |
| `aria2.remove` | `engine.cancel(id, false)` |
| `aria2.forceRemove` | `engine.cancel(id, true)` |
| `aria2.tellStatus` | `engine.status()` |
| `aria2.tellActive` | `engine.active()` |
| `aria2.tellWaiting` | `engine.waiting()` |
| `aria2.tellStopped` | `engine.stopped()` |
| `aria2.getGlobalStat` | `engine.global_stats()` |
| `aria2.changeGlobalOption` | `engine.set_config()` |

### GID Compatibility

gosh-dl generates aria2-compatible GIDs (16-character hex strings) for frontend compatibility:

```rust
impl DownloadId {
    pub fn to_gid(&self) -> String {
        // First 8 bytes of UUID as hex
        hex::encode(&self.0.as_bytes()[0..8])
    }

    pub fn from_gid(gid: &str) -> Option<Self> {
        // Reconstruct UUID from GID
    }
}
```

### Session Migration

Existing aria2 sessions cannot be directly migrated. On upgrade:

1. Completed downloads: Already in SQLite history, no action needed
2. Active downloads: Will be lost; document this in release notes
3. Settings: Will be preserved and applied to new engine

---

## Troubleshooting

### Common Build Errors

#### Missing OpenSSL
```
error: failed to run custom build command for `openssl-sys`
```
Solution: Install OpenSSL development packages (see Prerequisites)

#### SQLite linking error
```
error: linking with `cc` failed
note: ld: library not found for -lsqlite3
```
Solution: We use bundled SQLite (`rusqlite` feature), ensure `build-essential` is installed

#### Ring compilation failure on ARM
```
error: failed to compile `ring`
```
Solution: Ensure you have the latest Rust and clang:
```bash
rustup update
sudo apt install clang
```

### Runtime Errors

#### "Address already in use" (torrent)
The DHT port is in use. Configure a different port range:
```rust
config.torrent.listen_port_range = (6890, 6899);
```

#### "Too many open files"
Increase ulimit:
```bash
ulimit -n 65535
```

#### Slow downloads
- Check connection limits
- Verify speed limits aren't too low
- For torrents, ensure DHT is enabled
- Check firewall settings

### Debug Logging

```rust
// Enable debug logging
std::env::set_var("RUST_LOG", "gosh_dl=debug");
env_logger::init();

// Or with tracing
tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .init();
```

---

## Contributing

### Code Style

```bash
# Format code
cargo fmt

# Lint
cargo clippy -- -D warnings

# Check for common mistakes
cargo clippy -- -W clippy::pedantic
```

### Commit Messages

```
feat: add segmented download support
fix: handle connection reset during download
docs: update API reference
test: add integration tests for DHT
perf: optimize piece verification
refactor: extract common HTTP logic
```

### Pull Request Checklist

- [ ] Code compiles without warnings
- [ ] All tests pass
- [ ] New code has tests
- [ ] Documentation updated
- [ ] CHANGELOG updated
- [ ] Version bumped if needed

---

## License

gosh-dl is licensed under AGPL-3.0. See LICENSE file for details.
