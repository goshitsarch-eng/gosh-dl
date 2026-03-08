//! HTTP Download Engine
//!
//! This module handles HTTP/HTTPS downloads with support for:
//! - Single and multi-connection (segmented) downloads
//! - Resume capability via Range headers
//! - Progress tracking
//! - Custom headers, user-agent, referer
//! - Connection pooling with rate limiting
//! - Retry logic with exponential backoff
//! - Checksum verification (MD5/SHA256)

pub(crate) mod checksum;
pub(crate) mod connection;
pub(crate) mod mirror;
pub(crate) mod resume;
pub(crate) mod segment;

pub use checksum::{compute_checksum, verify_checksum, ChecksumAlgorithm, ExpectedChecksum};
pub use connection::{ConnectionPool, RetryPolicy, SpeedCalculator};
pub use mirror::MirrorManager;
pub use resume::{check_resume, ResumeInfo};
pub use segment::{calculate_segment_count, probe_server, SegmentedDownload, ServerCapabilities};

use crate::config::EngineConfig;
use crate::error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result, StorageErrorKind};
use crate::storage::Segment;
use crate::types::DownloadProgress;

use futures::StreamExt;
use parking_lot::RwLock;
use reqwest::{Client, Response};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

pub(crate) const ACCEPT_ENCODING_IDENTITY: &str = "identity";

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

fn partial_path_for(save_path: &Path) -> PathBuf {
    save_path.with_extension(
        save_path
            .extension()
            .map(|e| format!("{}.part", e.to_string_lossy()))
            .unwrap_or_else(|| "part".to_string()),
    )
}

/// HTTP Downloader
pub struct HttpDownloader {
    pool: Arc<ConnectionPool>,
    config: HttpDownloaderConfig,
    retry_policy: RetryPolicy,
}

/// Configuration for HTTP downloader
#[derive(Debug, Clone)]
pub struct HttpDownloaderConfig {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub max_redirects: usize,
    pub default_user_agent: String,
}

impl HttpDownloader {
    /// Create a new HTTP downloader
    pub fn new(config: &EngineConfig) -> Result<Self> {
        // Create connection pool with rate limiting if configured
        let pool = ConnectionPool::with_limits(
            &config.http,
            config.global_download_limit,
            config.global_upload_limit,
        )?;

        // Create retry policy from config
        let retry_policy = RetryPolicy::new(
            config.http.max_retries as u32,
            config.http.retry_delay_ms,
            config.http.max_retry_delay_ms,
        );

        Ok(Self {
            pool: Arc::new(pool),
            config: HttpDownloaderConfig {
                connect_timeout: Duration::from_secs(config.http.connect_timeout),
                read_timeout: Duration::from_secs(config.http.read_timeout),
                max_redirects: config.http.max_redirects,
                default_user_agent: config.user_agent.clone(),
            },
            retry_policy,
        })
    }

    /// Get the underlying client
    fn client(&self) -> &Client {
        self.pool.client()
    }

