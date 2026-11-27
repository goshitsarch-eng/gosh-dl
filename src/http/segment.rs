//! Segmented Download Support
//!
//! This module provides multi-connection segmented downloads for faster
//! HTTP/HTTPS transfers. It splits files into segments and downloads
//! them in parallel using multiple connections.

use super::connection::RetryPolicy;
use super::resume::{
    should_restart_without_ranges, validate_ranged_response, RangedResponseContext,
};
use super::{cancellable, ACCEPT_ENCODING_IDENTITY};
use crate::error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result, StorageErrorKind};
use crate::storage::Segment;
use crate::types::DownloadProgress;

use bytes::Bytes;
use futures::stream::StreamExt;
use parking_lot::RwLock;
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Minimum segment size (1 MiB)
pub const MIN_SEGMENT_SIZE: u64 = 1024 * 1024;

/// Default number of connections per download
pub const DEFAULT_CONNECTIONS: usize = 16;

/// Progress update interval
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Persistence interval for segment state
const PERSISTENCE_INTERVAL: Duration = Duration::from_secs(5);

fn log_progress_invariant(context: &str, progress: &DownloadProgress) {
    if let Some(total_size) = progress.total_size {
        if progress.completed_size > total_size {
            debug_assert!(
                progress.completed_size <= total_size,
                "{} progress exceeded total size: {} > {}",
                context,
                progress.completed_size,
                total_size
            );
            tracing::warn!(
                "{} progress exceeded total size: {} > {}",
                context,
                progress.completed_size,
                total_size
            );
        }
    }
}

/// Shared state for a segmented download
struct SharedState {
    /// Total bytes downloaded across all segments
    downloaded: AtomicU64,
    /// Current download speed (bytes/sec)
    speed: AtomicU64,
    /// Number of active connections
    active_connections: AtomicU64,
    /// Whether download is paused
    paused: AtomicBool,
    /// Per-segment downloaded bytes (for tracking progress)
    segment_progress: RwLock<Vec<u64>>,
    /// Last persistence time
    last_persistence: RwLock<Instant>,
}

/// Segmented download manager
pub struct SegmentedDownload {
    /// URL to download from
    url: String,
    /// Total file size
    total_size: u64,
    /// Path to save the file
    save_path: PathBuf,
    /// Segments
    segments: Vec<Segment>,
    /// Whether server supports range requests (stored for resume validation)
    #[allow(dead_code)]
    supports_range: bool,
    /// ETag for validation
    etag: Option<String>,
    /// Last-Modified for validation
    last_modified: Option<String>,
    /// Shared state (wrapped in Arc for task sharing)
    state: Arc<SharedState>,
    /// Mirror manager for striping segments across multiple URLs
    mirrors: Option<Arc<super::MirrorManager>>,
    /// Engine-wide [`FileConflictPolicy`](crate::types::FileConflictPolicy)
    /// discriminant (`None` = overwrite), read when the download finishes.
    conflict_policy: Option<Arc<std::sync::atomic::AtomicU8>>,
    /// Where the finished file ended up, once renamed.
    finalized_path: RwLock<Option<PathBuf>>,
}

/// Server capabilities determined from HEAD request
#[derive(Debug, Clone)]
pub struct ServerCapabilities {
    /// Content-Length header value
    pub content_length: Option<u64>,
    /// Whether server supports Range requests
    pub supports_range: bool,
    /// ETag header for validation
    pub etag: Option<String>,
    /// Last-Modified header for validation
    pub last_modified: Option<String>,
    /// Suggested filename from Content-Disposition
    pub suggested_filename: Option<String>,
}

impl SegmentedDownload {
    /// Create a new segmented download
    pub fn new(
        url: String,
        total_size: u64,
        save_path: PathBuf,
        supports_range: bool,
        etag: Option<String>,
        last_modified: Option<String>,
    ) -> Self {
        Self {
            url,
            total_size,
            save_path,
            segments: Vec::new(),
            supports_range,
            etag,
            last_modified,
            state: Arc::new(SharedState {
                downloaded: AtomicU64::new(0),
                speed: AtomicU64::new(0),
                active_connections: AtomicU64::new(0),
                paused: AtomicBool::new(false),
                segment_progress: RwLock::new(Vec::new()),
                last_persistence: RwLock::new(Instant::now()),
            }),
            mirrors: None,
            conflict_policy: None,
            finalized_path: RwLock::new(None),
        }
    }

    /// Share the engine's file-conflict policy with this download.
    pub(crate) fn set_file_conflict_policy(&mut self, policy: Arc<std::sync::atomic::AtomicU8>) {
        self.conflict_policy = Some(policy);
    }

    /// The path the finished file was saved to. Equal to
    /// [`save_path`](Self::save_path) unless the conflict policy picked a
    /// different name.
    pub fn final_path(&self) -> PathBuf {
        self.finalized_path
            .read()
            .clone()
            .unwrap_or_else(|| self.save_path.clone())
    }

    /// Enable mirror striping: each segment is assigned one of the available
    /// URLs round-robin, and a failing URL is retried against the others.
    /// Only the primary URL's requests carry If-Range validators (mirrors
    /// have their own ETags); non-primary responses are instead validated by
    /// their Content-Range total against the expected file size.
    pub fn set_mirror_manager(&mut self, mirrors: Arc<super::MirrorManager>) {
        self.mirrors = Some(mirrors);
    }

