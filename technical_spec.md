# gosh-dl Technical Specification

A native Rust download engine supporting HTTP/HTTPS and BitTorrent protocols.

---

## Table of Contents

1. [Architecture](#architecture)
2. [Core API](#core-api)
3. [HTTP Implementation](#http-implementation)
4. [BitTorrent Implementation](#bittorrent-implementation)
5. [Storage Layer](#storage-layer)
6. [Scheduling and Priority](#scheduling-and-priority)
7. [Configuration](#configuration)

---

## Architecture

### Design Principles

- **Library-first**: Reusable crate, not tied to any specific application
- **Async-native**: Built on Tokio for efficient concurrent operations
- **Type-safe**: Strong typing throughout, minimal runtime errors
- **Observable**: Event-driven architecture for progress tracking

### Module Structure

```
src/
├── lib.rs                 # Public API, re-exports
├── engine.rs              # DownloadEngine - main coordinator
├── types.rs               # Core types (DownloadId, Status, Progress)
├── error.rs               # Typed error hierarchy
├── config.rs              # EngineConfig and sub-configs
├── priority_queue.rs      # Priority-based download scheduling
├── scheduler.rs           # Time-based bandwidth scheduling
│
├── http/                  # HTTP download engine
│   ├── mod.rs             # HttpDownloader
│   ├── segment.rs         # Segmented download logic
│   ├── connection.rs      # Connection pooling, rate limiting
│   ├── resume.rs          # Resume detection and validation
│   ├── mirror.rs          # Mirror failover management
│   └── checksum.rs        # MD5/SHA256 verification
│
├── torrent/               # BitTorrent engine
│   ├── mod.rs             # TorrentDownloader
│   ├── bencode.rs         # Bencode parser
│   ├── metainfo.rs        # Torrent file parser
│   ├── magnet.rs          # Magnet URI parser
│   ├── metadata.rs        # BEP 9 metadata fetching
│   ├── tracker.rs         # HTTP/UDP tracker clients
│   ├── peer.rs            # Peer wire protocol
│   ├── piece.rs           # Piece management
│   ├── dht.rs             # DHT client (BEP 5)
│   ├── pex.rs             # Peer Exchange (BEP 11)
│   ├── lpd.rs             # Local Peer Discovery (BEP 14)
│   ├── choking.rs         # Choking algorithm
│   ├── mse.rs             # Message Stream Encryption
│   ├── webseed.rs         # WebSeed support (BEP 17/19)
│   ├── transport.rs       # Transport abstraction
│   └── utp/               # uTP protocol (BEP 29)
│       ├── mod.rs
│       ├── socket.rs      # Connection management
│       ├── packet.rs      # Packet encoding/decoding
│       ├── congestion.rs  # LEDBAT algorithm
│       ├── state.rs       # Connection state machine
│       └── multiplexer.rs # UDP socket sharing
│
└── storage/               # Persistence layer
    ├── mod.rs             # Storage trait + MemoryStorage
    └── sqlite.rs          # SQLite backend
```

### Data Flow

```
User Request
     │
     ▼
┌────────────────┐
│ DownloadEngine │ ◄─── Configuration
└───────┬────────┘
        │
        ▼
┌───────────────────┐
│  Priority Queue   │ ◄─── Concurrent limit enforcement
└───────┬───────────┘
        │
   ┌────┴────┐
   ▼         ▼
┌──────┐  ┌─────────┐
│ HTTP │  │ Torrent │
└──┬───┘  └────┬────┘
   │           │
   │    ┌──────┼──────┬──────────┐
   │    ▼      ▼      ▼          ▼
   │ Tracker  DHT    PEX       LPD
   │    └──────┴──────┴──────────┘
   │                  │
   │           ┌──────┴──────┐
   │           ▼             ▼
   │        Peers        WebSeeds
   │           │             │
   └───────────┴─────────────┘
               │
               ▼
        ┌─────────────┐
        │   Storage   │
        │  (SQLite)   │
        └─────────────┘
               │
               ▼
          Event Stream
```

---

## Core API

### DownloadEngine

```rust
impl DownloadEngine {
    // Lifecycle
    pub async fn new(config: EngineConfig) -> Result<Arc<Self>>;
    pub async fn shutdown(&self) -> Result<()>;

    // Downloads
    pub async fn add_http(&self, url: &str, opts: DownloadOptions) -> Result<DownloadId>;
    pub async fn add_torrent(&self, data: &[u8], opts: DownloadOptions) -> Result<DownloadId>;
    pub async fn add_magnet(&self, uri: &str, opts: DownloadOptions) -> Result<DownloadId>;

    // Control
    pub async fn pause(&self, id: DownloadId) -> Result<()>;
    pub async fn resume(&self, id: DownloadId) -> Result<()>;
    pub async fn cancel(&self, id: DownloadId, delete_files: bool) -> Result<()>;

    // Priority
    pub fn set_priority(&self, id: DownloadId, priority: DownloadPriority) -> Result<()>;
    pub fn get_priority(&self, id: DownloadId) -> Option<DownloadPriority>;

    // Status
    pub fn status(&self, id: DownloadId) -> Option<DownloadStatus>;
    pub fn list(&self) -> Vec<DownloadStatus>;
    pub fn active(&self) -> Vec<DownloadStatus>;
    pub fn waiting(&self) -> Vec<DownloadStatus>;
    pub fn stopped(&self) -> Vec<DownloadStatus>;
    pub fn global_stats(&self) -> GlobalStats;

    // Bandwidth scheduling
    pub fn get_bandwidth_limits(&self) -> BandwidthLimits;
    pub fn set_schedule_rules(&self, rules: Vec<ScheduleRule>);
    pub fn get_schedule_rules(&self) -> Vec<ScheduleRule>;

    // Events
    pub fn subscribe(&self) -> broadcast::Receiver<DownloadEvent>;

    // Configuration
    pub fn set_config(&self, config: EngineConfig) -> Result<()>;
    pub fn get_config(&self) -> EngineConfig;
}
```

### Download States

```rust
pub enum DownloadState {
    Queued,                    // Waiting in priority queue
    Connecting,                // Establishing connection
    Downloading,               // Active transfer
    Seeding,                   // Torrent only: uploading to peers
    Paused,                    // Paused by user
    Completed,                 // Successfully finished
    Error {                    // Failed
        kind: String,
        message: String,
        retryable: bool,
    },
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

### Error Hierarchy

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
    DhtError, PexError, LpdError, MetadataError,
}
```

---

## HTTP Implementation

### Segmented Downloads

Supports up to 16 parallel connections per download with automatic fallback for servers that don't support range requests.

**Segment calculation:**

```rust
pub fn calculate_segment_count(total_size: u64, max_connections: usize, min_segment_size: u64) -> usize {
    if total_size == 0 { return 1; }
    let max_by_size = (total_size / min_segment_size) as usize;
    max_connections.min(max_by_size).max(1)
}
```

### Download Flow

```
HEAD Request → Content-Length, Accept-Ranges, ETag
    ↓
Calculate Segments (e.g., 100MB ÷ 16 = 6.25MB each)
    ↓
Spawn N tasks with Range headers
    ↓
Each task writes to correct file offset via seek
    ↓
Aggregate progress from all segments
    ↓
Verify checksum (if provided)
    ↓
Rename .part file on completion
```

### Resume Detection

Resume is validated using ETag/Last-Modified headers:

```rust
pub struct ResumeInfo {
    pub can_resume: bool,
    pub existing_size: u64,
    pub total_size: Option<u64>,
    pub supports_range: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub async fn check_resume(client: &Client, url: &str, part_path: &Path,
    saved_etag: Option<&str>, saved_last_modified: Option<&str>) -> Result<ResumeInfo>;
```

If server content has changed (ETag mismatch), download restarts from beginning.

### Speed Limiting

Uses token bucket algorithm via `governor` crate within `ConnectionPool`:

```rust
impl ConnectionPool {
    pub fn new(config: &HttpConfig, download_limit: Option<u64>, upload_limit: Option<u64>) -> Self;
    pub async fn acquire_download(&self, bytes: u64);  // Blocks until quota available
    pub async fn acquire_upload(&self, bytes: u64);
}
```

Rate limiting breaks large requests into 16KB chunks to prevent long blocking periods.

### Retry Policy

Exponential backoff with jitter:

```rust
pub struct RetryPolicy {
    pub max_attempts: u32,          // Default: 3
    pub initial_delay_ms: u64,      // Default: 1000
    pub max_delay_ms: u64,          // Default: 30000
    pub jitter_factor: f64,         // Default: 0.25 (±25%)
}

impl RetryPolicy {
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base = self.initial_delay_ms * 2u64.pow(attempt.min(10));
        let capped = base.min(self.max_delay_ms);
        let jitter = 1.0 + (rand() - 0.5) * 2.0 * self.jitter_factor;
        Duration::from_millis((capped as f64 * jitter) as u64)
    }
}
```

### Mirror Failover

```rust
pub struct MirrorManager {
    urls: Vec<String>,              // Primary + mirrors
    current_index: AtomicUsize,
    failed: RwLock<HashSet<usize>>,
    failure_counts: Vec<AtomicUsize>,
    max_failures: usize,            // Default: 3
}

impl MirrorManager {
    pub fn url_for_segment(&self, segment_idx: usize) -> String;  // Round-robin
    pub fn report_failure(&self, url: &str);                      // Track failures
    pub fn report_success(&self, url: &str);                      // Reset counter
}
```

### Checksum Verification

```rust
pub enum ChecksumAlgorithm {
    Md5,
    Sha256,
}

pub struct ExpectedChecksum {
    pub algorithm: ChecksumAlgorithm,
    pub value: String,  // Hex-encoded
}

impl ExpectedChecksum {
    pub fn parse(s: &str) -> Option<Self>;  // "md5:abc123" or "sha256:def456"
}

pub async fn verify_checksum(path: &Path, expected: &ExpectedChecksum) -> Result<bool>;
pub async fn compute_checksum(path: &Path, algorithm: ChecksumAlgorithm) -> Result<String>;
```

---

## BitTorrent Implementation

### Bencode Format

```
Integers:   i<number>e        Example: i42e
Strings:    <length>:<data>   Example: 4:spam
Lists:      l<items>e         Example: l4:spami42ee
Dicts:      d<pairs>e         Example: d3:cow3:moo4:spam4:eggse
```

```rust
pub enum BencodeValue {
    Integer(i64),
    Bytes(Vec<u8>),
    List(Vec<BencodeValue>),
    Dict(BTreeMap<Vec<u8>, BencodeValue>),
}

impl BencodeValue {
    pub fn parse(data: &[u8]) -> Result<(Self, &[u8])>;
    pub fn parse_exact(data: &[u8]) -> Result<Self>;  // Consumes all input
}
```

### Metainfo (Torrent Files)

```rust
pub struct Metainfo {
    pub info_hash: [u8; 20],
    pub info: Info,
    pub announce: Option<String>,
    pub announce_list: Vec<Vec<String>>,  // Tiered trackers
    pub creation_date: Option<i64>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub encoding: Option<String>,
    pub url_list: Vec<String>,            // BEP 19 web seeds
    pub httpseeds: Vec<String>,           // BEP 17 web seeds
}

pub struct Info {
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<u8>,        // Concatenated SHA-1 hashes (20 bytes each)
    pub files: Vec<FileInfo>,
    pub total_size: u64,
    pub private: bool,          // BEP 27
}

pub struct FileInfo {
    pub path: PathBuf,
    pub length: u64,
    pub offset: u64,            // Byte offset in piece stream
}
```

### Magnet URIs

Format: `magnet:?xt=urn:btih:<hash>&dn=<name>&tr=<tracker>&ws=<webseed>`

```rust
pub struct MagnetUri {
    pub info_hash: [u8; 20],
    pub display_name: Option<String>,
    pub trackers: Vec<String>,
    pub web_seeds: Vec<String>,
    pub exact_length: Option<u64>,
    pub exact_source: Option<String>,
    pub keyword_topic: Option<String>,
}

impl MagnetUri {
    pub fn parse(uri: &str) -> Result<Self>;
    pub fn name(&self) -> String;                 // display_name or hex hash
    pub fn info_hash_hex(&self) -> String;
    pub fn to_uri(&self) -> String;               // Regenerate URI
}
```

Supports both hex (40 chars) and base32 (32 chars) encoded info hashes.

### Tracker Protocol

**HTTP Tracker (BEP 3):**

```rust
pub struct AnnounceRequest {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: AnnounceEvent,     // None, Started, Stopped, Completed
    pub compact: bool,            // Request compact peer format
    pub numwant: Option<u32>,
}

pub struct AnnounceResponse {
    pub interval: u32,            // Seconds until next announce (clamped 60-3600)
    pub min_interval: Option<u32>,
    pub peers: Vec<SocketAddr>,
    pub complete: Option<u32>,    // Seeders
    pub incomplete: Option<u32>,  // Leechers
    pub tracker_id: Option<String>,
}
```

**UDP Tracker (BEP 15):** Uses connection ID handshake with magic `0x41727101980`.

### Peer Wire Protocol

**Handshake (68 bytes):**
```
<pstrlen=19><pstr="BitTorrent protocol"><reserved=8><info_hash=20><peer_id=20>
```

Reserved bytes encode extensions: bit 20 = DHT (BEP 5), bit 44 = Fast Extension (BEP 6), bit 43 = Extension Protocol (BEP 10).

**Messages:**

```rust
pub enum PeerMessage {
    KeepAlive,                                          // Length: 0
    Choke,                                              // ID: 0
    Unchoke,                                            // ID: 1
    Interested,                                         // ID: 2
    NotInterested,                                      // ID: 3
    Have { piece_index: u32 },                          // ID: 4
    Bitfield { bitfield: BitVec },                      // ID: 5
    Request { index: u32, begin: u32, length: u32 },    // ID: 6
    Piece { index: u32, begin: u32, block: Vec<u8> },   // ID: 7
    Cancel { index: u32, begin: u32, length: u32 },     // ID: 8
    Port { port: u16 },                                 // ID: 9 (DHT port)
    Extended { id: u8, payload: Vec<u8> },              // ID: 20 (BEP 10)
}
```

Block size is fixed at 16KB (`BLOCK_SIZE = 16384`).

### Piece Manager

```rust
pub struct PieceManager {
    metainfo: Arc<Metainfo>,
    have: BitVec,                              // Verified pieces
    pending: HashMap<u32, PendingPiece>,       // In-progress downloads
    pending_webseed: HashMap<u32, ()>,         // Pieces requested from webseeds
    piece_rarity: Vec<u32>,                    // Per-piece peer count
    selected_pieces: Option<BitVec>,           // For partial downloads
    sequential_mode: bool,                     // Download in order
}

impl PieceManager {
    pub fn need_piece(&self, index: u32) -> bool;
    pub fn select_piece(&self, peer_has: &BitVec) -> Option<u32>;  // Rarest-first
    pub fn add_block(&mut self, index: u32, offset: u32, data: Vec<u8>) -> Result<bool>;
    pub fn verify_piece(&mut self, index: u32) -> Result<bool>;    // SHA-1
    pub fn set_selected_files(&mut self, indices: Option<&[usize]>);
    pub fn set_sequential(&mut self, enabled: bool);
}
```

**Selection algorithm:**
1. In sequential mode: returns lowest incomplete piece index
2. Otherwise: rarest-first (lowest `piece_rarity` count among needed pieces)

**Endgame Mode:** Activates when ≤10 pieces remain; allows duplicate requests from multiple peers.

### DHT (BEP 5)

Uses `mainline` crate for Mainline DHT:

```rust
pub struct DhtClient {
    dht: Dht,
    peer_cache: RwLock<HashMap<Sha1Hash, Vec<SocketAddr>>>,
}

impl DhtClient {
    pub async fn new(bootstrap_nodes: &[String]) -> Result<Self>;
    pub async fn find_peers(&self, info_hash: [u8; 20]) -> Vec<SocketAddr>;
    pub async fn find_peers_timeout(&self, info_hash: [u8; 20], timeout: Duration) -> Vec<SocketAddr>;
    pub async fn announce(&self, info_hash: [u8; 20], port: u16) -> Result<()>;
}
```

Default bootstrap nodes: `router.bittorrent.com:6881`, `router.utorrent.com:6881`, `dht.transmissionbt.com:6881`.

### PEX (BEP 11)

Peer Exchange via BEP 10 extension protocol:

```rust
pub struct PexState {
    known_peers: HashSet<SocketAddr>,
    last_send: Instant,
    min_interval: Duration,         // 60 seconds
}

pub struct PexMessage {
    pub added: Vec<SocketAddr>,     // IPv4 peers added
    pub added_flags: Vec<u8>,       // Flags per added peer
    pub dropped: Vec<SocketAddr>,   // Peers removed
    pub added6: Vec<SocketAddr>,    // IPv6 peers
    pub added6_flags: Vec<u8>,
    pub dropped6: Vec<SocketAddr>,
}

impl PexState {
    pub fn should_send(&self) -> bool;
    pub fn build_message(&mut self, current_peers: &HashSet<SocketAddr>) -> PexMessage;
}
```

Flags: `0x01` = prefers encryption, `0x02` = is seeder, `0x04` = supports uTP.

### LPD (BEP 14)

Local Peer Discovery via UDP multicast to `239.192.152.143:6771`:

```rust
pub struct LpdService {
    socket: UdpSocket,
    listen_port: u16,
    announce_interval: Duration,    // 5 minutes
}

impl LpdService {
    pub async fn new(listen_port: u16) -> Result<Self>;
    pub async fn announce(&self, info_hash: [u8; 20]) -> Result<()>;
    pub fn subscribe(&self) -> broadcast::Receiver<LocalPeer>;
}
```

Message format:
```
BT-SEARCH * HTTP/1.1\r\n
Host: 239.192.152.143:6771\r\n
Port: <port>\r\n
Infohash: <hex info_hash>\r\n
\r\n
```

### Metadata Fetching (BEP 9)

For magnet links, metadata is fetched from peers via ut_metadata extension:

```rust
pub struct MetadataFetcher {
    info_hash: [u8; 20],
    metadata_size: Option<usize>,
    pieces: Vec<Option<Vec<u8>>>,
    requested: HashSet<usize>,
}

impl MetadataFetcher {
    pub fn need_metadata(&self) -> bool;
    pub fn request_piece(&mut self, peer_id: &[u8]) -> Option<usize>;
    pub fn receive_piece(&mut self, index: usize, data: Vec<u8>) -> bool;
    pub fn is_complete(&self) -> bool;
    pub fn assemble(&self) -> Option<Vec<u8>>;  // Returns info dict
}
```

Validates assembled metadata by computing SHA-1 and comparing to info_hash.

### Choking Algorithm

Recalculates every 10 seconds, rotates optimistic unchoke every 30 seconds:

```rust
pub struct ChokingManager {
    unchoke_slots: usize,           // Default: 4
    optimistic_peer: Option<SocketAddr>,
    last_optimistic_rotation: Instant,
}

impl ChokingManager {
    pub fn recalculate(&mut self, peers: &mut [PeerStats], am_seeding: bool) -> Vec<ChokingDecision>;
}
```

**Algorithm:**
1. Sort peers by download rate (when downloading) or upload rate (when seeding)
2. Unchoke top N interested peers
3. Reserve one slot for optimistic unchoke (rotated every 30s)

### Message Stream Encryption (MSE/PE)

```rust
pub enum EncryptionPolicy {
    Disabled,   // Plaintext only
    Allowed,    // Accept both
    Preferred,  // Try encrypted, fall back to plain
    Required,   // Reject non-MSE peers
}

pub struct EncryptionConfig {
    pub policy: EncryptionPolicy,
    pub allow_plaintext: bool,
    pub allow_rc4: bool,
    pub min_padding: usize,         // Default: 0
    pub max_padding: usize,         // Default: 512
}
```

**Handshake process:**
1. Diffie-Hellman key exchange (768-bit prime)
2. Derive RC4 keys from shared secret + info_hash
3. Discard first 1024 bytes of RC4 keystream (RC4-drop1024)
4. Optional random padding for obfuscation

### WebSeeds (BEP 17/19)

```rust
pub enum WebSeedType {
    GetRight,   // BEP 19: URL is base path, append file path
    Hoffman,    // BEP 17: URL is base, use specific request format
}

pub struct WebSeedManager {
    seeds: Vec<WebSeed>,
    semaphore: Semaphore,           // Limits concurrent connections
    config: WebSeedConfig,
}

pub struct WebSeedConfig {
    pub enabled: bool,              // Default: true
    pub max_connections: usize,     // Default: 4
    pub timeout_seconds: u64,       // Default: 30
    pub max_failures: u32,          // Default: 5
}
```

WebSeeds download pieces via HTTP Range requests, with exponential backoff (2x, max 5min) on failures.

### uTP Protocol (BEP 29)

```rust
pub enum TransportPolicy {
    TcpOnly,
    UtpOnly,
    PreferUtp,      // Default
    PreferTcp,
}

pub struct UtpConfigSettings {
    pub enabled: bool,              // Default: true
    pub policy: TransportPolicy,
    pub tcp_fallback: bool,         // Default: true
    pub target_delay_us: u32,       // Default: 100000 (100ms)
    pub max_window_size: u32,       // Default: 1MB
    pub recv_window: u32,           // Default: 1MB
    pub enable_sack: bool,          // Default: true
}
```

Implements LEDBAT congestion control: delays extra traffic until network delay drops below target (100ms default).

---

## Storage Layer

### Storage Trait

```rust
#[async_trait]
pub trait Storage: Send + Sync {
    async fn save_download(&self, status: &DownloadStatus) -> Result<()>;
    async fn load_download(&self, id: DownloadId) -> Result<Option<DownloadStatus>>;
    async fn load_all(&self) -> Result<Vec<DownloadStatus>>;
    async fn delete_download(&self, id: DownloadId) -> Result<()>;
    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> Result<()>;
    async fn load_segments(&self, id: DownloadId) -> Result<Vec<Segment>>;
    async fn delete_segments(&self, id: DownloadId) -> Result<()>;
    async fn health_check(&self) -> Result<()>;
    async fn compact(&self) -> Result<()>;
}
```

### Segment State

```rust
pub enum SegmentState {
    Pending,
    Downloading,
    Completed,
    Failed { error: String, retries: u32 },
}

pub struct Segment {
    pub index: usize,
    pub start: u64,
    pub end: u64,
    pub downloaded: u64,
    pub state: SegmentState,
}
```

### SQLite Implementation

Uses WAL mode for crash safety and concurrent reads:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE downloads (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    state_error_kind TEXT,
    state_error_message TEXT,
    state_error_retryable INTEGER,
    priority TEXT DEFAULT 'normal',
    total_size INTEGER,
    completed_size INTEGER DEFAULT 0,
    download_speed INTEGER DEFAULT 0,
    upload_speed INTEGER DEFAULT 0,
    connections INTEGER DEFAULT 0,
    seeders INTEGER DEFAULT 0,
    peers INTEGER DEFAULT 0,
    name TEXT NOT NULL,
    url TEXT,
    magnet_uri TEXT,
    info_hash TEXT,
    save_dir TEXT NOT NULL,
    filename TEXT,
    user_agent TEXT,
    referer TEXT,
    headers_json TEXT,
    cookies_json TEXT,
    checksum_json TEXT,
    mirrors_json TEXT,
    etag TEXT,
    last_modified TEXT,
    created_at TEXT NOT NULL,
    completed_at TEXT
);

CREATE TABLE segments (
    download_id TEXT NOT NULL,
    segment_index INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    downloaded INTEGER DEFAULT 0,
    state TEXT NOT NULL,
    error_message TEXT,
    error_retries INTEGER DEFAULT 0,
    FOREIGN KEY (download_id) REFERENCES downloads(id) ON DELETE CASCADE,
    UNIQUE (download_id, segment_index)
);
```

**Crash recovery:** Downloads in `Downloading` state are restored as `Paused` with `downloaded` bytes preserved for resume.

---

## Scheduling and Priority

### Priority Queue

```rust
pub enum DownloadPriority {
    Low,        // -1
    Normal,     // 0 (default)
    High,       // 1
    Critical,   // 2
}

pub struct PriorityQueue {
    semaphore: Semaphore,           // Limits concurrent downloads
    waiting: BinaryHeap<QueueEntry>,
    active: HashMap<DownloadId, DownloadPriority>,
}

impl PriorityQueue {
    pub async fn acquire(&self, id: DownloadId, priority: DownloadPriority) -> PriorityPermit;
    pub fn set_priority(&self, id: DownloadId, priority: DownloadPriority) -> bool;
    pub fn queue_position(&self, id: DownloadId) -> Option<usize>;
    pub fn stats(&self) -> PriorityQueueStats;
}
```

**Ordering:** Higher priority first, then FIFO within same priority level.

### Bandwidth Scheduling

```rust
pub struct ScheduleRule {
    pub start_hour: u8,             // 0-23
    pub end_hour: u8,               // 0-23 (can wrap midnight)
    pub days: Vec<Weekday>,         // Empty = all days
    pub download_limit: Option<u64>,
    pub upload_limit: Option<u64>,
}

pub struct BandwidthScheduler {
    rules: Vec<ScheduleRule>,
    default_limits: BandwidthLimits,
    current_limits: RwLock<BandwidthLimits>,
}

impl BandwidthScheduler {
    pub fn get_limits(&self) -> BandwidthLimits;
    pub fn update(&self) -> bool;   // Returns true if limits changed
}
```

Rules are evaluated in order; first matching rule wins. Empty rules list uses defaults.

---

## Configuration

### EngineConfig

```rust
pub struct EngineConfig {
    pub download_dir: PathBuf,
    pub max_concurrent_downloads: usize,        // Default: 5
    pub max_connections_per_download: usize,    // Default: 16
    pub min_segment_size: u64,                  // Default: 1 MiB
    pub global_download_limit: Option<u64>,     // bytes/sec
    pub global_upload_limit: Option<u64>,       // bytes/sec
    pub schedule_rules: Vec<ScheduleRule>,      // Time-based limits
    pub user_agent: String,                     // Default: "gosh-dl/VERSION"
    pub enable_dht: bool,                       // Default: true
    pub enable_pex: bool,                       // Default: true
    pub enable_lpd: bool,                       // Default: true
    pub max_peers: usize,                       // Default: 55
    pub seed_ratio: f64,                        // Default: 1.0
    pub database_path: Option<PathBuf>,
    pub http: HttpConfig,
    pub torrent: TorrentConfig,
}
```

### HttpConfig

```rust
pub struct HttpConfig {
    pub connect_timeout: u64,                   // Default: 30 seconds
    pub read_timeout: u64,                      // Default: 60 seconds
    pub max_redirects: usize,                   // Default: 10
    pub max_retries: usize,                     // Default: 3
    pub retry_delay_ms: u64,                    // Default: 1000
    pub max_retry_delay_ms: u64,                // Default: 30000
    pub accept_invalid_certs: bool,             // Default: false
    pub proxy_url: Option<String>,              // HTTP/HTTPS/SOCKS5
}
```

### TorrentConfig

```rust
pub struct TorrentConfig {
    pub listen_port_range: (u16, u16),          // Default: (6881, 6889)
    pub dht_bootstrap_nodes: Vec<String>,
    pub allocation_mode: AllocationMode,        // Default: None
    pub tracker_update_interval: u64,           // Default: 1800 seconds
    pub peer_timeout: u64,                      // Default: 120 seconds
    pub max_pending_requests: usize,            // Default: 16
    pub enable_endgame: bool,                   // Default: true
    pub tick_interval_ms: u64,                  // Default: 100
    pub connect_interval_secs: u64,             // Default: 5
    pub choking_interval_secs: u64,             // Default: 10
    pub webseed: WebSeedConfig,
    pub encryption: EncryptionConfig,
    pub utp: UtpConfigSettings,
}

pub enum AllocationMode {
    None,       // Files grow as data arrives
    Sparse,     // Set file size, let OS handle
    Full,       // Preallocate with zeros
}
```

---

## License

MIT License - see [LICENSE](LICENSE) for details.