    /// Get the retry policy
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry_policy
    }

    /// Download a file from a URL
    ///
    /// Returns the final path of the downloaded file
    #[allow(clippy::too_many_arguments)]
    pub async fn download<F>(
        &self,
        url: &str,
        save_dir: &Path,
        filename: Option<&str>,
        user_agent: Option<&str>,
        referer: Option<&str>,
        headers: &[(String, String)],
        cookies: Option<&[String]>,
        checksum: Option<&ExpectedChecksum>,
        cancel_token: CancellationToken,
        progress_callback: F,
    ) -> Result<PathBuf>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        let progress_callback = Arc::new(progress_callback);
        // Build the request
        let mut request = self.client().get(url);

        // Set user agent
        let ua = user_agent.unwrap_or(&self.config.default_user_agent);
        request = request.header("User-Agent", ua);

        // Set referer if provided
        if let Some(ref_url) = referer {
            request = request.header("Referer", ref_url);
        }

        // Add custom headers
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request = request.header("Accept-Encoding", ACCEPT_ENCODING_IDENTITY);

        // Add cookies if provided
        if let Some(cookie_list) = cookies {
            if !cookie_list.is_empty() {
                let cookie_value = cookie_list.join("; ");
                request = request.header("Cookie", cookie_value);
            }
        }

        // Send HEAD request first to get metadata
        let mut head_request = self.client().head(url).header("User-Agent", ua);
        if let Some(cookie_list) = cookies {
            if !cookie_list.is_empty() {
                head_request = head_request.header("Cookie", cookie_list.join("; "));
            }
        }
        head_request = head_request.header("Accept-Encoding", ACCEPT_ENCODING_IDENTITY);
        let head_response = head_request.send().await;

        let (content_length, supports_range, suggested_filename, etag, last_modified) =
            match head_response {
                Ok(resp) => {
                    let length = resp
                        .headers()
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());

                    let supports_range = resp
                        .headers()
                        .get("accept-ranges")
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.contains("bytes"))
                        .unwrap_or(false);

                    // Try to get filename from Content-Disposition
                    let suggested = resp
                        .headers()
                        .get("content-disposition")
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_content_disposition);

                    let etag = resp
                        .headers()
                        .get("etag")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());

                    let last_modified = resp
                        .headers()
                        .get("last-modified")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());

                    (length, supports_range, suggested, etag, last_modified)
                }
                Err(_) => {
                    // HEAD failed, we'll get metadata from GET response
                    (None, false, None, None, None)
                }
            };

        // Check for cancellation
        if cancel_token.is_cancelled() {
            return Err(EngineError::Shutdown);
        }

        // Determine filename
        let final_filename = filename
            .map(|s| s.to_string())
            .or(suggested_filename)
            .or_else(|| extract_filename_from_url(url))
            .unwrap_or_else(|| "download".to_string());

        // Ensure save directory exists
        if !save_dir.exists() {
            tokio::fs::create_dir_all(save_dir).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    save_dir,
                    format!("Failed to create directory: {}", e),
                )
            })?;
        }

        // Validate filename for path traversal attacks (security)
        // Check each path component to prevent directory traversal
        use std::path::Component;
        for component in Path::new(&final_filename).components() {
            match component {
                Component::ParentDir => {
                    return Err(EngineError::storage(
                        StorageErrorKind::PathTraversal,
                        Path::new(&final_filename),
                        "Invalid filename: contains parent directory reference (..)",
                    ));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(EngineError::storage(
                        StorageErrorKind::PathTraversal,
                        Path::new(&final_filename),
                        "Invalid filename: contains absolute path",
                    ));
                }
                _ => {}
            }
        }

        let save_path = save_dir.join(&final_filename);

        // Use .part extension during download
        let part_path = partial_path_for(&save_path);

        // Check if we can resume
        let existing_size = if supports_range && part_path.exists() {
            tokio::fs::metadata(&part_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        let mut allow_resume = existing_size > 0;

        loop {
            let resume_from = if allow_resume { existing_size } else { 0 };
            let if_range = if resume_from > 0 {
                etag.as_deref().or(last_modified.as_deref())
            } else {
                None
            };

            let mut attempt_request = request.try_clone().ok_or_else(|| {
                EngineError::Internal(
                    "Failed to clone HTTP request builder for restartless retry".to_string(),
                )
            })?;

            if resume_from > 0 {
                attempt_request =
                    attempt_request.header("Range", format!("bytes={}-", resume_from));
                if let Some(if_range_val) = if_range {
                    attempt_request = attempt_request.header("If-Range", if_range_val);
                }
            }

            // Send the request
            let response = attempt_request.send().await?;

            // Check response status
            let status = response.status();
            if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                return Err(EngineError::network(
                    NetworkErrorKind::HttpStatus(status.as_u16()),
                    format!("HTTP error: {}", status),
                ));
            }

            if resume_from > 0 {
                let range_validation = resume::validate_ranged_response(
                    resume_from,
                    None,
                    status,
                    response
                        .headers()
                        .get("content-range")
                        .and_then(|v| v.to_str().ok()),
                    resume::RangedResponseContext {
                        sent_if_range: if_range.is_some(),
                        expected_etag: etag.as_deref(),
                        expected_last_modified: last_modified.as_deref(),
                        response_etag: response.headers().get("etag").and_then(|v| v.to_str().ok()),
                        response_last_modified: response
                            .headers()
                            .get("last-modified")
                            .and_then(|v| v.to_str().ok()),
                    },
                );

                if let Err(err) = range_validation {
                    if resume::should_restart_without_ranges(&err) {
                        tracing::warn!(
                            "HTTP resume for {} cannot continue safely ({}). Restarting from byte 0 with a single stream.",
                            url,
                            err
                        );
                        allow_resume = false;
                        continue;
                    }
                    return Err(err);
                }
            }

            // Get actual content length from response if not from HEAD
            let response_content_length = response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|len| len + resume_from);

            if let (Some(head_len), Some(get_len)) = (content_length, response_content_length) {
                if head_len != get_len {
                    tracing::warn!(
                        "HEAD content-length mismatch for {}: HEAD={}, GET={}",
                        url,
                        head_len,
                        get_len
                    );
                }
            }

            // Prefer the GET response length over HEAD when available.
            let total_size = response_content_length.or(content_length);

            // Open file for writing
            let file = if resume_from > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT {
                // Append mode for resume
                OpenOptions::new()
                    .write(true)
                    .append(true)
                    .open(&part_path)
                    .await
                    .map_err(|e| {
                        EngineError::storage(
                            StorageErrorKind::Io,
                            &part_path,
                            format!("Failed to open file for append: {}", e),
                        )
                    })?
            } else {
                // Create a fresh partial file, truncating any stale resume data.
                File::create(&part_path).await.map_err(|e| {
                    EngineError::storage(
                        StorageErrorKind::Io,
                        &part_path,
                        format!("Failed to create file: {}", e),
                    )
                })?
            };

            // Download with progress tracking
            let downloaded = Arc::new(AtomicU64::new(resume_from));

            // Stream the response body
            let result = self
                .stream_to_file(
                    response,
                    file,
                    downloaded.clone(),
                    total_size,
                    cancel_token.clone(),
                    {
                        let progress_callback = Arc::clone(&progress_callback);
                        move |completed, speed| {
                            let progress = DownloadProgress {
                                total_size,
                                completed_size: completed,
                                download_speed: speed,
                                upload_speed: 0,
                                connections: 1,
                                seeders: 0,
                                peers: 0,
                                eta_seconds: total_size.and_then(|total| {
                                    if speed > 0 {
                                        Some((total.saturating_sub(completed)) / speed)
                                    } else {
                                        None
                                    }
                                }),
                            };
                            log_progress_invariant("http download", &progress);
                            progress_callback(progress);
                        }
                    },
                )
                .await;

            match result {
                Ok(_) => {
                    // Verify checksum before renaming (if checksum was provided)
                    if let Some(expected) = checksum {
                        let verified = verify_checksum(&part_path, expected).await?;
                        if !verified {
                            let actual = compute_checksum(&part_path, expected.algorithm).await?;
                            return Err(checksum::checksum_mismatch_error(
                                &expected.value,
                                &actual,
                            ));
                        }
                        tracing::debug!(
                            "Checksum verified: {} matches expected",
                            expected.algorithm
                        );
                    }

                    // Rename .part file to final name
                    tokio::fs::rename(&part_path, &save_path)
                        .await
                        .map_err(|e| {
                            EngineError::storage(
                                StorageErrorKind::Io,
                                &save_path,
                                format!("Failed to rename file: {}", e),
                            )
                        })?;

                    return Ok(save_path);
                }
                Err(e) => {
                    // Keep .part file for potential resume
                    return Err(e);
                }
            }
        }
    }

    /// Stream response body to file with progress tracking
    async fn stream_to_file<F>(
        &self,
        response: Response,
        mut file: File,
        downloaded: Arc<AtomicU64>,
        total_size: Option<u64>,
        cancel_token: CancellationToken,
        progress_callback: F,
    ) -> Result<()>
    where
        F: Fn(u64, u64) + Send,
    {
        let mut stream = response.bytes_stream();
        let mut last_update = Instant::now();
        let mut bytes_since_update: u64 = 0;
        let update_interval = Duration::from_millis(250); // Update progress 4 times per second

        while let Some(chunk_result) = tokio::select! {
            chunk = stream.next() => chunk,
            _ = cancel_token.cancelled() => {
                file.flush().await.ok();
                return Err(EngineError::Shutdown);
            }
        } {
            let chunk: bytes::Bytes = chunk_result.map_err(|e: reqwest::Error| {
                EngineError::network(NetworkErrorKind::Other, format!("Stream error: {}", e))
            })?;

            let chunk_len = chunk.len() as u64;

            // Apply rate limiting if configured
            self.pool.acquire_download(chunk_len).await;

            // Write chunk to file
            file.write_all(&chunk).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    PathBuf::new(),
                    format!("Failed to write: {}", e),
                )
            })?;

            // Record downloaded bytes for stats
            self.pool.record_download(chunk_len);

            // Update counters
            let new_total = downloaded.fetch_add(chunk_len, Ordering::Relaxed) + chunk_len;
            if let Some(expected) = total_size {
                if new_total > expected {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::InvalidResponse,
                        format!(
                            "Response exceeded expected size: received {} bytes, expected {} bytes",
                            new_total, expected
                        ),
                    ));
                }
            }
            bytes_since_update += chunk_len;

            // Emit progress at intervals
            let now = Instant::now();
            if now.duration_since(last_update) >= update_interval {
                let elapsed_secs = now.duration_since(last_update).as_secs_f64();
                let speed = if elapsed_secs > 0.0 {
                    (bytes_since_update as f64 / elapsed_secs) as u64
                } else {
                    0
                };

                progress_callback(new_total, speed);

                last_update = now;
                bytes_since_update = 0;
            }
        }

        // Flush and sync
        file.flush().await.map_err(|e| {
            EngineError::storage(
                StorageErrorKind::Io,
                PathBuf::new(),
                format!("Failed to flush: {}", e),
            )
        })?;

        file.sync_all().await.map_err(|e| {
            EngineError::storage(
                StorageErrorKind::Io,
                PathBuf::new(),
                format!("Failed to sync: {}", e),
            )
        })?;

        // Final progress update
        let final_size = downloaded.load(Ordering::Relaxed);
        progress_callback(final_size, 0);

        // Validate received size matches expected (if known)
        if let Some(expected) = total_size {
            if final_size != expected {
                return Err(EngineError::protocol(
                    ProtocolErrorKind::InvalidResponse,
                    format!(
                        "Download size mismatch: received {} bytes, expected {} bytes",
                        final_size, expected
                    ),
                ));
            }
        }

        Ok(())
    }

    /// Download a file using multiple connections (segmented download)
    ///
    /// This method probes the server first and uses segmented downloads
    /// if the server supports Range requests and the file is large enough.
    #[allow(clippy::too_many_arguments)]
    /// Download with segmented multi-connection support.
    ///
    /// Returns the final path and optionally an Arc reference to the SegmentedDownload
    /// (only when using segmented download mode).
    pub async fn download_segmented<F>(
        &self,
        url: &str,
        save_dir: &Path,
        filename: Option<&str>,
        user_agent: Option<&str>,
        referer: Option<&str>,
        headers: &[(String, String)],
        cookies: Option<&[String]>,
        checksum: Option<&ExpectedChecksum>,
        max_connections: usize,
        min_segment_size: u64,
        cancel_token: CancellationToken,
        saved_segments: Option<Vec<Segment>>,
        progress_callback: F,
        segmented_ref: Option<Arc<RwLock<Option<Arc<SegmentedDownload>>>>>,
    ) -> Result<(PathBuf, Option<Arc<SegmentedDownload>>)>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        let progress_callback = Arc::new(progress_callback);
        let ua = user_agent.unwrap_or(&self.config.default_user_agent);

        // Probe server capabilities
        let capabilities = probe_server(self.client(), url, ua).await?;

        // Determine filename
        let final_filename = filename
            .map(|s| s.to_string())
            .or(capabilities.suggested_filename.clone())
            .or_else(|| extract_filename_from_url(url))
            .unwrap_or_else(|| "download".to_string());

        // Ensure save directory exists
        if !save_dir.exists() {
            tokio::fs::create_dir_all(save_dir).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    save_dir,
                    format!("Failed to create directory: {}", e),
                )
            })?;
        }

        let save_path = save_dir.join(&final_filename);

        // Decide whether to use segmented download
        let use_segmented = capabilities.supports_range
            && capabilities
                .content_length
                .map(|l| l > min_segment_size)
                .unwrap_or(false);

        if use_segmented {
            let total_size = capabilities.content_length.unwrap();

            // Create segmented download
            let mut download = SegmentedDownload::new(
                url.to_string(),
                total_size,
                save_path.clone(),
                true,
                capabilities.etag,
                capabilities.last_modified,
            );

            // Restore or initialize segments
            if let Some(segments) = saved_segments {
                tracing::debug!("Restoring {} saved segments", segments.len());
                download.restore_segments(segments);
            } else {
                download.init_segments(max_connections, min_segment_size);
            }

            // Wrap in Arc for sharing
            let download = Arc::new(download);
            let download_ref = Arc::clone(&download);

            // Populate shared reference for external access during download (for persistence)
            if let Some(ref slot) = segmented_ref {
                *slot.write() = Some(Arc::clone(&download));
            }

            // Build headers vec
            let mut all_headers = headers.to_vec();
            if let Some(r) = referer {
                all_headers.push(("Referer".to_string(), r.to_string()));
            }
            // Add cookies to headers
            if let Some(cookie_list) = cookies {
                if !cookie_list.is_empty() {
                    all_headers.push(("Cookie".to_string(), cookie_list.join("; ")));
                }
            }

            // Start download
            let segmented_result = download
                .start(
                    self.client(),
                    ua,
                    &all_headers,
                    max_connections,
                    &self.retry_policy,
                    cancel_token.clone(),
                    {
                        let progress_callback = Arc::clone(&progress_callback);
                        move |progress| progress_callback(progress)
                    },
                )
                .await;

            if let Err(err) = segmented_result {
                if resume::should_restart_without_ranges(&err) && !cancel_token.is_cancelled() {
                    tracing::warn!(
                        "Segmented download for {} cannot continue safely ({}). Restarting from byte 0 with a single stream.",
                        url,
                        err
                    );
                    if let Some(ref slot) = segmented_ref {
                        *slot.write() = None;
                    }
                    resume::cleanup_partial(&partial_path_for(&save_path)).await?;
                    let path = self
                        .download(
                            url,
                            save_dir,
                            Some(&final_filename),
                            user_agent,
                            referer,
                            headers,
                            cookies,
                            checksum,
                            cancel_token,
                            {
                                let progress_callback = Arc::clone(&progress_callback);
                                move |progress| progress_callback(progress)
                            },
                        )
                        .await?;
                    return Ok((path, None));
                }
                return Err(err);
            }

            // Verify checksum if provided
            if let Some(expected) = checksum {
                let verified = verify_checksum(&save_path, expected).await?;
                if !verified {
                    let actual = compute_checksum(&save_path, expected.algorithm).await?;
                    return Err(checksum::checksum_mismatch_error(&expected.value, &actual));
                }
                tracing::debug!("Checksum verified: {} matches expected", expected.algorithm);
            }

            Ok((save_path, Some(download_ref)))
        } else {
            // Fall back to single-connection download
            let path = self
                .download(
                    url,
                    save_dir,
                    Some(&final_filename),
                    user_agent,
                    referer,
                    headers,
                    cookies,
                    checksum,
                    cancel_token,
                    {
                        let progress_callback = Arc::clone(&progress_callback);
                        move |progress| progress_callback(progress)
                    },
                )
                .await?;
            Ok((path, None))
        }
    }
}