    /// Initialize segments for a new download
    pub fn init_segments(&mut self, max_connections: usize, min_segment_size: u64) {
        if self.total_size == 0 {
            // Nothing to fetch; a zero-byte range has no valid `end`.
            *self.state.segment_progress.write() = Vec::new();
            self.segments = Vec::new();
            return;
        }
        let num_segments =
            calculate_segment_count(self.total_size, max_connections, min_segment_size);
        let segment_size = self.total_size / num_segments as u64;

        let mut segments = Vec::with_capacity(num_segments);
        for i in 0..num_segments {
            let start = i as u64 * segment_size;
            let end = if i == num_segments - 1 {
                self.total_size - 1
            } else {
                (i as u64 + 1) * segment_size - 1
            };
            segments.push(Segment::new(i, start, end));
        }

        // Initialize segment progress tracking
        *self.state.segment_progress.write() = vec![0u64; num_segments];

        self.segments = segments;
    }

    /// Restore segments from saved state
    pub fn restore_segments(&mut self, saved_segments: Vec<Segment>) {
        // Calculate total already downloaded
        let downloaded: u64 = saved_segments.iter().map(|s| s.downloaded).sum();
        self.state.downloaded.store(downloaded, Ordering::Relaxed);

        // Initialize segment progress tracking with saved values
        let progress: Vec<u64> = saved_segments.iter().map(|s| s.downloaded).collect();
        *self.state.segment_progress.write() = progress;

        self.segments = saved_segments;
    }

    /// Get current segments
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Get segments with current progress updated
    ///
    /// This creates a snapshot of the current segment state for persistence.
    pub fn segments_with_progress(&self) -> Vec<Segment> {
        let progress = self.state.segment_progress.read();
        self.segments
            .iter()
            .enumerate()
            .map(|(idx, s)| {
                let mut segment = s.clone();
                if let Some(&downloaded) = progress.get(idx) {
                    segment.downloaded = downloaded;
                    if segment.downloaded >= segment.size() {
                        segment.state = crate::storage::SegmentState::Completed;
                    } else if segment.downloaded > 0 {
                        segment.state = crate::storage::SegmentState::Downloading;
                    }
                }
                segment
            })
            .collect()
    }

    /// Start the segmented download
    #[allow(clippy::too_many_arguments)]
    pub async fn start<F>(
        &self,
        client: &Client,
        user_agent: &str,
        headers: &[(String, String)],
        max_connections: usize,
        retry_policy: &RetryPolicy,
        cancel_token: CancellationToken,
        progress_callback: F,
    ) -> Result<()>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        self.start_with_scope(
            client,
            user_agent,
            headers,
            max_connections,
            retry_policy,
            #[cfg(feature = "recursive-http")]
            None,
            Vec::new(),
            cancel_token,
            progress_callback,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_with_scope<F>(
        &self,
        client: &Client,
        user_agent: &str,
        headers: &[(String, String)],
        max_connections: usize,
        retry_policy: &RetryPolicy,
        #[cfg(feature = "recursive-http")] redirect_scope: Option<super::crawl::RedirectScope>,
        limiters: Vec<Arc<crate::limiter::RateLimiter>>,
        cancel_token: CancellationToken,
        progress_callback: F,
    ) -> Result<()>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        let limiters: Arc<[Arc<crate::limiter::RateLimiter>]> = limiters.into();
        // Create/open the file and pre-allocate space
        let file = self.prepare_file().await?;
        let file = Arc::new(tokio::sync::Mutex::new(file));

        // Create semaphore for connection limiting
        // At least one permit: zero would park every segment task forever.
        let semaphore = Arc::new(Semaphore::new(max_connections.max(1)));

        // Child cancel token: cancelled on fatal (non-retryable) segment errors
        // so sibling segments stop promptly instead of wasting bandwidth
        let fatal_cancel = cancel_token.child_token();

        // Shared state for progress tracking
        let progress_callback = Arc::new(progress_callback);
        let last_progress = Arc::new(RwLock::new(Instant::now()));
        let bytes_since_progress = Arc::new(AtomicU64::new(0));

        // Clone segments data for tasks
        let segments_data: Vec<_> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.is_complete())
            .map(|(idx, s)| (idx, s.start, s.end, s.downloaded))
            .collect();

        // Spawn tasks for each pending segment
        let mut handles = Vec::new();

