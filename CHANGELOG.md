# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Recursive HTTP directory mirroring behind the `recursive-http` feature flag
- `discover_http_recursive()` and `add_http_recursive()` engine APIs
- Recursive boundary types: `RecursiveOptions`, `RecursiveManifest`, `RecursiveJob`, `TrackedRecursiveJob`, `RecursiveJobStatus`, and `RecursiveJobEvent`
- Recursive parent job lifecycle APIs: `list_recursive_jobs()`, `recursive_job()`, `cancel_recursive_job()`, `remove_recursive_job()`, and `subscribe_recursive_jobs()`
- Dedicated recursive parent event stream separate from `DownloadEvent`
- SQLite schema v4 support for persisted tracked recursive jobs

### Changed
- Recursive HTTP child downloads now reuse the standard HTTP pipeline while carrying redirect-scope and fail-fast runtime metadata through persistence/restart

### Fixed
- Recursive enqueue is now transactional: if child creation fails partway through, already-added children are rolled back instead of being left orphaned
- Recursive redirect scope is enforced during discovery, child downloads, and resumed child downloads restored from storage

### Documentation
- Updated the README, technical spec, and recursive design/checklist docs to match the current shipped recursive feature set and remaining follow-up work

## [0.3.2] - 2026-03-11

### Fixed
- **Downloads bombing out on mid-stream connection drops**: body/decode errors from reqwest were classified as non-retryable `NetworkErrorKind::Other`, making the retry loop dead code for the most common failure mode — now correctly classified as `ConnectionReset` (retryable)
- **Double retryability bug in `From<reqwest::Error>`**: the `From` impl computed a narrow retryable set (only Timeout and ConnectionRefused) instead of using the standard `EngineError::network()` constructor which also covers ConnectionReset, Unreachable, 408, 429, and 5xx
- **Segment errors hardcoded as non-retryable**: segment request send and stream errors manually constructed `NetworkErrorKind::Other` instead of using proper reqwest error conversion — all segment errors now classify correctly
- **Segmented download aggregate error always non-retryable**: when segments failed with retryable errors (e.g. 500s), the aggregate error was wrapped as `Other` (non-retryable) — now preserves retryability from underlying segment errors
- **Single-stream downloads never retrying on stream errors**: `stream_to_file()` had no retry mechanism — stream errors now trigger automatic retry with resume (via Range requests when supported) or restart from byte 0 (when server lacks range support)
- **Segment progress lost on failure**: the engine error handler never saved segment progress, so retrying a failed segmented download restarted from zero — segment progress is now saved to both memory cache and database before marking failure
- **Sibling segments wasting bandwidth after fatal error**: when one segment hit a non-retryable error (403, 404), other segments continued downloading until completion — added a child cancellation token that stops siblings promptly

### Changed
- Default `max_retries` increased from 3 to 5 for better resilience on flaky connections

### Added
- 6 new integration tests: retry exhaustion with retryable flag, permanent 4xx non-retry, 416 fallback to single-stream, ETag change restart, sibling cancellation on fatal segment error, segment progress preservation on failure

## [0.3.1] - 2026-03-08

### Fixed
- **Segmented downloads stopping late when range support disappears**: segmented HTTP transfers now downgrade to a single stream and restart safely from byte `0` instead of failing after a server or CDN ignores a later `Range` request
- **Resume attempts failing hard on ignored `Range` responses**: single-connection HTTP resumes now restart cleanly from byte `0` when the server returns a full-body response instead of an appendable partial response
- **Poor diagnostics for `If-Range` mismatches**: ranged-response validation now distinguishes likely validator/resource changes from plain ignored ranges and surfaces restart-required errors without reintroducing over-100% progress

## [0.3.0] - 2026-03-08

### Fixed
- **HTTP progress still exceeding 100% on compressed responses**: disabled transparent `gzip`/`brotli` decoding for engine-managed downloads and now force `Accept-Encoding: identity` so byte accounting matches the on-the-wire `Content-Length`
- **Incorrect total size when `HEAD` and `GET` disagree**: single-connection downloads now prefer the actual `GET` response length, log mismatches, and fail early if streamed bytes exceed the expected total instead of drifting past completion
- **WebSeed byte accounting drift**: WebSeed requests now use the same identity-encoding behavior as HTTP downloads so torrent-backed web downloads keep progress aligned with transferred bytes

## [0.2.9] - 2026-03-08