/// Parse filename from Content-Disposition header
pub fn parse_content_disposition(header: &str) -> Option<String> {
    // Look for filename="..." or filename*=UTF-8''...
    if let Some(start) = header.find("filename=") {
        let rest = &header[start + 9..];
        if let Some(stripped) = rest.strip_prefix('"') {
            // Quoted filename
            let end = stripped.find('"')?;
            return Some(stripped[..end].to_string());
        } else {
            // Unquoted filename
            let end = rest.find(';').unwrap_or(rest.len());
            return Some(rest[..end].trim().to_string());
        }
    }

    if let Some(start) = header.find("filename*=") {
        let rest = &header[start + 10..];
        // UTF-8'' prefix
        if let Some(quote_start) = rest.find("''") {
            let encoded = &rest[quote_start + 2..];
            let end = encoded.find(';').unwrap_or(encoded.len());
            // URL decode
            if let Ok(decoded) = urlencoding::decode(&encoded[..end]) {
                return Some(decoded.to_string());
            }
        }
    }

    None
}

/// Extract filename from URL path
fn extract_filename_from_url(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .path_segments()?
        .next_back()
        .filter(|s| !s.is_empty())
        .map(|s| {
            // URL decode the filename
            urlencoding::decode(s)
                .map(|d| d.to_string())
                .unwrap_or_else(|_| s.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content_disposition() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"test.zip\""),
            Some("test.zip".to_string())
        );

        assert_eq!(
            parse_content_disposition("attachment; filename=test.zip"),
            Some("test.zip".to_string())
        );
    }

    #[test]
    fn test_extract_filename_from_url() {
        assert_eq!(
            extract_filename_from_url("https://example.com/path/to/file.zip"),
            Some("file.zip".to_string())
        );

        assert_eq!(
            extract_filename_from_url("https://example.com/path/to/file%20name.zip"),
            Some("file name.zip".to_string())
        );
    }
}