        for (segment_idx, start, end, already_downloaded) in segments_data {
            let client = client.clone();
            let url = self.url.clone();
            let mirrors = self.mirrors.clone();
            let user_agent = user_agent.to_string();
            let headers = headers.to_vec();
            let file = Arc::clone(&file);
            let semaphore = Arc::clone(&semaphore);
            let cancel_token = fatal_cancel.clone();
            let etag = self.etag.clone();
            let last_modified = self.last_modified.clone();
            let state = Arc::clone(&self.state);
            let progress_callback = Arc::clone(&progress_callback);
            let last_progress = Arc::clone(&last_progress);
            let bytes_since_progress = Arc::clone(&bytes_since_progress);
            let total_size = self.total_size;
            let retry_policy = retry_policy.clone();
            let limiters = Arc::clone(&limiters);
            #[cfg(feature = "recursive-http")]
            let redirect_scope = redirect_scope.clone();

            let handle = tokio::spawn(async move {
                // Acquire permit (bail out promptly if cancelled while queued)
                let _permit = match cancellable(&cancel_token, semaphore.acquire()).await {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) | Err(_) => return Ok(()),
                };

                // Check cancellation
                if cancel_token.is_cancelled() {
                    return Ok(());
                }

                // Check if paused
                if state.paused.load(Ordering::Relaxed) {
                    return Ok(());
                }

                state.active_connections.fetch_add(1, Ordering::Relaxed);

                // Persistent state across retries
                let mut segment_bytes: u64 = already_downloaded;
                let expected_segment_size = end - start + 1;
                let mut attempt = 0u32;
                // Progress at the previous stream failure. A failure that
                // follows new bytes is a fresh problem, not a repeat of the
                // last one, so it starts a new retry budget; otherwise a
                // long transfer over a flaky link would die after
                // `max_retries` interruptions in total.
                let mut bytes_at_last_failure: u64 = already_downloaded;

                // Check if already complete before entering retry loop
                if start + segment_bytes > end {
                    state.active_connections.fetch_sub(1, Ordering::Relaxed);
                    return Ok(());
                }

                let result: Result<()> = 'retry: loop {
                    // Check cancellation between retries
                    if cancel_token.is_cancelled() {
                        break 'retry Ok(());
                    }

                    // Calculate resume position from current progress
                    let resume_start = start + segment_bytes;
                    if resume_start > end {
                        break 'retry Ok(());
                    }

                    // Choose the URL: stripe across available mirrors when
                    // configured (rotating on retries), else the primary.
                    let (request_url, is_primary) = match mirrors.as_ref() {
                        Some(m) => {
                            let candidate = m
                                .url_for_segment(segment_idx + attempt as usize)
                                .to_string();
                            let is_primary = candidate == url;
                            (candidate, is_primary)
                        }
                        None => (url.clone(), true),
                    };

                    // Build request with Range header
                    let mut request = client.get(&request_url);
                    request = request.header("User-Agent", &user_agent);
                    request = request.header("Range", format!("bytes={}-{}", resume_start, end));

                    // Add ETag for validation if available. Only the primary
                    // URL: mirrors have their own validators, and a mismatched
                    // If-Range would silently return 200 (full body).
                    let send_if_range = is_primary;
                    if send_if_range {
                        if let Some(if_range_val) = etag.as_deref().or(last_modified.as_deref()) {
                            request = request.header("If-Range", if_range_val);
                        }
                    }

                    // Add custom headers
                    for (name, value) in &headers {
                        request = request.header(name.as_str(), value.as_str());
                    }
                    request = request.header("Accept-Encoding", ACCEPT_ENCODING_IDENTITY);

                    // Send request
                    let response = match cancellable(&cancel_token, request.send()).await {
                        Err(_) => break 'retry Ok(()),
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            if let Some(m) = mirrors.as_ref() {
                                m.report_url_failure(&request_url);
                            }
                            let err: EngineError = e.into();
                            attempt += 1;
                            if retry_policy.should_retry(attempt - 1, &err) {
                                tracing::warn!(
                                    "Segment {} request failed (attempt {}/{}), retrying: {}",
                                    segment_idx,
                                    attempt,
                                    retry_policy.max_attempts,
                                    err
                                );
                                let delay = retry_policy.delay_for_attempt(attempt - 1);
                                if cancellable(&cancel_token, tokio::time::sleep(delay))
                                    .await
                                    .is_err()
                                {
                                    break 'retry Ok(());
                                }
                                continue 'retry;
                            }
                            if !err.is_retryable() {
                                cancel_token.cancel();
                            }
                            break 'retry Err(err);
                        }
                    };
                    #[cfg(feature = "recursive-http")]
                    if let Some(scope) = redirect_scope.as_ref() {
                        if let Err(e) = super::crawl::validate_redirect_scope(response.url(), scope)
                        {
                            cancel_token.cancel();
                            break 'retry Err(e);
                        }
                    }

                    let status = response.status();

                    // Handle 416 Range Not Satisfiable — not retryable
                    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                        cancel_token.cancel();
                        break 'retry Err(EngineError::network(
                            NetworkErrorKind::HttpStatus(416),
                            format!(
                                "Segment {} range not satisfiable (file may have changed on server)",
                                segment_idx
                            ),
                        ));
                    }

                    // Handle server errors (5xx) with retry
                    if status.is_server_error() {
                        if let Some(m) = mirrors.as_ref() {
                            m.report_url_failure(&request_url);
                        }
                        let err = EngineError::network(
                            NetworkErrorKind::HttpStatus(status.as_u16()),
                            format!("Segment {} server error: {}", segment_idx, status),
                        );
                        attempt += 1;
                        if retry_policy.should_retry(attempt - 1, &err) {
                            tracing::warn!(
                                "Segment {} server error (attempt {}/{}), retrying: {}",
                                segment_idx,
                                attempt,
                                retry_policy.max_attempts,
                                status
                            );
                            let delay = retry_policy.delay_for_attempt(attempt - 1);
                            if cancellable(&cancel_token, tokio::time::sleep(delay))
                                .await
                                .is_err()
                            {
                                break 'retry Ok(());
                            }
                            continue 'retry;
                        }
                        break 'retry Err(err);
                    }

                    if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                        cancel_token.cancel();
                        break 'retry Err(EngineError::network(
                            NetworkErrorKind::HttpStatus(status.as_u16()),
                            format!("Segment {} HTTP error: {}", segment_idx, status),
                        ));
                    }

                    let content_range = response
                        .headers()
                        .get("content-range")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());

                    if let Err(e) = validate_ranged_response(
                        resume_start,
                        Some(end),
                        status,
                        content_range.as_deref(),
                        RangedResponseContext {
                            sent_if_range: send_if_range
                                && (etag.is_some() || last_modified.is_some()),
                            expected_etag: if is_primary { etag.as_deref() } else { None },
                            expected_last_modified: if is_primary {
                                last_modified.as_deref()
                            } else {
                                None
                            },
                            response_etag: response
                                .headers()
                                .get("etag")
                                .and_then(|v| v.to_str().ok()),
                            response_last_modified: response
                                .headers()
                                .get("last-modified")
                                .and_then(|v| v.to_str().ok()),
                        },
                    ) {
                        if !is_primary {
                            // A misbehaving mirror shouldn't fail the download:
                            // penalize it and retry against another URL.
                            if let Some(m) = mirrors.as_ref() {
                                m.report_url_failure(&request_url);
                            }
                            attempt += 1;
                            if attempt < retry_policy.max_attempts {
                                continue 'retry;
                            }
                        }
                        break 'retry Err(e);
                    }

                    // Cross-mirror sanity: without shared validators, the one
                    // thing every mirror must agree on is the file size.
                    if let Some(total) = content_range
                        .as_deref()
                        .and_then(|cr| cr.rsplit('/').next())
                        .and_then(|t| t.parse::<u64>().ok())
                    {
                        if total != total_size {
                            if let Some(m) = mirrors.as_ref() {
                                m.report_url_failure(&request_url);
                            }
                            let err = EngineError::protocol(
                                ProtocolErrorKind::InvalidResponse,
                                format!(
                                    "Mirror {} reports size {} but expected {}",
                                    request_url, total, total_size
                                ),
                            );
                            if !is_primary {
                                attempt += 1;
                                if attempt < retry_policy.max_attempts {
                                    continue 'retry;
                                }
                            }
                            break 'retry Err(err);
                        }
                    }

                    // Stream data to file
                    let mut stream = response.bytes_stream();
                    let mut stream_failed = false;

                    while let Some(chunk_result) = tokio::select! {
                        chunk = stream.next() => chunk,
                        _ = cancel_token.cancelled() => None,
                    } {
                        // Check pause
                        if state.paused.load(Ordering::Relaxed) {
                            break;
                        }

                        let chunk: Bytes = match chunk_result {
                            Ok(c) => c,
                            Err(e) => {
                                let err: EngineError = e.into();
                                if segment_bytes > bytes_at_last_failure {
                                    attempt = 0;
                                }
                                bytes_at_last_failure = segment_bytes;
                                attempt += 1;
                                if retry_policy.should_retry(attempt - 1, &err) {
                                    tracing::warn!(
                                        "Segment {} stream error (attempt {}/{}), retrying from byte {}: {}",
                                        segment_idx, attempt, retry_policy.max_attempts, segment_bytes, err
                                    );
                                    stream_failed = true;
                                    break;
                                }
                                if !err.is_retryable() {
                                    cancel_token.cancel();
                                }
                                break 'retry Err(err);
                            }
                        };

                        let chunk_len = chunk.len() as u64;

                        // Reject an over-long body *before* writing: the
                        // bytes past this segment's end would land in the
                        // neighbouring segment and corrupt data it already
                        // downloaded.
                        if segment_bytes + chunk_len > expected_segment_size {
                            break 'retry Err(EngineError::protocol(
                                ProtocolErrorKind::InvalidResponse,
                                format!(
                                    "Segment {} exceeded expected size: received {} bytes, expected {} bytes",
                                    segment_idx,
                                    segment_bytes + chunk_len,
                                    expected_segment_size
                                ),
                            ));
                        }

                        // Write to file at correct offset. The flush waits
                        // for the write to reach the OS and reports its
                        // result: tokio defers write errors to the next
                        // operation, so without it a failed write (disk
                        // full) would already be counted as progress and
                        // leave a hole in the file after a resume.
                        let write_result: std::io::Result<()> = async {
                            let mut file = file.lock().await;
                            file.seek(SeekFrom::Start(start + segment_bytes)).await?;
                            file.write_all(&chunk).await?;
                            file.flush().await
                        }
                        .await;
                        if let Err(e) = write_result {
                            break 'retry Err(EngineError::storage(
                                StorageErrorKind::Io,
                                PathBuf::new(),
                                format!("Write failed: {}", e),
                            ));
                        }

                        segment_bytes += chunk_len;

                        // Update segment progress for persistence
                        {
                            let mut progress = state.segment_progress.write();
                            if let Some(p) = progress.get_mut(segment_idx) {
                                *p = segment_bytes;
                            }
                        }

                        // Update global counters
                        let total_downloaded =
                            state.downloaded.fetch_add(chunk_len, Ordering::Relaxed) + chunk_len;
                        if total_downloaded > total_size {
                            break 'retry Err(EngineError::protocol(
                                ProtocolErrorKind::InvalidResponse,
                                format!(
                                    "Download exceeded expected size: received {} bytes, expected {} bytes",
                                    total_downloaded, total_size
                                ),
                            ));
                        }
                        bytes_since_progress.fetch_add(chunk_len, Ordering::Relaxed);

                        // Rate limiting: draw this chunk from every configured
                        // budget (global engine limit, then per-download). The
                        // wait must not outlive a pause/cancel.
                        let limited = async {
                            for limiter in limiters.iter() {
                                limiter.acquire(chunk_len).await;
                            }
                        };
                        if cancellable(&cancel_token, limited).await.is_err() {
                            break 'retry Ok(());
                        }

                        // Emit progress at intervals. Speed is computed here
                        // from the shared byte counter so it reflects ALL
                        // segments; a per-task rate would report ~1/N of the
                        // real throughput (whichever task stored last).
                        let now = Instant::now();
                        let should_emit = {
                            let mut last = last_progress.write();
                            let elapsed = now.duration_since(*last);
                            if elapsed >= PROGRESS_INTERVAL {
                                *last = now;
                                let bytes = bytes_since_progress.swap(0, Ordering::Relaxed);
                                let aggregate =
                                    (bytes as f64 / elapsed.as_secs_f64().max(0.001)) as u64;
                                state.speed.store(aggregate, Ordering::Relaxed);
                                true
                            } else {
                                false
                            }
                        };

                        if should_emit {
                            let current_speed = state.speed.load(Ordering::Relaxed);
                            let connections =
                                state.active_connections.load(Ordering::Relaxed) as u32;

                            let progress = DownloadProgress {
                                total_size: Some(total_size),
                                completed_size: total_downloaded,
                                download_speed: current_speed,
                                upload_speed: 0,
                                connections,
                                seeders: 0,
                                peers: 0,
                                eta_seconds: total_size
                                    .saturating_sub(total_downloaded)
                                    .checked_div(current_speed),
                            };
                            log_progress_invariant("segmented http download", &progress);
                            progress_callback(progress);
                        }
                    }

                    if stream_failed {
                        let delay = retry_policy.delay_for_attempt(attempt - 1);
                        if cancellable(&cancel_token, tokio::time::sleep(delay))
                            .await
                            .is_err()
                        {
                            break 'retry Ok(());
                        }
                        continue 'retry;
                    }

                    // Stream completed successfully (or was paused/cancelled)
                    break 'retry Ok(());
                };

                state.active_connections.fetch_sub(1, Ordering::Relaxed);

                // Return the result from the retry loop
                result
            });

            handles.push(handle);
        }

        // Wait for all segment tasks to complete and collect errors
        let mut segment_errors: Vec<String> = Vec::new();
        let mut any_retryable = false;
        let mut restart_without_ranges_reason: Option<String> = None;
        for (idx, handle) in handles.into_iter().enumerate() {
            match handle.await {
                Err(e) => {
                    // Task panicked
                    tracing::error!("Segment {} task panicked: {:?}", idx, e);
                    segment_errors.push(format!("Segment {} panicked: {:?}", idx, e));
                }
                Ok(Err(e)) => {
                    // Task returned an error
                    tracing::error!("Segment {} failed: {:?}", idx, e);
                    if e.is_retryable() {
                        any_retryable = true;
                    }
                    if restart_without_ranges_reason.is_none() && should_restart_without_ranges(&e)
                    {
                        restart_without_ranges_reason = Some(e.to_string());
                    }
                    segment_errors.push(format!("Segment {} failed: {}", idx, e));
                }
                Ok(Ok(())) => {
                    // Task completed successfully
                }
            }
        }

        // If any segments failed, return error
        if !segment_errors.is_empty() {
            if let Some(reason) = restart_without_ranges_reason {
                return Err(EngineError::protocol(
                    ProtocolErrorKind::RangeNotSupported,
                    format!(
                        "Segmented download requires restart without ranges: {}",
                        reason
                    ),
                ));
            }
            // Preserve retryability: if any segment had a retryable error,
            // the aggregate should also be retryable so the engine can retry
            let kind = if any_retryable {
                NetworkErrorKind::ConnectionReset
            } else {
                NetworkErrorKind::Other
            };
            return Err(EngineError::network(
                kind,
                format!(
                    "Download failed: {} segment(s) failed: {}",
                    segment_errors.len(),
                    segment_errors.join("; ")
                ),
            ));
        }

        // Sync file to disk
        {
            let mut file = file.lock().await;
            file.flush().await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    &self.save_path,
                    format!("Flush failed: {}", e),
                )
            })?;
            file.sync_all().await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    &self.save_path,
                    format!("Sync failed: {}", e),
                )
            })?;
        }

        // Final progress update
        let total_downloaded = self.state.downloaded.load(Ordering::Relaxed);
        if total_downloaded != self.total_size {
            // A pause or cancel makes segment tasks return early with Ok, so
            // an incomplete total here is the *expected* outcome of those —
            // report it as Shutdown, not as a scary protocol error that the
            // engine would feed into mirror failover.
            if cancel_token.is_cancelled() || self.state.paused.load(Ordering::Relaxed) {
                return Err(EngineError::Shutdown);
            }
            return Err(EngineError::protocol(
                ProtocolErrorKind::InvalidResponse,
                format!(
                    "Segmented download size mismatch: received {} bytes, expected {} bytes",
                    total_downloaded, self.total_size
                ),
            ));
        }
        let progress = DownloadProgress {
            total_size: Some(self.total_size),
            completed_size: total_downloaded,
            download_speed: 0,
            upload_speed: 0,
            connections: 0,
            seeders: 0,
            peers: 0,
            eta_seconds: None,
        };
        log_progress_invariant("segmented http download", &progress);
        progress_callback(progress);

        // Check if complete
        if total_downloaded >= self.total_size {
            // Rename from .part to final name
            self.finalize().await?;
        }

        Ok(())
    }

    /// Number of bytes available as one contiguous run from the start of the
    /// file: segments write sequentially within their ranges, so this is the
    /// first incomplete segment's start plus its progress. Used by streaming
    /// readers to know how far they can safely read.
    pub fn contiguous_available(&self) -> u64 {
        let progress = self.state.segment_progress.read();
        let mut ordered: Vec<&Segment> = self.segments.iter().collect();
        ordered.sort_by_key(|s| s.start);
        for segment in ordered {
            let downloaded = progress
                .get(segment.index)
                .copied()
                .unwrap_or(segment.downloaded);
            let size = segment.size();
            if downloaded < size {
                return segment.start + downloaded;
            }
        }
        self.total_size
    }

    /// The final save path for this download.
    pub fn save_path(&self) -> &Path {
        &self.save_path
    }

    /// Check if persistence is due based on the time interval.
    ///
    /// Returns true if enough time has passed since the last persistence,
    /// and resets the timer if so.
    pub fn should_persist(&self) -> bool {
        let mut last = self.state.last_persistence.write();
        let now = Instant::now();
        if now.duration_since(*last) >= PERSISTENCE_INTERVAL {
            *last = now;
            true
        } else {
            false
        }
    }

    /// Force mark persistence as done (call after successful save).
    pub fn mark_persisted(&self) {
        *self.state.last_persistence.write() = Instant::now();
    }

    /// Prepare the output file
    async fn prepare_file(&self) -> Result<File> {
        // Use .part extension during download
        let part_path = self.part_path();

        // Ensure parent directory exists
        if let Some(parent) = part_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    parent,
                    format!("Create dir failed: {}", e),
                )
            })?;
        }

        // Check if file exists (for resume)
        let file = if part_path.exists() {
            let file = OpenOptions::new()
                .write(true)
                .read(true)
                .open(&part_path)
                .await
                .map_err(|e| {
                    EngineError::storage(
                        StorageErrorKind::Io,
                        &part_path,
                        format!("Open failed: {}", e),
                    )
                })?;

            // Force the reopened partial to exactly total_size: stale bytes
            // past the end (e.g. from a previous, larger version of the file)
            // would otherwise survive into the renamed final file.
            file.set_len(self.total_size).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    &part_path,
                    format!("Truncate failed: {}", e),
                )
            })?;

            file
        } else {
            // Create new file and pre-allocate
            let file = File::create(&part_path).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    &part_path,
                    format!("Create failed: {}", e),
                )
            })?;

            // Pre-allocate space
            file.set_len(self.total_size).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    &part_path,
                    format!("Pre-allocate failed: {}", e),
                )
            })?;

            file
        };

        Ok(file)
    }

    /// Get the .part file path
    fn part_path(&self) -> PathBuf {
        let ext = self
            .save_path
            .extension()
            .map(|e| format!("{}.part", e.to_string_lossy()))
            .unwrap_or_else(|| "part".to_string());
        self.save_path.with_extension(ext)
    }

    /// Rename .part file to final name
    async fn finalize(&self) -> Result<()> {
        let part_path = self.part_path();
        if part_path.exists() {
            let policy = self
                .conflict_policy
                .as_ref()
                .map(|p| crate::types::FileConflictPolicy::from_u8(p.load(Ordering::Relaxed)))
                .unwrap_or_default();
            let final_path = super::finalize_partial(&part_path, &self.save_path, policy).await?;
            *self.finalized_path.write() = Some(final_path);
        }
        Ok(())
    }

    /// Pause the download
    pub fn pause(&self) {
        self.state.paused.store(true, Ordering::Relaxed);
    }

    /// Check if download is complete
    pub fn is_complete(&self) -> bool {
        self.state.downloaded.load(Ordering::Relaxed) >= self.total_size
    }

    /// Get current progress
    pub fn progress(&self) -> DownloadProgress {
        let progress = DownloadProgress {
            total_size: Some(self.total_size),
            completed_size: self.state.downloaded.load(Ordering::Relaxed),
            download_speed: self.state.speed.load(Ordering::Relaxed),
            upload_speed: 0,
            connections: self.state.active_connections.load(Ordering::Relaxed) as u32,
            seeders: 0,
            peers: 0,
            eta_seconds: {
                let speed = self.state.speed.load(Ordering::Relaxed);
                let remaining = self
                    .total_size
                    .saturating_sub(self.state.downloaded.load(Ordering::Relaxed));
                remaining.checked_div(speed)
            },
        };
        log_progress_invariant("segmented http download", &progress);
        progress
    }
}

