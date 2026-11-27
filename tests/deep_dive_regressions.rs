#![cfg(feature = "http")]
//! Regression tests for defects found during the deep-dive audit that
//! followed 0.6.4, and for issues #20 and #21.
//!
//! Each test documents the original failure in its doc comment so the
//! behaviour it guards is clear without reading the fix.

use gosh_dl::storage::{async_trait, MemoryStorage, Segment, Storage};
use gosh_dl::{
    DownloadEngine, DownloadId, DownloadOptions, DownloadState, DownloadStatus, EngineConfig,
    EngineError, FileConflictPolicy, HttpConfig, StorageErrorKind,
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(dir: &TempDir, max_retries: usize) -> EngineConfig {
    EngineConfig {
        download_dir: dir.path().into(),
        http: HttpConfig {
            max_retries,
            retry_delay_ms: 10,
            max_retry_delay_ms: 20,
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn wait_for(
    engine: &DownloadEngine,
    id: DownloadId,
    timeout: Duration,
    mut done: impl FnMut(&DownloadState) -> bool,
) -> DownloadState {
    tokio::time::timeout(timeout, async {
        loop {
            let state = engine.status(id).expect("download should exist").state;
            if done(&state) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("download did not reach the expected state in time")
}

/// One parsed request, as seen by the scripted server below.
struct Request {
    method: String,
    /// Value of the `Range` header, e.g. `bytes=0-99`.
    range: Option<String>,
}

/// What the scripted server should do with a request.
struct Reply {
    delay: Duration,
    /// Raw bytes written to the socket (status line, headers, body).
    bytes: Vec<u8>,
}

impl Reply {
    fn now(bytes: Vec<u8>) -> Self {
        Self {
            delay: Duration::ZERO,
            bytes,
        }
    }
}

fn response(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// A raw one-request-per-connection HTTP server. wiremock always sends
/// self-consistent responses; several of these tests need a server that
/// misbehaves in a precise way.
async fn serve<F>(handler: F) -> SocketAddr
where
    F: Fn(&Request) -> Reply + Send + Sync + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let request = Request {
                    method: text.split(' ').next().unwrap_or_default().to_string(),
                    range: text.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("range")
                            .then(|| value.trim().to_string())
                    }),
                };
                let reply = handler(&request);
                if !reply.delay.is_zero() {
                    tokio::time::sleep(reply.delay).await;
                }
                let _ = sock.write_all(&reply.bytes).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

/// Original failure: resuming a single-connection download whose `.part`
/// file was already as long as the remote file (complete but never renamed,
/// e.g. after a crash between the final write and the rename, or after a
/// checksum failure) sent `Range: bytes=<len>-`, got `416 Range Not
/// Satisfiable`, and failed. Every later `resume()` did exactly the same, so
/// the download could never finish without deleting the `.part` by hand.
#[tokio::test]
async fn resuming_an_already_complete_partial_file_restarts_instead_of_failing() {
    let body: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let served = body.clone();
    let addr = serve(move |req| {
        let len = served.len();
        if req.method == "HEAD" {
            return Reply::now(response(
                "200 OK",
                &[
                    ("Content-Length", len.to_string()),
                    ("Accept-Ranges", "bytes".to_string()),
                ],
                b"",
            ));
        }
        match &req.range {
            Some(_) => Reply::now(response(
                "416 Range Not Satisfiable",
                &[
                    ("Content-Range", format!("bytes */{len}")),
                    ("Content-Length", "0".to_string()),
                ],
                b"",
            )),
            None => Reply::now(response(
                "200 OK",
                &[("Content-Length", len.to_string())],
                &served,
            )),
        }
    })
    .await;

    let dir = TempDir::new().unwrap();
    // The leftover from a previous session: all bytes present, not renamed.
    tokio::fs::write(dir.path().join("file.bin.part"), &body)
        .await
        .unwrap();

    let engine = DownloadEngine::new(config(&dir, 0)).await.unwrap();
    let id = engine
        .add_http(
            &format!("http://{addr}/file.bin"),
            DownloadOptions::default(),
        )
        .await
        .unwrap();

    let state = wait_for(&engine, id, Duration::from_secs(10), |s| {
        matches!(s, DownloadState::Completed | DownloadState::Error { .. })
    })
    .await;
    assert_eq!(
        state,
        DownloadState::Completed,
        "416 on resume must restart"
    );
    assert_eq!(
        tokio::fs::read(dir.path().join("file.bin")).await.unwrap(),
        body
    );
    engine.shutdown().await.unwrap();
}

/// Original failure: the single-connection path (small files, servers
/// without range support) returned the first `503` straight to the caller,
/// while the segmented path retried the same status with back-off. A brief
/// overload therefore failed exactly the downloads that are cheapest to
/// retry.
#[tokio::test]
async fn single_connection_download_retries_transient_statuses() {
    let body = b"small payload that fits in one connection".to_vec();
    let served = body.clone();
    let gets = Arc::new(AtomicUsize::new(0));
    let gets_seen = Arc::clone(&gets);
    let addr = serve(move |req| {
        if req.method == "HEAD" {
            return Reply::now(response(
                "200 OK",
                &[("Content-Length", served.len().to_string())],
                b"",
            ));
        }
        if gets_seen.fetch_add(1, Ordering::SeqCst) < 2 {
            return Reply::now(response(
                "503 Service Unavailable",
                &[("Content-Length", "0".to_string())],
                b"",
            ));
        }
        Reply::now(response(
            "200 OK",
            &[("Content-Length", served.len().to_string())],
            &served,
        ))
    })
    .await;

    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(config(&dir, 3)).await.unwrap();
    let id = engine
        .add_http(
            &format!("http://{addr}/small.bin"),
            DownloadOptions::default(),
        )
        .await
        .unwrap();

    let state = wait_for(&engine, id, Duration::from_secs(10), |s| {
        matches!(s, DownloadState::Completed | DownloadState::Error { .. })
    })
    .await;
    assert_eq!(state, DownloadState::Completed, "503 must be retried");
    assert_eq!(gets.load(Ordering::SeqCst), 3, "two failures, one success");
    assert_eq!(
        tokio::fs::read(dir.path().join("small.bin")).await.unwrap(),
        body
    );
    engine.shutdown().await.unwrap();
}

/// Original failure: a segment wrote each chunk to disk *before* checking
/// that it still fit inside the segment. A server that sent more bytes than
/// the requested range therefore overwrote the start of the next segment,
/// which another connection had already downloaded; the download then
/// failed, but a later resume trusted the neighbour's saved progress and
/// kept the garbage.
#[tokio::test]
async fn over_long_segment_body_cannot_overwrite_the_next_segment() {
    let body: Vec<u8> = (0..4096u32).map(|i| (i % 239) as u8).collect();
    let served = body.clone();
    let addr = serve(move |req| {
        let len = served.len();
        if req.method == "HEAD" {
            return Reply::now(response(
                "200 OK",
                &[
                    ("Content-Length", len.to_string()),
                    ("Accept-Ranges", "bytes".to_string()),
                ],
                b"",
            ));
        }
        match req.range.as_deref() {
            // First segment: a correct Content-Range, but 1024 bytes too
            // many. Delayed so the second segment is on disk first.
            Some("bytes=0-2047") => {
                let mut too_long = served[..2048].to_vec();
                too_long.extend_from_slice(&[0xEE; 1024]);
                Reply {
                    delay: Duration::from_millis(400),
                    bytes: response(
                        "206 Partial Content",
                        &[
                            ("Content-Range", format!("bytes 0-2047/{len}")),
                            ("Content-Length", too_long.len().to_string()),
                        ],
                        &too_long,
                    ),
                }
            }
            Some("bytes=2048-4095") => Reply::now(response(
                "206 Partial Content",
                &[
                    ("Content-Range", format!("bytes 2048-4095/{len}")),
                    ("Content-Length", "2048".to_string()),
                ],
                &served[2048..],
            )),
            other => panic!("unexpected range {other:?}"),
        }
    })
    .await;

    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(EngineConfig {
        min_segment_size: 1024,
        max_connections_per_download: 2,
        ..config(&dir, 0)
    })
    .await
    .unwrap();
    let id = engine
        .add_http(
            &format!("http://{addr}/two-segments.bin"),
            DownloadOptions::default(),
        )
        .await
        .unwrap();

    let state = wait_for(&engine, id, Duration::from_secs(10), |s| {
        matches!(s, DownloadState::Completed | DownloadState::Error { .. })
    })
    .await;
    assert!(
        matches!(state, DownloadState::Error { .. }),
        "an over-long segment is a protocol error, got {state:?}"
    );

    let part = tokio::fs::read(dir.path().join("two-segments.bin.part"))
        .await
        .expect("partial file is kept for resume");
    assert_eq!(
        &part[2048..4096],
        &body[2048..],
        "the second segment's bytes must not be overwritten by the first"
    );
    engine.shutdown().await.unwrap();
}

/// A [`Storage`] that can be told to stall: either while saving an *active*
/// status (what the periodic persistence task writes) or while loading
/// segments (what `resume()` awaits before it rebuilds the worker).
#[derive(Default)]
struct StallingStorage {
    inner: MemoryStorage,
    stall_active_saves: Duration,
    stall_segment_loads: Duration,
    /// Signalled when a stalled save of an active status has started.
    active_save_started: tokio::sync::Notify,
}

#[async_trait]
impl Storage for StallingStorage {
    async fn save_download(&self, status: &DownloadStatus) -> gosh_dl::Result<()> {
        if status.state.is_active() && !self.stall_active_saves.is_zero() {
            self.active_save_started.notify_one();
            tokio::time::sleep(self.stall_active_saves).await;
        }
        self.inner.save_download(status).await
    }
    async fn load_download(&self, id: DownloadId) -> gosh_dl::Result<Option<DownloadStatus>> {
        self.inner.load_download(id).await
    }
    async fn load_all(&self) -> gosh_dl::Result<Vec<DownloadStatus>> {
        self.inner.load_all().await
    }
    async fn delete_download(&self, id: DownloadId) -> gosh_dl::Result<()> {
        self.inner.delete_download(id).await
    }
    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> gosh_dl::Result<()> {
        self.inner.save_segments(id, segments).await
    }
    async fn load_segments(&self, id: DownloadId) -> gosh_dl::Result<Vec<Segment>> {
        if !self.stall_segment_loads.is_zero() {
            tokio::time::sleep(self.stall_segment_loads).await;
        }
        self.inner.load_segments(id).await
    }
    async fn delete_segments(&self, id: DownloadId) -> gosh_dl::Result<()> {
        self.inner.delete_segments(id).await
    }
    async fn save_torrent_data(&self, id: DownloadId, data: &[u8]) -> gosh_dl::Result<()> {
        self.inner.save_torrent_data(id, data).await
    }
    async fn load_torrent_data(&self, id: DownloadId) -> gosh_dl::Result<Option<Vec<u8>>> {
        self.inner.load_torrent_data(id).await
    }
    async fn health_check(&self) -> gosh_dl::Result<()> {
        self.inner.health_check().await
    }
    async fn compact(&self) -> gosh_dl::Result<()> {
        self.inner.compact().await
    }
}

/// A server whose GET never answers within the test: the download stays
/// active for as long as the test needs it to.
async fn stalled_server() -> SocketAddr {
    serve(|req| {
        if req.method == "HEAD" {
            return Reply::now(response(
                "200 OK",
                &[("Content-Length", "1024".to_string())],
                b"",
            ));
        }
        Reply {
            delay: Duration::from_secs(60),
            bytes: response("200 OK", &[("Content-Length", "1024".to_string())], b""),
        }
    })
    .await
}

/// Original failure: the periodic persistence task snapshots every active
/// download and then writes the snapshots one by one. A download cancelled
/// while its snapshot was waiting to be written had its storage record
/// deleted by `cancel()` and then *re-created* by the stale write, so the
/// cancelled download reappeared (as paused) the next time the application
/// started. The same race could overwrite a `Completed` record with
/// `Downloading`.
#[tokio::test]
async fn cancel_during_a_background_save_does_not_resurrect_the_download() {
    let addr = stalled_server().await;
    let dir = TempDir::new().unwrap();
    let storage = Arc::new(StallingStorage {
        stall_active_saves: Duration::from_millis(500),
        ..Default::default()
    });
    let engine = DownloadEngine::with_storage(config(&dir, 0), storage.clone())
        .await
        .unwrap();
    let id = engine
        .add_http(
            &format!("http://{addr}/slow.bin"),
            DownloadOptions::default(),
        )
        .await
        .unwrap();
    wait_for(&engine, id, Duration::from_secs(5), |s| s.is_active()).await;

    // The periodic save (every 5s) is now writing a `Downloading` snapshot.
    tokio::time::timeout(
        Duration::from_secs(10),
        storage.active_save_started.notified(),
    )
    .await
    .expect("periodic persistence should run");

    engine.cancel(id, false).await.unwrap();
    assert!(engine.status(id).is_none());

    // Let the stalled snapshot write land.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        storage.load_all().await.unwrap().is_empty(),
        "a cancelled download must not come back from storage"
    );
    engine.shutdown().await.unwrap();
}

/// Original failure: `resume()` checked the state, awaited storage, and only
/// then started rebuilding the worker, without marking the download as
/// being resumed. Two overlapping calls (a double-click, a retry timer
/// racing a user action) both passed the check; for torrents restored from
/// storage that built two downloaders writing the same files, one of them
/// orphaned and impossible to stop.
#[tokio::test]
async fn overlapping_resume_calls_start_a_single_worker() {
    let addr = stalled_server().await;
    let dir = TempDir::new().unwrap();
    let storage = Arc::new(StallingStorage {
        stall_segment_loads: Duration::from_millis(200),
        ..Default::default()
    });
    let engine = DownloadEngine::with_storage(config(&dir, 0), storage)
        .await
        .unwrap();
    let id = engine
        .add_http(
            &format!("http://{addr}/slow.bin"),
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let (first, second) = tokio::join!(engine.resume(id), engine.resume(id));
    let outcomes = [first, second];
    assert_eq!(
        outcomes.iter().filter(|r| r.is_ok()).count(),
        1,
        "exactly one resume may win: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|r| matches!(r, Err(EngineError::InvalidState { .. }))),
        "the loser must be told the download is already resuming: {outcomes:?}"
    );

    // The claim is released afterwards: pause + resume works normally.
    wait_for(&engine, id, Duration::from_secs(5), |s| s.is_active()).await;
    engine.pause(id).await.unwrap();
    engine.resume(id).await.unwrap();
    engine.cancel(id, false).await.unwrap();
    engine.shutdown().await.unwrap();
}

/// Original failure: zero-valued torrent timer settings passed validation
/// and then panicked `tokio::time::interval` inside the torrent task, which
/// died silently and left the download in `Downloading` forever. The MSE
/// padding range had the same problem (`min > max` panics the RNG).
#[tokio::test]
async fn configurations_that_would_panic_a_worker_are_rejected() {
    let dir = TempDir::new().unwrap();
    let base = config(&dir, 0);

    let mut zero_tick = base.clone();
    zero_tick.torrent.tick_interval_ms = 0;
    let mut zero_connect = base.clone();
    zero_connect.torrent.connect_interval_secs = 0;
    let mut zero_choke = base.clone();
    zero_choke.torrent.choking_interval_secs = 0;
    let mut inverted_padding = base.clone();
    inverted_padding.torrent.encryption.min_padding = 300;
    inverted_padding.torrent.encryption.max_padding = 100;
    let mut nan_ratio = base.clone();
    nan_ratio.seed_ratio = f64::NAN;

    for (name, bad) in [
        ("tick_interval_ms", zero_tick),
        ("connect_interval_secs", zero_connect),
        ("choking_interval_secs", zero_choke),
        ("padding", inverted_padding),
        ("seed_ratio", nan_ratio),
    ] {
        assert!(
            matches!(bad.validate(), Err(EngineError::InvalidInput { .. })),
            "{name} should be rejected"
        );
        assert!(DownloadEngine::new(bad).await.is_err(), "{name}");
    }
    assert!(base.validate().is_ok());
}

/// A server like a pre-signed S3 URL minted for `GET`: `HEAD` is refused
/// with `403`, while `GET` (with or without `Range`) works. Returns the
/// address and the `Range` header of every `GET` it served.
async fn get_only_server(
    body: Vec<u8>,
) -> (SocketAddr, Arc<std::sync::Mutex<Vec<Option<String>>>>) {
    let ranges = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&ranges);
    let addr = serve(move |req| {
        if req.method != "GET" {
            return Reply::now(response(
                "403 Forbidden",
                &[("Content-Length", "0".to_string())],
                b"",
            ));
        }
        seen.lock().unwrap().push(req.range.clone());
        let len = body.len();
        let Some(range) = req.range.as_deref() else {
            return Reply::now(response(
                "200 OK",
                &[("Content-Length", len.to_string())],
                &body,
            ));
        };
        let spec = range.strip_prefix("bytes=").expect("byte range");
        let (start, end) = spec.split_once('-').expect("start-end");
        let start: usize = start.parse().unwrap();
        let end: usize = if end.is_empty() {
            len - 1
        } else {
            end.parse().unwrap()
        };
        Reply::now(response(
            "206 Partial Content",
            &[
                ("Content-Range", format!("bytes {start}-{end}/{len}")),
                ("Content-Length", (end - start + 1).to_string()),
            ],
            &body[start..=end],
        ))
    })
    .await;
    (addr, ranges)
}

/// Original failure (issue #20): capabilities were probed with `HEAD` only.
/// URLs that answer `GET` but not `HEAD` (pre-signed S3-style URLs are
/// signed per method) looked like "unknown size, no range support", so they
/// were always fetched over a single connection.
#[tokio::test]
async fn get_only_urls_are_probed_with_a_ranged_get_and_download_segmented() {
    let body: Vec<u8> = (0..8192u32).map(|i| (i % 247) as u8).collect();
    let (addr, ranges) = get_only_server(body.clone()).await;

    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(EngineConfig {
        min_segment_size: 1024,
        max_connections_per_download: 4,
        ..config(&dir, 0)
    })
    .await
    .unwrap();
    let id = engine
        .add_http(
            &format!("http://{addr}/presigned.bin?X-Amz-Signature=abc"),
            DownloadOptions::default(),
        )
        .await
        .unwrap();

    let state = wait_for(&engine, id, Duration::from_secs(10), |s| {
        matches!(s, DownloadState::Completed | DownloadState::Error { .. })
    })
    .await;
    assert_eq!(state, DownloadState::Completed);
    assert_eq!(
        tokio::fs::read(dir.path().join("presigned.bin"))
            .await
            .unwrap(),
        body
    );

    let ranges = ranges.lock().unwrap().clone();
    assert!(
        ranges.iter().any(|r| r.as_deref() == Some("bytes=0-0")),
        "the size/range probe should fall back to a ranged GET: {ranges:?}"
    );
    let segments = ranges
        .iter()
        .filter(|r| matches!(r.as_deref(), Some(r) if r != "bytes=0-0"))
        .count();
    assert_eq!(segments, 4, "four ranged segment requests: {ranges:?}");
    assert!(
        ranges.iter().all(|r| r.is_some()),
        "no full-body GET should be needed: {ranges:?}"
    );
    engine.shutdown().await.unwrap();
}

/// Original failure (issue #20): with `HEAD` refused, an existing `.part`
/// file was never resumed - range support was unknown, so the partial file
/// was truncated and the download restarted from byte 0 every time.
#[tokio::test]
async fn get_only_urls_resume_an_existing_partial_file() {
    let body: Vec<u8> = (0..4096u32).map(|i| (i % 241) as u8).collect();
    let (addr, ranges) = get_only_server(body.clone()).await;

    let dir = TempDir::new().unwrap();
    // First half already on disk from an earlier, interrupted session.
    tokio::fs::write(dir.path().join("resume.bin.part"), &body[..2048])
        .await
        .unwrap();

    // Default 1 MiB minimum segment size: this stays single-connection.
    let engine = DownloadEngine::new(config(&dir, 0)).await.unwrap();
    let id = engine
        .add_http(
            &format!("http://{addr}/resume.bin"),
            DownloadOptions::default(),
        )
        .await
        .unwrap();

    let state = wait_for(&engine, id, Duration::from_secs(10), |s| {
        matches!(s, DownloadState::Completed | DownloadState::Error { .. })
    })
    .await;
    assert_eq!(state, DownloadState::Completed);
    assert_eq!(
        tokio::fs::read(dir.path().join("resume.bin"))
            .await
            .unwrap(),
        body
    );

    let ranges = ranges.lock().unwrap().clone();
    assert!(
        ranges.iter().any(|r| r.as_deref() == Some("bytes=2048-")),
        "the download must continue from the partial file: {ranges:?}"
    );
    assert!(
        ranges.iter().all(|r| r.is_some()),
        "the file must not be fetched from byte 0 again: {ranges:?}"
    );
    engine.shutdown().await.unwrap();
}

/// Issue #21: by default a finished download still replaces a file that
/// already has its name (unchanged behaviour), but the application can now
/// ask for the existing file to be kept and the download saved next to it.
#[tokio::test]
async fn rename_policy_keeps_an_existing_file_and_numbers_the_download() {
    let body: Vec<u8> = (0..2000u32).map(|i| (i % 233) as u8).collect();
    let (addr, _) = get_only_server(body.clone()).await;
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(config(&dir, 0)).await.unwrap();
    assert_eq!(engine.file_conflict_policy(), FileConflictPolicy::Overwrite);
    engine.set_file_conflict_policy(FileConflictPolicy::Rename);

    // Two files already occupy the name and the first numbered sibling.
    tokio::fs::write(dir.path().join("doc.bin"), b"mine")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("doc (1).bin"), b"also mine")
        .await
        .unwrap();

    let id = engine
        .add_http(
            &format!("http://{addr}/doc.bin"),
            DownloadOptions::default(),
        )
        .await
        .unwrap();
    let state = wait_for(&engine, id, Duration::from_secs(10), |s| {
        matches!(s, DownloadState::Completed | DownloadState::Error { .. })
    })
    .await;
    assert_eq!(state, DownloadState::Completed);

    assert_eq!(
        tokio::fs::read(dir.path().join("doc.bin")).await.unwrap(),
        b"mine"
    );
    assert_eq!(
        tokio::fs::read(dir.path().join("doc (1).bin"))
            .await
            .unwrap(),
        b"also mine"
    );
    assert_eq!(
        tokio::fs::read(dir.path().join("doc (2).bin"))
            .await
            .unwrap(),
        body
    );
    // The status reports where the data actually went.
    assert_eq!(
        engine.status(id).unwrap().metadata.filename.as_deref(),
        Some("doc (2).bin")
    );

    // The default policy still overwrites.
    engine.set_file_conflict_policy(FileConflictPolicy::Overwrite);
    let id = engine
        .add_http(
            &format!("http://{addr}/doc.bin"),
            DownloadOptions::default(),
        )
        .await
        .unwrap();
    wait_for(&engine, id, Duration::from_secs(10), |s| {
        matches!(s, DownloadState::Completed | DownloadState::Error { .. })
    })
    .await;
    assert_eq!(
        tokio::fs::read(dir.path().join("doc.bin")).await.unwrap(),
        body
    );
    engine.shutdown().await.unwrap();
}

/// Issue #21: with the `Fail` policy a name conflict is reported as an
/// error, nothing is overwritten, the finished data is kept, and once the
/// application has resolved the conflict `resume()` completes the download
/// without fetching the segments again.
#[tokio::test]
async fn fail_policy_reports_the_conflict_and_resume_finishes_without_refetching() {
    let body: Vec<u8> = (0..8192u32).map(|i| (i % 229) as u8).collect();
    let (addr, ranges) = get_only_server(body.clone()).await;
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(EngineConfig {
        min_segment_size: 1024,
        max_connections_per_download: 4,
        ..config(&dir, 0)
    })
    .await
    .unwrap();
    engine.set_file_conflict_policy(FileConflictPolicy::Fail);

    let existing = dir.path().join("big.bin");
    tokio::fs::write(&existing, b"created while downloading")
        .await
        .unwrap();

    let id = engine
        .add_http(
            &format!("http://{addr}/big.bin"),
            DownloadOptions::default(),
        )
        .await
        .unwrap();
    let state = wait_for(&engine, id, Duration::from_secs(10), |s| {
        matches!(s, DownloadState::Completed | DownloadState::Error { .. })
    })
    .await;
    let DownloadState::Error {
        kind, retryable, ..
    } = &state
    else {
        panic!("a name conflict must fail the download, got {state:?}");
    };
    assert!(
        kind.contains(&format!("{:?}", StorageErrorKind::AlreadyExists)),
        "the error should identify the conflict: {kind}"
    );
    assert!(!retryable);
    assert_eq!(
        tokio::fs::read(&existing).await.unwrap(),
        b"created while downloading",
        "the existing file must be untouched"
    );
    assert_eq!(
        tokio::fs::read(dir.path().join("big.bin.part"))
            .await
            .unwrap(),
        body,
        "the finished data is kept"
    );

    // The application moves the conflicting file away and resumes.
    let segment_requests = |ranges: &[Option<String>]| {
        ranges
            .iter()
            .filter(|r| r.as_deref() != Some("bytes=0-0"))
            .count()
    };
    let before = segment_requests(&ranges.lock().unwrap());
    tokio::fs::rename(&existing, dir.path().join("big.old.bin"))
        .await
        .unwrap();
    engine.resume(id).await.unwrap();
    let state = wait_for(&engine, id, Duration::from_secs(10), |s| {
        matches!(s, DownloadState::Completed | DownloadState::Error { .. })
    })
    .await;
    assert_eq!(state, DownloadState::Completed);
    assert_eq!(tokio::fs::read(&existing).await.unwrap(), body);
    assert_eq!(
        segment_requests(&ranges.lock().unwrap()),
        before,
        "resume only has to rename; no data is downloaded again"
    );
    engine.shutdown().await.unwrap();
}
