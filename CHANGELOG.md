# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.5] - 2026-01-12

### Fixed
- Fixed progress reporting exceeding 100% in BitTorrent endgame mode
  - Race condition in `verify_and_save()` allowed multiple threads to increment `verified_bytes` for the same piece
  - Now only the first thread to remove a piece from pending increments the counters

## [0.1.4] - 2025-01-10

### Added
- WebSocket Secure (WSS) tracker support for WebTorrent compatibility
  - Supports both `wss://` and `ws://` tracker URLs
  - JSON-based announce protocol with dictionary and compact peer formats
  - Full timeout handling and error reporting

### Fixed
- UDP tracker DNS resolution now uses async `tokio::net::lookup_host()` instead of blocking `std::net::ToSocketAddrs`
  - Prevents blocking the tokio runtime thread during DNS lookups
  - Improves performance and reliability for UDP tracker announces

### Dependencies
- Added `tokio-tungstenite` 0.24 for WebSocket support
- Added `base64` 0.22 for compact peer decoding in WSS responses

## [0.1.3] - 2024-12-XX

### Fixed
- Priority changes via `set_priority()` are now persisted immediately to the database
  - Previously, priority changes were only saved during the periodic 30-second persistence cycle
  - This ensures priority survives application restarts even if changed shortly before shutdown

### Documentation
- Added persistence note to `set_priority()` doc comment

## [0.1.2] - 2024-12-XX

### Security
- Updated `mainline` crate to v6 to resolve LRU cache soundness vulnerability
  - The previous version had a potential memory safety issue in the underlying LRU implementation

## [0.1.1] - 2024-12-XX

### Fixed
- Fixed missing `storage.delete_download(id)` call when canceling downloads
  - Previously, canceled downloads were not properly removed from the database
  - This caused orphaned entries that could accumulate over time

## [0.1.0] - 2024-12-XX

### Added
- Initial release of gosh-dl download engine library

#### HTTP/HTTPS Downloads
- Multi-connection segmented downloads (up to 16 parallel connections)
- Automatic resume with ETag/Last-Modified validation
- Connection pooling with token bucket rate limiting
- Custom headers (User-Agent, Referer, cookies)
- Mirror/fallback URL support with automatic failover
- Checksum verification (MD5, SHA256)
- Proxy support (HTTP, HTTPS, SOCKS5)

#### BitTorrent Protocol
- Full protocol support (BEP 3)
- Magnet URI parsing and metadata fetching (BEP 9)
- DHT for trackerless downloads (BEP 5)
- Peer Exchange (BEP 11)
- Local Peer Discovery (BEP 14)
- HTTP and UDP tracker support (BEP 3, BEP 15)
- WebSeeds (BEP 17 Hoffman-style, BEP 19 GetRight-style)
- Message Stream Encryption (MSE/PE)
- uTP transport protocol (BEP 29) with LEDBAT congestion control
- Private torrent handling (BEP 27)

#### Download Management
- Priority queue (Critical, High, Normal, Low)
- Bandwidth scheduling with time-based rules
- Partial torrent downloads (file selection)
- Sequential download mode for streaming
- File preallocation (none, sparse, full)

#### Reliability
- SQLite-based state persistence with WAL mode
- Automatic retry with exponential backoff and jitter
- Crash recovery and resume
- Segment-level progress tracking for HTTP downloads

[Unreleased]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/goshitsarch-eng/gosh-dl/releases/tag/v0.1.0