/// Whether `segments` tile `[0, total_size)` exactly: contiguous, in index
/// order, non-empty, and with no segment claiming more bytes than it spans.
///
/// Saved segments come from storage and describe a file of a particular
/// size; anything else must not be resumed.
pub(crate) fn segments_cover(segments: &[Segment], total_size: u64) -> bool {
    if segments.is_empty() || total_size == 0 {
        return false;
    }
    let mut expected_start = 0u64;
    for (position, segment) in segments.iter().enumerate() {
        if segment.index != position
            || segment.start != expected_start
            || segment.end < segment.start
            || segment.downloaded > segment.size()
        {
            return false;
        }
        expected_start = match segment.end.checked_add(1) {
            Some(next) => next,
            None => return false,
        };
    }
    expected_start == total_size
}

/// Calculate optimal number of segments based on file size and constraints
pub fn calculate_segment_count(
    total_size: u64,
    max_connections: usize,
    min_segment_size: u64,
) -> usize {
    if total_size == 0 {
        return 1;
    }

    // Calculate maximum segments based on min_segment_size (a zero minimum
    // would divide by zero; treat it as "one byte")
    let max_segments_by_size = (total_size / min_segment_size.max(1)) as usize;

    // Use the smaller of max_connections and max_segments_by_size
    let num_segments = max_connections.min(max_segments_by_size.max(1));

    // Ensure at least 1 segment
    num_segments.max(1)
}

