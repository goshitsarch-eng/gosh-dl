#![cfg(feature = "http")]
//! Regression tests for defects found during the 0.6.4 application audit.
//!
//! Each test documents the original failure in its doc comment so the
//! behaviour it guards is clear without reading the fix.

use gosh_dl::{
    DownloadEngine, DownloadId, DownloadOptions, DownloadState, EngineConfig, EngineError,
    HttpConfig,
};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fast_config(dir: &TempDir) -> EngineConfig {
    EngineConfig {
        download_dir: dir.path().into(),
        http: HttpConfig {
            max_retries: 0,
            retry_delay_ms: 10,
            max_retry_delay_ms: 10,
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

/// Original failure: `DownloadOptions { max_connections: Some(0) }` was
/// accepted, and the segmented worker then created a zero-permit semaphore
/// that no segment task could ever acquire, so the download hung forever
/// in `Downloading` without making a single request.
#[tokio::test]
async fn max_connections_zero_is_rejected_before_enqueueing() {
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();
    let err = engine
        .add_http(
            "http://127.0.0.1:1/file.bin",
            DownloadOptions {
                max_connections: Some(0),
                ..Default::default()
            },
        )
        .await
        .expect_err("zero connections can never make progress");
    assert!(
        matches!(
            err,
            EngineError::InvalidInput {
                field: "max_connections",
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
    assert!(engine.list().is_empty());
    engine.shutdown().await.unwrap();
}

/// Original failure: the engine took the last URL path segment verbatim, so
/// `hello%20world.txt` was saved as a file literally named
/// `hello%20world.txt`, while the HTTP layer's own extractor (never reached
/// because the engine always supplies a name) percent-decoded it. Decoding
/// also means an encoded traversal (`..%2F..%2Fevil`) must be rejected.
#[tokio::test]
async fn url_filenames_are_percent_decoded_and_validated() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("decoded"))
        .mount(&server)
        .await;
    let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();

    let id = engine
        .add_http(
            &format!("{}/dir/hello%20world.txt", server.uri()),
            DownloadOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        engine.status(id).unwrap().metadata.filename.as_deref(),
        Some("hello world.txt")
    );
    wait_for(&engine, id, Duration::from_secs(5), |s| {
        s == &DownloadState::Completed
    })
    .await;
    assert_eq!(
        tokio::fs::read(dir.path().join("hello world.txt"))
            .await
            .unwrap(),
        b"decoded"
    );

    let err = engine
        .add_http(
            &format!("{}/..%2F..%2Fevil", server.uri()),
            DownloadOptions::default(),
        )
        .await
        .expect_err("percent-encoded traversal must be rejected");
    assert!(
        matches!(
            err,
            EngineError::Storage {
                kind: gosh_dl::StorageErrorKind::PathTraversal,
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
    engine.shutdown().await.unwrap();
}

/// Original failure: a download that ended in `Error { retryable: true }`
/// could not be resumed. `resume()` only accepted `Paused`, so the segment
/// progress the engine carefully saved on failure was unreachable and the
/// only way forward was `repair()`, which discards all partial data.
#[tokio::test]
async fn failed_download_can_be_resumed() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();
    let id = engine
        .add_http(
            &format!("{}/flaky.bin", server.uri()),
            DownloadOptions::default(),
        )
        .await
        .unwrap();
    let state = wait_for(&engine, id, Duration::from_secs(10), |s| s.is_finished()).await;
    assert!(
        matches!(
            state,
            DownloadState::Error {
                retryable: true,
                ..
            }
        ),
        "expected a retryable failure, got {state:?}"
    );

    // The server recovers.
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("recovered"))
        .mount(&server)
        .await;

    engine
        .resume(id)
        .await
        .expect("a failed download must be resumable");
    wait_for(&engine, id, Duration::from_secs(10), |s| {
        s == &DownloadState::Completed
    })
    .await;
    assert_eq!(
        tokio::fs::read(dir.path().join("flaky.bin")).await.unwrap(),
        b"recovered"
    );

    // Completed downloads are still not resumable.
    assert!(matches!(
        engine.resume(id).await,
        Err(EngineError::InvalidState { .. })
    ));
    engine.shutdown().await.unwrap();
}

/// Original failure: after `shutdown()` the engine still accepted new
/// downloads. They ran without the persistence task, and torrents ran
/// without their progress task, so their status silently froze.
#[tokio::test]
async fn adding_downloads_after_shutdown_is_rejected() {
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();
    engine.shutdown().await.unwrap();
    let err = engine
        .add_http("http://127.0.0.1:1/file.bin", DownloadOptions::default())
        .await
        .expect_err("engine is shut down");
    assert!(matches!(err, EngineError::Shutdown), "{err:?}");
    assert!(engine.list().is_empty());
}

/// Minimal HTTP/1.1 server that honours `Range` requests but delivers an
/// un-ranged body in two halves with a pause in between, so the engine's
/// single-stream worker is reliably caught between chunks.
async fn two_part_range_server(body: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut tmp = [0u8; 1024];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    raw.extend_from_slice(&tmp[..n]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&raw).to_string();
                let is_head = request.starts_with("HEAD ");
                let range_start = request
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("Range: bytes=")
                            .or_else(|| line.strip_prefix("range: bytes="))
                    })
                    .and_then(|range| range.split('-').next())
                    .and_then(|start| start.parse::<usize>().ok());

                if is_head {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    return;
                }

                match range_start {
                    Some(start) if start > 0 && start < body.len() => {
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            body.len() - start,
                            start,
                            body.len() - 1,
                            body.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(&body[start..]).await;
                    }
                    _ => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let half = body.len() / 2;
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(&body[..half]).await;
                        let _ = sock.flush().await;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        let _ = sock.write_all(&body[half..]).await;
                    }
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

/// Original failure: `pause()` cancelled the worker's token but returned
/// without waiting for the worker, and `resume()` immediately started a new
/// worker on the same `.part` file. A worker stalled in a non-cancellable
/// wait (here the global rate limiter) woke up later and appended its chunk
/// *after* the new worker had measured the file and issued its `Range`
/// request, so the same bytes landed on disk twice and the "completed" file
/// was corrupt (or the final rename failed).
#[tokio::test]
async fn pause_waits_for_the_worker_so_resume_cannot_corrupt_the_partial_file() {
    let body: Vec<u8> = (0..16 * 1024).map(|i| (i % 251) as u8).collect();
    let base = two_part_range_server(body.clone()).await;
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(EngineConfig {
        // 4 KiB/s: the first 8 KiB half fits the burst allowance, the second
        // half books ~1.5 s of debt that the old worker must sleep through.
        global_download_limit: Some(4 * 1024),
        ..fast_config(&dir)
    })
    .await
    .unwrap();

    let id = engine
        .add_http(&format!("{base}/two-part.bin"), DownloadOptions::default())
        .await
        .unwrap();
    // The first half has arrived and the second is held by the limiter.
    tokio::time::sleep(Duration::from_millis(900)).await;
    engine.pause(id).await.unwrap();
    engine.resume(id).await.unwrap();

    let state = wait_for(&engine, id, Duration::from_secs(20), |s| s.is_finished()).await;
    assert_eq!(state, DownloadState::Completed, "resume must complete");
    let on_disk = tokio::fs::read(dir.path().join("two-part.bin"))
        .await
        .unwrap();
    assert_eq!(
        on_disk.len(),
        body.len(),
        "a paused worker must not append after resume reopened the file"
    );
    assert_eq!(on_disk, body);
    engine.shutdown().await.unwrap();
}

/// Original failure: `cancel(id, true)` cancelled the worker's token and
/// deleted the files without waiting for the worker, so a worker that was
/// still writing could leave a `.part` behind (and on Windows the delete of
/// an open file fails outright).
#[tokio::test]
async fn cancel_with_delete_waits_for_the_worker() {
    let body: Vec<u8> = (0..16 * 1024).map(|i| (i % 253) as u8).collect();
    let base = two_part_range_server(body.clone()).await;
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(EngineConfig {
        global_download_limit: Some(4 * 1024),
        ..fast_config(&dir)
    })
    .await
    .unwrap();
    let id = engine
        .add_http(&format!("{base}/two-part.bin"), DownloadOptions::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
    engine.cancel(id, true).await.unwrap();
    // Give a lingering worker every chance to resurrect the file.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        !dir.path().join("two-part.bin.part").exists(),
        "partial file must be gone after cancel with delete"
    );
    assert!(!dir.path().join("two-part.bin").exists());
    engine.shutdown().await.unwrap();
}

/// Original failure: `set_config` changed the global bandwidth limit on the
/// HTTP pool only, so the limit reported by the engine and the one applied
/// to transfers could disagree. The engine now updates its own limiters
/// directly; this checks the user-visible contract end to end.
#[tokio::test]
async fn set_config_applies_new_global_limits_immediately() {
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();
    assert_eq!(engine.get_bandwidth_limits().download, None);
    let mut config = engine.get_config();
    config.global_download_limit = Some(12_345);
    config.global_upload_limit = Some(6_789);
    engine.set_config(config).unwrap();
    let limits = engine.get_bandwidth_limits();
    assert_eq!(limits.download, Some(12_345));
    assert_eq!(limits.upload, Some(6_789));
    engine.shutdown().await.unwrap();
}

/// Original failure: `cancel(id, true)` removed whatever existed at the
/// final output path, including a pre-existing directory (recursively) or
/// file of the same name, even though an incomplete HTTP download only ever
/// owns its `.part` file.
#[tokio::test]
async fn cancel_with_delete_on_an_incomplete_download_only_removes_the_part_file() {
    let dir = TempDir::new().unwrap();
    let bystander_dir = dir.path().join("report.bin");
    tokio::fs::create_dir_all(bystander_dir.join("inner"))
        .await
        .unwrap();
    tokio::fs::write(bystander_dir.join("inner/keep.txt"), b"keep")
        .await
        .unwrap();
    let part = dir.path().join("report.bin.part");
    tokio::fs::write(&part, b"partial").await.unwrap();

    let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();
    let id = engine
        .add_http(
            "http://127.0.0.1:1/report.bin",
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    engine.cancel(id, true).await.unwrap();

    assert!(
        bystander_dir.join("inner/keep.txt").exists(),
        "a directory that merely shares the output name must survive"
    );
    assert!(
        !part.exists(),
        "the partial file is ours and must be removed"
    );
    engine.shutdown().await.unwrap();
}

#[cfg(feature = "recursive-http")]
mod recursive {
    use super::*;
    use gosh_dl::{RecursiveJobState, RecursiveOptions};
    use wiremock::matchers::path;

    async fn html(server: &MockServer, route: &str, body: &str) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(body.to_string()),
            )
            .mount(server)
            .await;
    }

    async fn file(server: &MockServer, route: &str, body: &str) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
            .mount(server)
            .await;
        Mock::given(method("HEAD"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200).insert_header("Content-Length", body.len().to_string()),
            )
            .mount(server)
            .await;
    }

    /// Original failure: a job with one child removed and one still paused
    /// reported the terminal-looking `Partial`, hiding that work remained.
    #[tokio::test]
    async fn job_state_reports_pending_work_before_terminal_mixes() {
        let dir = TempDir::new().unwrap();
        let server = MockServer::start().await;
        html(
            &server,
            "/files/",
            "<a href=\"one.bin\">one</a><a href=\"two.bin\">two</a>",
        )
        .await;
        let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();
        let job = engine
            .add_http_recursive(
                &format!("{}/files/", server.uri()),
                DownloadOptions {
                    start_paused: true,
                    ..Default::default()
                },
                RecursiveOptions::default(),
            )
            .await
            .unwrap();
        let tracked = engine.list_recursive_jobs().remove(0);
        assert_eq!(
            engine.recursive_job_status(&tracked.as_job()).state,
            RecursiveJobState::Paused
        );
        engine.cancel(job.child_ids[0], false).await.unwrap();
        let status = engine.recursive_job_status(&tracked.as_job());
        assert_eq!(status.progress.missing_children, 1);
        assert_eq!(status.progress.paused_children, 1);
        assert_eq!(
            status.state,
            RecursiveJobState::Paused,
            "a paused child is pending work, not a terminal partial result"
        );
        engine.cancel(job.child_ids[1], false).await.unwrap();
        assert_eq!(
            engine.recursive_job_status(&tracked.as_job()).state,
            RecursiveJobState::Failed
        );
        engine.shutdown().await.unwrap();
    }

    /// Original failures: `preserve_paths: false` was silently ignored, and
    /// percent-encoded link text was kept verbatim in local paths.
    #[tokio::test]
    async fn discovery_flattens_when_asked_and_decodes_percent_encoding() {
        let dir = TempDir::new().unwrap();
        let server = MockServer::start().await;
        html(
            &server,
            "/files/",
            "<a href=\"sub%20dir/\">sub</a><a href=\"top%20level.bin\">top</a>",
        )
        .await;
        html(
            &server,
            "/files/sub%20dir/",
            "<a href=\"deep%20one.bin\">deep</a>",
        )
        .await;
        let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();
        let root = format!("{}/files/", server.uri());

        let nested = engine
            .discover_http_recursive(
                &root,
                &DownloadOptions::default(),
                &RecursiveOptions::default(),
            )
            .await
            .unwrap();
        let mut paths: Vec<String> = nested
            .entries
            .iter()
            .map(|e| e.relative_path.to_string_lossy().replace('\\', "/"))
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["sub dir/deep one.bin", "top level.bin"]);

        let flat = engine
            .discover_http_recursive(
                &root,
                &DownloadOptions::default(),
                &RecursiveOptions {
                    preserve_paths: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut paths: Vec<String> = flat
            .entries
            .iter()
            .map(|e| e.relative_path.to_string_lossy().to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["deep one.bin", "top level.bin"]);
        engine.shutdown().await.unwrap();
    }

    /// Original failure: directory pages at `max_depth` were fetched (and
    /// consumed page budget) even though their links could never be used.
    #[tokio::test]
    async fn discovery_does_not_fetch_pages_it_can_never_parse() {
        let dir = TempDir::new().unwrap();
        let server = MockServer::start().await;
        html(
            &server,
            "/files/",
            "<a href=\"sub/\">sub</a><a href=\"a.bin\">a</a>",
        )
        .await;
        html(&server, "/files/sub/", "<a href=\"b.bin\">b</a>").await;
        let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();
        let manifest = engine
            .discover_http_recursive(
                &format!("{}/files/", server.uri()),
                &DownloadOptions::default(),
                &RecursiveOptions {
                    max_depth: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].relative_path.to_string_lossy(), "a.bin");
        let fetched: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(
            fetched,
            vec!["/files/"],
            "the unparsable subdirectory must not be fetched"
        );
        engine.shutdown().await.unwrap();
    }

    /// Guard for the delete-path semantics on recursive children: removing
    /// a job with `delete_files` must not recurse into an unrelated
    /// directory that shares a child's name.
    #[tokio::test]
    async fn removing_a_job_with_delete_does_not_touch_same_named_directories() {
        let dir = TempDir::new().unwrap();
        let server = MockServer::start().await;
        html(&server, "/files/", "<a href=\"Documents\">d</a>").await;
        file(&server, "/files/Documents", "listing").await;
        let bystander = dir.path().join("Documents");
        tokio::fs::create_dir_all(bystander.join("inner"))
            .await
            .unwrap();
        tokio::fs::write(bystander.join("inner/keep.txt"), b"keep")
            .await
            .unwrap();
        let engine = DownloadEngine::new(fast_config(&dir)).await.unwrap();
        engine
            .add_http_recursive(
                &format!("{}/files/", server.uri()),
                DownloadOptions {
                    start_paused: true,
                    ..Default::default()
                },
                RecursiveOptions::default(),
            )
            .await
            .unwrap();
        let job = engine.list_recursive_jobs().remove(0);
        engine.remove_recursive_job(job.id, true).await.unwrap();
        assert!(bystander.join("inner/keep.txt").exists());
        assert!(engine.list_recursive_jobs().is_empty());
        engine.shutdown().await.unwrap();
    }
}
