# gosh-dl

A fast, embeddable download engine for Rust applications. Supports HTTP/HTTPS with multi-connection acceleration and full BitTorrent protocol including DHT, PEX, encryption, and WebSeeds.

[![Crates.io](https://img.shields.io/crates/v/gosh-dl.svg)](https://crates.io/crates/gosh-dl)
[![Documentation](https://docs.rs/gosh-dl/badge.svg)](https://docs.rs/gosh-dl)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## Why gosh-dl?

gosh-dl brings download functionality directly into your Rust application as a native library, eliminating the complexity of managing external processes, parsing JSON-RPC responses, or bundling platform-specific binaries. Modern applications demand seamless integration, and gosh-dl delivers exactly that; async function calls that feel natural in your codebase, compile-time type safety that catches errors before runtime, and shared memory that keeps your application lightweight and responsive.

Whether you're building a media application that needs BitTorrent with streaming support, a package manager requiring resilient HTTP downloads with checksums and mirrors, or any software that moves files across the network, gosh-dl provides the complete feature set you need. Multi-connection acceleration splits large downloads across parallel connections for maximum throughput. Automatic resume with ETag validation ensures interrupted transfers pick up exactly where they left off. Full BitTorrent support includes DHT for trackerless operation, peer exchange for efficient swarm discovery, and protocol encryption for privacy.

The engine handles the complexity of segmented downloads, tracker communication, DHT peer discovery, and connection encryption while exposing a clean, intuitive API that integrates naturally with Tokio-based applications. Priority queues let you control which downloads matter most, bandwidth scheduling adapts to time-of-day constraints, and SQLite persistence ensures nothing is lost across restarts.

A standalone CLI application is coming soon for those who need command-line access to these capabilities.

## Features

### HTTP/HTTPS Downloads
- Multi-connection segmented downloads (up to 16 parallel connections)
- Automatic resume with ETag/Last-Modified validation
- Connection pooling with token bucket rate limiting
- Custom headers (User-Agent, Referer, cookies)
- Mirror/fallback URL support with automatic failover
- Checksum verification (MD5, SHA256)
- Proxy support (HTTP, HTTPS, SOCKS5)

### BitTorrent Protocol
- Full protocol support (BEP 3)
- Magnet URI parsing and metadata fetching (BEP 9)
- DHT for trackerless downloads (BEP 5)
- Peer Exchange (BEP 11)
- Local Peer Discovery (BEP 14)
- UDP tracker support (BEP 15)
- WebSeeds (BEP 17 Hoffman-style, BEP 19 GetRight-style)
- Message Stream Encryption (MSE/PE)
- uTP transport protocol (BEP 29) with LEDBAT congestion control
- Private torrent handling (BEP 27)

### Download Management
- Priority queue (Critical, High, Normal, Low)
- Bandwidth scheduling with time-based rules
- Partial torrent downloads (file selection)
- Sequential download mode for streaming
- File preallocation (none, sparse, full)

### Reliability
- SQLite-based state persistence with WAL mode
- Automatic retry with exponential backoff and jitter
- Crash recovery and resume
- Segment-level progress tracking for HTTP downloads

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
gosh-dl = "0.1"
tokio = { version = "1", features = ["full"] }
```

Basic usage:

```rust
use gosh_dl::{DownloadEngine, EngineConfig, DownloadOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = DownloadEngine::new(EngineConfig::default()).await?;

    // HTTP download
    let id = engine.add_http(
        "https://example.com/file.zip",
        DownloadOptions::default(),
    ).await?;

    // Subscribe to progress events
    let mut events = engine.subscribe();
    while let Ok(event) = events.recv().await {
        println!("Event: {:?}", event);
    }

    Ok(())
}
```

## API Overview

### Download Management

```rust
// Add downloads
let http_id = engine.add_http(url, options).await?;
let torrent_id = engine.add_torrent(&torrent_bytes, options).await?;
let magnet_id = engine.add_magnet(magnet_uri, options).await?;

// Control
engine.pause(id).await?;
engine.resume(id).await?;
engine.cancel(id, delete_files).await?;

// Priority
engine.set_priority(id, DownloadPriority::High)?;

// Status
let status = engine.status(id);
let all = engine.list();
let active = engine.active();
let waiting = engine.waiting();
let stopped = engine.stopped();
let stats = engine.global_stats();
```

### Download Options

```rust
use gosh_dl::{DownloadOptions, DownloadPriority};
use gosh_dl::http::ExpectedChecksum;

let options = DownloadOptions {
    priority: DownloadPriority::High,
    save_dir: Some(PathBuf::from("/downloads")),
    filename: Some("custom_name.zip".to_string()),
    user_agent: Some("MyApp/1.0".to_string()),
    referer: Some("https://example.com".to_string()),
    headers: vec![("Authorization".to_string(), "Bearer token".to_string())],
    cookies: Some(vec!["session=abc123".to_string()]),
    checksum: ExpectedChecksum::parse("sha256:abcd1234..."),
    mirrors: vec!["https://mirror1.example.com/file.zip".to_string()],
    max_connections: Some(8),
    max_download_speed: Some(5 * 1024 * 1024), // 5 MB/s
    // Torrent-specific
    selected_files: Some(vec![0, 2, 5]), // Download only specific files
    sequential: Some(true), // For streaming playback
    ..Default::default()
};
```

### Events

```rust
use gosh_dl::DownloadEvent;

let mut events = engine.subscribe();
while let Ok(event) = events.recv().await {
    match event {
        DownloadEvent::Added { id } => println!("Added: {}", id),
        DownloadEvent::Started { id } => println!("Started: {}", id),
        DownloadEvent::Progress { id, progress } => {
            println!("{}: {:.1}% at {} KB/s",
                id,
                progress.percentage(),
                progress.download_speed / 1024
            );
        }
        DownloadEvent::StateChanged { id, old_state, new_state } => {
            println!("{}: {:?} -> {:?}", id, old_state, new_state);
        }
        DownloadEvent::Completed { id } => println!("Done: {}", id),
        DownloadEvent::Failed { id, error, retryable } => {
            eprintln!("Failed {}: {} (retryable: {})", id, error, retryable);
        }
        DownloadEvent::Paused { id } => println!("Paused: {}", id),
        DownloadEvent::Resumed { id } => println!("Resumed: {}", id),
        DownloadEvent::Removed { id } => println!("Removed: {}", id),
    }
}
```

## Configuration

```rust
use gosh_dl::{EngineConfig, HttpConfig, DownloadPriority};
use std::path::PathBuf;

let config = EngineConfig {
    download_dir: PathBuf::from("/downloads"),
    max_concurrent_downloads: 5,
    max_connections_per_download: 16,
    min_segment_size: 1024 * 1024, // 1 MB
    global_download_limit: Some(10 * 1024 * 1024), // 10 MB/s
    global_upload_limit: Some(5 * 1024 * 1024), // 5 MB/s
    user_agent: "MyApp/1.0".to_string(),
    enable_dht: true,
    enable_pex: true,
    enable_lpd: true,
    max_peers: 55,
    seed_ratio: 1.0,
    database_path: Some(PathBuf::from("/data/gosh-dl.db")),
    ..Default::default()
};
```

### Bandwidth Scheduling

```rust
use gosh_dl::ScheduleRule;

// Limit bandwidth during work hours
let work_hours = ScheduleRule::new()
    .start_hour(9)
    .end_hour(17)
    .weekdays()
    .download_limit(Some(1024 * 1024)); // 1 MB/s

let config = EngineConfig::default()
    .add_schedule_rule(work_hours);
```

## Building

```bash
cargo build --release
cargo test
cargo doc --open
```

See [technical_spec.md](technical_spec.md) for architecture details.

---

## Comparison with aria2

gosh-dl was designed as a native Rust alternative to [aria2](https://aria2.github.io/), the popular C++ download utility. While aria2 is excellent as a standalone tool, embedding it in applications requires spawning an external process and communicating via JSON-RPC.

| Aspect | aria2 | gosh-dl |
|--------|-------|---------|
| **Integration** | External process + JSON-RPC | Native library calls |
| **Deployment** | Bundle platform binaries | Single Rust crate |
| **Type Safety** | JSON strings | Rust types with compile-time checks |
| **Error Handling** | Parse JSON responses | Native `Result<T, E>` |
| **Process Management** | Handle lifecycle, crashes | None required |
| **Memory** | Separate process | Shared with your app |

### Migration Guide

| aria2 RPC | gosh-dl |
|-----------|---------|
| `aria2.addUri(urls)` | `engine.add_http(url, opts)` |
| `aria2.addTorrent(torrent)` | `engine.add_torrent(bytes, opts)` |
| `aria2.pause(gid)` | `engine.pause(id)` |
| `aria2.unpause(gid)` | `engine.resume(id)` |
| `aria2.remove(gid)` | `engine.cancel(id, false)` |
| `aria2.tellStatus(gid)` | `engine.status(id)` |
| `aria2.tellActive()` | `engine.active()` |
| `aria2.tellWaiting()` | `engine.waiting()` |
| `aria2.tellStopped()` | `engine.stopped()` |
| `aria2.getGlobalStat()` | `engine.global_stats()` |
| `aria2.changeOption(gid, {priority})` | `engine.set_priority(id, priority)` |

---

## License

MIT License - see [LICENSE](LICENSE) for details.

## Acknowledgments

- Built with [Tokio](https://tokio.rs/) for async runtime
- Uses [mainline](https://crates.io/crates/mainline) for DHT support