/// Probe server capabilities.
///
/// Tries `HEAD` first and, if the server refuses it (pre-signed URLs that
/// are valid for `GET` only, servers that answer `405`), falls back to a
/// `GET` with `Range: bytes=0-0`, whose `206` + `Content-Range` reveals the
/// same facts.
pub async fn probe_server(
    client: &Client,
    url: &str,
    user_agent: &str,
) -> Result<ServerCapabilities> {
    probe_with_get_fallback(
        client.head(url).header("User-Agent", user_agent),
        client.get(url).header("User-Agent", user_agent),
    )
    .await
    .map(|(capabilities, _)| capabilities)
}

/// Probe with `HEAD`; when that fails, probe with `GET` + `Range: bytes=0-0`.
///
/// Plenty of servers answer `GET` but not `HEAD`: pre-signed S3-style URLs
/// are signed for one method (a URL minted for `GET` returns `403` to
/// `HEAD`), and some application servers reply `405`. Treating a failed
/// `HEAD` as "no length, no ranges" silently downgraded those downloads to
/// a single connection that could never resume. A one-byte ranged `GET`
/// reveals the same facts: `206` with `Content-Range: bytes 0-0/<total>`
/// proves range support and gives the size.
///
/// Both builders must already carry the caller's headers (user agent,
/// cookies, ...). Returns the capabilities and the final URL after
/// redirects.
pub(crate) async fn probe_with_get_fallback(
    head: reqwest::RequestBuilder,
    get: reqwest::RequestBuilder,
) -> Result<(ServerCapabilities, reqwest::Url)> {
    match probe_head(head).await {
        Ok(found) => Ok(found),
        Err(head_err) => {
            tracing::debug!(
                "HEAD probe failed ({}); probing with a ranged GET instead",
                head_err
            );
            probe_ranged_get(get).await
        }
    }
}