### Fixed
- **HTTP progress exceeding 100%**: resumed and segmented downloads now reject `200 OK` responses to `Range` requests and require valid `206 Partial Content` + `Content-Range`, preventing duplicate byte accounting when servers falsely advertise range support
- **Segment retry policy not honoring retryable HTTP statuses**: `408`, `429`, and `5xx` responses are now marked retryable so segmented downloads actually retry transient server failures before surfacing an error
- **Progress invariant coverage**: added regression tests for lying range support, segmented retry/resume edge cases, and torrent progress bounds so `completed_size` no longer silently drifts past `total_size`

## [0.2.8] - 2026-03-06

### Changed
- Updated `rand` from 0.9 to 0.10 — avoids duplicate dependencies in downstream projects
  - Migrated from `Rng` trait to `RngExt` trait (renamed in 0.10)
- Tightened `mainline` version from `"6"` to `"6.1.0"` to prevent resolving unexpectedly old versions

## [0.2.7] - 2026-02-14

### Fixed
- **CI formatting failure**: `rustfmt` required line-wrapping on `tracing::debug!` macro calls added in v0.2.6

## [0.2.6] - 2026-02-14

### Fixed
- **Completed downloads lost on restart**: `persist_active_downloads()` only saved downloads where `is_active()` returned true (Downloading/Seeding/Connecting), so completed state was never written to the database — added event-driven persistence that saves state immediately when a download completes
- **Completed downloads invisible after reload**: `load_persisted_downloads()` had an explicit `continue` that skipped any download with `Completed` state, making completed downloads vanish from the UI on restart — removed the skip so completed downloads load as inert entries
- **Error state not persisted**: downloads that failed were also never persisted (same `is_active()` filter), so error state and messages were lost on restart — added persistence at all five error transition sites

## [0.2.5] - 2026-02-14

### Fixed
- **CI failures across all jobs**: removed phantom `test_http_large` example entry from Cargo.toml that referenced a file not committed to the repository
- **Clippy `too_many_arguments` lint**: added `#[allow]` on `SegmentedDownload::start()` which grew to 8 parameters after the retry policy addition
- **Code formatting**: ran `cargo fmt` on files modified in v0.2.4 (`engine.rs`, `segment.rs`, `webseed.rs`)

## [0.2.4] - 2026-02-13

### Fixed
- **Torrent completion event never fires**: added success-path handling after `run_peer_loop()` in both torrent and magnet download paths — downloads reaching 100% now correctly emit `DownloadEvent::Completed`
- **HTTP pause/resume loses progress without storage**: cached segment data in memory so resume works even without a `database_path` configured
- **WebSeed connections not counted in peer stats**: `progress()` now includes active WebSeed connections in the connected peers count
- **"Piece N not found in pending" race condition**: `verify_and_save()` now returns `Ok(false)` for duplicate/late blocks instead of erroring, preventing spurious failures in endgame mode
- **Slow torrent speeds**: increased `max_pending_requests` from 16 to 64, allowing higher throughput on fast connections
- **Dead tracker URLs in magnet example**: replaced non-functional tracker URLs in `magnet_smoke.rs` with operational ones

## [0.2.3] - 2026-02-13

### Fixed
- **Large file downloads failing at ~400MB**: the reqwest HTTP client was configured with `.timeout()` which sets a total request deadline (default 60s), not a per-read idle timeout — downloads taking longer than 60 seconds would be killed mid-stream; switched to `.read_timeout()` which resets after each successful read
- **Segment failures killing entire download**: wired the existing `RetryPolicy` into the segmented download path — each segment now retries with exponential backoff and resumes from the byte position already written instead of failing the whole download immediately
- **Rate limiter silent overflow**: speed limits were cast from `u64` to `u32` with truncation; values above `u32::MAX` now clamp instead of wrapping to zero
- **SQLite busy errors with concurrent readers**: added `busy_timeout(5s)` so external tools (GUI, CLI monitors) reading the database don't cause `SQLITE_BUSY` failures
- **Segment persistence not atomic**: wrapped `save_segments()` DELETE+INSERT in an explicit transaction to prevent corrupt resume data on crash
- **WebSeed unbounded memory growth**: replaced unbounded event channel with a bounded channel (`max_connections * 2` capacity) to apply backpressure when the consumer falls behind
- **WebSeed timeout same as main client**: applied the same `.timeout()` → `.read_timeout()` fix to the WebSeed HTTP client