async fn probe_head(
    request: reqwest::RequestBuilder,
) -> Result<(ServerCapabilities, reqwest::Url)> {
    let response = request
        .header("Accept-Encoding", ACCEPT_ENCODING_IDENTITY)
        .send()
        .await
        .map_err(EngineError::from)?;

    if !response.status().is_success() {
        return Err(EngineError::network(
            NetworkErrorKind::HttpStatus(response.status().as_u16()),
            format!("HEAD request returned: {}", response.status()),
        ));
    }

    Ok((
        capabilities_from_headers(response.headers()),
        response.url().clone(),
    ))
}

/// `GET` with `Range: bytes=0-0`. The body (one byte for a `206`, the whole
/// resource for a server that ignores `Range`) is never read: dropping the
/// response abandons the transfer.
async fn probe_ranged_get(
    request: reqwest::RequestBuilder,
) -> Result<(ServerCapabilities, reqwest::Url)> {
    let response = request
        .header("Accept-Encoding", ACCEPT_ENCODING_IDENTITY)
        .header("Range", "bytes=0-0")
        .send()
        .await
        .map_err(EngineError::from)?;

    let status = response.status();
    let url = response.url().clone();
    let mut capabilities = capabilities_from_headers(response.headers());

    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        // Content-Length here is the one requested byte; the real size is
        // the total in Content-Range (`None` if the server sent `*`).
        let range = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(super::resume::parse_content_range);
        match range {
            Some((0, 0, total)) => {
                capabilities.supports_range = true;
                capabilities.content_length = total;
            }
            // A 206 that doesn't describe the byte we asked for can't be
            // trusted to honour other ranges either.
            _ => {
                capabilities.supports_range = false;
                capabilities.content_length = None;
            }
        }
    } else if status.is_success() {
        // The server ignored Range and is sending the whole resource: the
        // headers describe it, but ranges evidently don't work.
        capabilities.supports_range = false;
    } else {
        return Err(EngineError::network(
            NetworkErrorKind::HttpStatus(status.as_u16()),
            format!("GET probe returned: {}", status),
        ));
    }

    Ok((capabilities, url))
}

fn capabilities_from_headers(headers: &reqwest::header::HeaderMap) -> ServerCapabilities {
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let supports_range = headers
        .get("accept-ranges")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("bytes"))
        .unwrap_or(false);

    let etag = headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let last_modified = headers
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let suggested_filename = headers
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .and_then(super::parse_content_disposition);

    ServerCapabilities {
        content_length,
        supports_range,
        etag,
        last_modified,
        suggested_filename,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_segment_count() {
        // 100MB file, 16 connections, 1MB min
        assert_eq!(
            calculate_segment_count(100 * 1024 * 1024, 16, 1024 * 1024),
            16
        );

        // 10MB file, 16 connections, 1MB min -> only 10 segments
        assert_eq!(
            calculate_segment_count(10 * 1024 * 1024, 16, 1024 * 1024),
            10
        );

        // 500KB file, 16 connections, 1MB min -> 1 segment
        assert_eq!(calculate_segment_count(512 * 1024, 16, 1024 * 1024), 1);

        // Empty file
        assert_eq!(calculate_segment_count(0, 16, 1024 * 1024), 1);

        // Very large file
        assert_eq!(
            calculate_segment_count(10 * 1024 * 1024 * 1024, 16, 1024 * 1024),
            16
        );
    }

    #[test]
    fn test_segment_init() {
        let mut download = SegmentedDownload::new(
            "https://example.com/file.zip".to_string(),
            100 * 1024 * 1024, // 100MB
            PathBuf::from("/tmp/file.zip"),
            true,
            None,
            None,
        );

        download.init_segments(16, 1024 * 1024);

        let segments = download.segments();
        assert_eq!(segments.len(), 16);

        // Check segment boundaries
        assert_eq!(segments[0].start, 0);
        assert_eq!(segments[15].end, 100 * 1024 * 1024 - 1);

        // Check segments are contiguous
        for i in 0..15 {
            assert_eq!(segments[i].end + 1, segments[i + 1].start);
        }
    }

    #[test]
    fn test_parse_content_disposition() {
        use super::super::parse_content_disposition;
        assert_eq!(
            parse_content_disposition("attachment; filename=\"test.zip\""),
            Some("test.zip".to_string())
        );

        assert_eq!(
            parse_content_disposition("attachment; filename=test.zip"),
            Some("test.zip".to_string())
        );

        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''test%20file.zip"),
            Some("test file.zip".to_string())
        );
    }

    /// Original failure: saved segments were restored without checking that
    /// they describe a file of the current size. With no ETag/Last-Modified
    /// to notice the change, segments laid out for the old size were
    /// resumed against the new file and the download failed with a size
    /// mismatch on every retry.
    #[test]
    fn segments_cover_accepts_only_an_exact_tiling() {
        let tiling = vec![Segment::new(0, 0, 499), Segment::new(1, 500, 999)];
        assert!(segments_cover(&tiling, 1000));

        // Laid out for a different total size
        assert!(!segments_cover(&tiling, 1200));
        assert!(!segments_cover(&tiling, 800));
        // Nothing saved, or nothing to download
        assert!(!segments_cover(&[], 1000));
        assert!(!segments_cover(&tiling, 0));
        // A gap, an overlap, and a segment out of order
        assert!(!segments_cover(
            &[Segment::new(0, 0, 399), Segment::new(1, 500, 999)],
            1000
        ));
        assert!(!segments_cover(
            &[Segment::new(0, 0, 599), Segment::new(1, 500, 999)],
            1000
        ));
        assert!(!segments_cover(
            &[Segment::new(1, 500, 999), Segment::new(0, 0, 499)],
            1000
        ));
        // Inverted bounds and impossible progress (corrupt records)
        assert!(!segments_cover(&[Segment::new(0, 10, 5)], 6));
        let mut overfull = Segment::new(0, 0, 999);
        overfull.downloaded = 5000;
        assert!(!segments_cover(&[overfull], 1000));
        // `size()` on an inverted record must not panic
        assert_eq!(Segment::new(0, 10, 5).size(), 1);
    }

    #[test]
    fn segment_math_tolerates_degenerate_inputs() {
        // A zero minimum used to divide by zero.
        assert_eq!(calculate_segment_count(1000, 4, 0), 4);

        // A zero-length download has no byte ranges at all.
        let mut empty = SegmentedDownload::new(
            "http://example.invalid/empty".to_string(),
            0,
            PathBuf::from("empty.bin"),
            true,
            None,
            None,
        );
        empty.init_segments(4, 1024);
        assert!(empty.segments().is_empty());
    }
}