## [0.2.2] - 2026-02-07

### Fixed
- **CI caching**: sanitize feature flag matrix values in cache keys to avoid commas rejected by `Swatinem/rust-cache`

## [0.2.1] - 2026-02-07

### Fixed
- **CI formatting**: ran `cargo fmt` across all source files to pass format check
- **MSRV bumped to 1.85**: `curve25519-dalek` v5 (transitive dep from `mainline`) requires Rust edition 2024, which needs Rust 1.85+

## [0.2.0] - 2026-02-07

This is a major milestone release that significantly improves the BitTorrent stack,
adds proper infrastructure, and restructures the public API.

### Added

#### Infrastructure
- **Feature flags**: `http`, `torrent`, `storage`, `full` — compile only what you need
- **SQLite schema versioning**: `PRAGMA user_version` with automatic migrations
- **GitHub Actions CI**: test matrix, fmt, clippy, MSRV verification
- **Fuzz targets**: bencode, metainfo, magnet URI, and content-disposition parsers

#### API & Ergonomics
- **`DownloadOptions` builder**: 16 chainable builder methods (`new`, `priority`, `save_dir`, `filename`, etc.)
- **Error helpers**: `is_not_found()`, `is_network()`, `is_shutdown()` on `EngineError`
- **Examples**: `http_download.rs`, `torrent_download.rs`, `progress_display.rs`

#### BitTorrent Protocol
- **uTP transport** (BEP 29): fully wired into peer connections with `TorrentConfig.enable_utp` opt-in flag; uTP-first with TCP fallback
- **IPv6 compact peers** (BEP 7): `peers6` key parsing in HTTP tracker responses (18-byte compact format)
- **Cross-file WebSeed pieces** (BEP 19): separate HTTP Range requests per file segment for pieces spanning multiple files
- **Torrent crash recovery**: resume downloads from SQLite-stored torrent data (schema v2 migration)
- **24 torrent integration tests** using MockPeer for handshake, bitfield, and piece serving

### Fixed
- **MSE cipher state sync**: RC4 cipher instances are now correctly reused across handshake phases instead of being re-derived
- **`DownloadId::from_gid()` round-trip**: documented lossy behavior (8/16 UUID bytes); added `matches_gid()` for safe comparison
- **Tracker panic on TLS failure**: `TrackerClient::new()` now returns `Result` instead of panicking
- **Peer connection timeout**: dedicated 10s constant (`PEER_CONNECT_TIMEOUT`) prevents indefinite hangs
- **DHT blocking**: `get_peers()` wrapped in `spawn_blocking` to avoid starving the Tokio runtime
- **uTP accept() remote address**: `PendingConnection` now stores the actual remote address instead of `0.0.0.0:0`
- **Magnet link resume**: verify existing files when metadata is received, so partial downloads resume instead of restarting from scratch (thanks to [@fentas](https://github.com/fentas) for [reporting and fixing this](https://github.com/goshitsarch-eng/gosh-dl/pull/9))

### Changed
- **Unified `&self` API**: all public methods now take `&self` via `Arc::new_cyclic` + `Weak<Self>` pattern (previously mixed `&self` and `&Arc<Self>`)
- **Internal visibility**: restricted sub-module exports with `pub(crate)` across http, torrent, storage, and lib modules
- **README**: restructured feature list into honest maturity tiers (Tested / Lightly Tested / Planned)

## [0.1.6] - 2026-01-24

### Changed
- Updated `rand` from 0.8 to 0.9
  - Migrated from `thread_rng().gen()` to `rng().random()` API
  - Migrated from `thread_rng().fill()` to `rng().fill()` API
- Updated `tokio-tungstenite` from 0.24 to 0.28
  - Adapted to `Message::Text` now using `Utf8Bytes` instead of `String`
- Updated `rusqlite` from 0.32 to 0.38
  - Fixed `u64` no longer implementing `ToSql` directly
- Updated `reqwest` from 0.12 to 0.13
  - Renamed `rustls-tls` feature to `rustls`
- Updated `governor` from 0.8 to 0.10
- Updated `socket2` from 0.5 to 0.6
- Updated `dirs` from 5 to 6
- Updated `bytes` from 1 to 1.11

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

[Unreleased]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.9...v0.3.0
[0.2.9]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.6...v0.2.0
[0.1.6]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/goshitsarch-eng/gosh-dl/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/goshitsarch-eng/gosh-dl/releases/tag/v0.1.0
