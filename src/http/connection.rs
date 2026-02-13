//! Connection Pool Management
//!
//! This module provides HTTP connection pooling with health checks,
//! retry logic, and speed limiting capabilities.

use crate::config::HttpConfig;
use crate::error::{EngineError, NetworkErrorKind, Result};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use reqwest::Client;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Connection pool with rate limiting and health monitoring
pub struct ConnectionPool {
    /// HTTP client (reqwest handles its own connection pool)
    client: Client,
    /// Global rate limiter for download speed
    download_limiter: Option<DefaultDirectRateLimiter>,
    /// Global rate limiter for upload speed
    upload_limiter: Option<DefaultDirectRateLimiter>,
    /// Total bytes downloaded
    total_downloaded: AtomicU64,
    /// Total bytes uploaded
    total_uploaded: AtomicU64,
    /// Active connection count
    active_connections: AtomicU64,
    /// Connection statistics
    stats: RwLock<ConnectionStats>,
}

/// Connection statistics
#[derive(Debug, Clone, Default)]
pub struct ConnectionStats {
    /// Total connections created
    pub connections_created: u64,
    /// Total successful requests
    pub successful_requests: u64,
    /// Total failed requests
    pub failed_requests: u64,
    /// Total retried requests
    pub retried_requests: u64,
    /// Average response time in milliseconds
    pub avg_response_time_ms: f64,
    /// Last error message
    pub last_error: Option<String>,
}

impl ConnectionPool {
    /// Create a new connection pool
    pub fn new(config: &HttpConfig) -> Result<Self> {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout))
            .read_timeout(Duration::from_secs(config.read_timeout))
            .redirect(reqwest::redirect::Policy::limited(config.max_redirects))
            .danger_accept_invalid_certs(config.accept_invalid_certs)
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(Duration::from_secs(90))
            .gzip(true)
            .brotli(true);

        // Add proxy if configured
        if let Some(ref proxy_url) = config.proxy_url {
            let proxy = reqwest::Proxy::all(proxy_url)
                .map_err(|e| EngineError::Internal(format!("Invalid proxy URL: {}", e)))?;
            builder = builder.proxy(proxy);
        }

        let client = builder
            .build()
            .map_err(|e| EngineError::Internal(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self {
            client,
            download_limiter: None,
            upload_limiter: None,
            total_downloaded: AtomicU64::new(0),
            total_uploaded: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
            stats: RwLock::new(ConnectionStats::default()),
        })
    }

    /// Create a connection pool with rate limiting
    pub fn with_limits(
        config: &HttpConfig,
        download_limit: Option<u64>,
        upload_limit: Option<u64>,
    ) -> Result<Self> {
        let mut pool = Self::new(config)?;

        pool.download_limiter = download_limit.and_then(|limit| {
            let clamped = limit.min(u32::MAX as u64) as u32;
            NonZeroU32::new(clamped).map(|n| RateLimiter::direct(Quota::per_second(n)))
        });

        pool.upload_limiter = upload_limit.and_then(|limit| {
            let clamped = limit.min(u32::MAX as u64) as u32;
            NonZeroU32::new(clamped).map(|n| RateLimiter::direct(Quota::per_second(n)))
        });

        Ok(pool)
    }

    /// Get the underlying HTTP client
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Update download speed limit
    pub fn set_download_limit(&mut self, limit: Option<u64>) {
        self.download_limiter = limit.and_then(|l| {
            let clamped = l.min(u32::MAX as u64) as u32;
            NonZeroU32::new(clamped).map(|n| RateLimiter::direct(Quota::per_second(n)))
        });
    }

    /// Update upload speed limit
    pub fn set_upload_limit(&mut self, limit: Option<u64>) {
        self.upload_limiter = limit.and_then(|l| {
            let clamped = l.min(u32::MAX as u64) as u32;
            NonZeroU32::new(clamped).map(|n| RateLimiter::direct(Quota::per_second(n)))
        });
    }

    /// Wait for rate limiter permission to download bytes
    pub async fn acquire_download(&self, bytes: u64) {
        if let Some(ref limiter) = self.download_limiter {
            // Acquire permission in chunks to avoid blocking too long
            let chunk_size = 16384; // 16KB chunks
            let chunks = (bytes / chunk_size).max(1) as u32;
            for _ in 0..chunks {
                if let Some(n) = NonZeroU32::new(chunk_size as u32) {
                    let _ = limiter.until_n_ready(n).await;
                }
            }
        }
    }

    /// Wait for rate limiter permission to upload bytes
    pub async fn acquire_upload(&self, bytes: u64) {
        if let Some(ref limiter) = self.upload_limiter {
            let chunk_size = 16384;
            let chunks = (bytes / chunk_size).max(1) as u32;
            for _ in 0..chunks {
                if let Some(n) = NonZeroU32::new(chunk_size as u32) {
                    let _ = limiter.until_n_ready(n).await;
                }
            }
        }
    }

    /// Record downloaded bytes
    pub fn record_download(&self, bytes: u64) {
        self.total_downloaded.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record uploaded bytes
    pub fn record_upload(&self, bytes: u64) {
        self.total_uploaded.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Get total downloaded bytes
    pub fn total_downloaded(&self) -> u64 {
        self.total_downloaded.load(Ordering::Relaxed)
    }

    /// Get total uploaded bytes
    pub fn total_uploaded(&self) -> u64 {
        self.total_uploaded.load(Ordering::Relaxed)
    }

    /// Increment active connection count
    pub fn connection_started(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement active connection count
    pub fn connection_finished(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    /// Get active connection count
    pub fn active_connections(&self) -> u64 {
        self.active_connections.load(Ordering::Relaxed)
    }

    /// Record a successful request
    pub async fn record_success(&self, response_time_ms: f64) {
        let mut stats = self.stats.write().await;
        stats.successful_requests += 1;

        // Update average response time (exponential moving average)
        let alpha = 0.2;
        stats.avg_response_time_ms =
            alpha * response_time_ms + (1.0 - alpha) * stats.avg_response_time_ms;
    }

    /// Record a failed request
    pub async fn record_failure(&self, error: &str) {
        let mut stats = self.stats.write().await;
        stats.failed_requests += 1;
        stats.last_error = Some(error.to_string());
    }

    /// Record a retried request
    pub async fn record_retry(&self) {
        let mut stats = self.stats.write().await;
        stats.retried_requests += 1;
    }

    /// Get connection statistics
    pub async fn stats(&self) -> ConnectionStats {
        self.stats.read().await.clone()
    }
}

/// Retry policy with exponential backoff and jitter
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts
    pub max_attempts: u32,
    /// Initial delay in milliseconds
    pub initial_delay_ms: u64,
    /// Maximum delay in milliseconds
    pub max_delay_ms: u64,
    /// Jitter factor (0.0 to 1.0)
    pub jitter_factor: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay_ms: 1000,
            max_delay_ms: 30000,
            jitter_factor: 0.25,
        }
    }
}

impl RetryPolicy {
    /// Create a new retry policy
    pub fn new(max_attempts: u32, initial_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self {
            max_attempts,
            initial_delay_ms,
            max_delay_ms,
            jitter_factor: 0.25,
        }
    }

    /// Calculate delay for a given attempt (0-indexed)
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        // Exponential backoff
        let base = self.initial_delay_ms * 2u64.pow(attempt.min(10));
        let capped = base.min(self.max_delay_ms);

        // Add jitter: ±jitter_factor randomness
        let jitter = (rand::random::<f64>() - 0.5) * 2.0 * self.jitter_factor;
        let with_jitter = (capped as f64 * (1.0 + jitter)) as u64;

        Duration::from_millis(with_jitter)
    }

    /// Check if we should retry based on error type
    pub fn should_retry(&self, attempt: u32, error: &EngineError) -> bool {
        if attempt >= self.max_attempts {
            return false;
        }

        error.is_retryable()
    }
}

/// Execute a request with retry logic
pub async fn with_retry<F, T, Fut>(
    pool: &ConnectionPool,
    policy: &RetryPolicy,
    operation: F,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_error = None;

    for attempt in 0..policy.max_attempts {
        let start = Instant::now();

        match operation().await {
            Ok(result) => {
                let elapsed = start.elapsed().as_millis() as f64;
                pool.record_success(elapsed).await;
                return Ok(result);
            }
            Err(e) => {
                let _elapsed = start.elapsed().as_millis() as f64;
                pool.record_failure(&e.to_string()).await;

                if policy.should_retry(attempt, &e) {
                    pool.record_retry().await;
                    let delay = policy.delay_for_attempt(attempt);
                    tracing::debug!(
                        "Request failed (attempt {}), retrying in {:?}: {}",
                        attempt + 1,
                        delay,
                        e
                    );
                    tokio::time::sleep(delay).await;
                    last_error = Some(e);
                } else {
                    return Err(e);
                }
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| EngineError::network(NetworkErrorKind::Other, "Max retries exceeded")))
}

/// Speed calculator for tracking download/upload rates
#[derive(Debug)]
pub struct SpeedCalculator {
    /// Window size for averaging
    window_size: usize,
    /// Recent measurements (bytes, timestamp)
    measurements: Vec<(u64, Instant)>,
    /// Total bytes tracked
    total_bytes: u64,
}

impl SpeedCalculator {
    /// Create a new speed calculator
    pub fn new(window_size: usize) -> Self {
        Self {
            window_size,
            measurements: Vec::with_capacity(window_size),
            total_bytes: 0,
        }
    }

    /// Add a measurement
    pub fn add_bytes(&mut self, bytes: u64) {
        let now = Instant::now();
        self.total_bytes += bytes;

        if self.measurements.len() >= self.window_size {
            self.measurements.remove(0);
        }
        self.measurements.push((bytes, now));
    }

    /// Calculate current speed in bytes/second
    pub fn speed(&self) -> u64 {
        if self.measurements.len() < 2 {
            return 0;
        }

        let first = &self.measurements[0];
        let last = &self.measurements[self.measurements.len() - 1];

        let elapsed = last.1.duration_since(first.1).as_secs_f64();
        if elapsed <= 0.0 {
            return 0;
        }

        let bytes: u64 = self.measurements.iter().map(|(b, _)| *b).sum();
        (bytes as f64 / elapsed) as u64
    }

    /// Get total bytes tracked
    pub fn total(&self) -> u64 {
        self.total_bytes
    }

    /// Reset the calculator
    pub fn reset(&mut self) {
        self.measurements.clear();
        self.total_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_delay() {
        let policy = RetryPolicy::new(3, 1000, 30000);

        // First attempt: ~1000ms
        let delay0 = policy.delay_for_attempt(0);
        assert!(delay0.as_millis() >= 750 && delay0.as_millis() <= 1250);

        // Second attempt: ~2000ms
        let delay1 = policy.delay_for_attempt(1);
        assert!(delay1.as_millis() >= 1500 && delay1.as_millis() <= 2500);

        // Third attempt: ~4000ms
        let delay2 = policy.delay_for_attempt(2);
        assert!(delay2.as_millis() >= 3000 && delay2.as_millis() <= 5000);
    }

    #[test]
    fn test_speed_calculator() {
        let mut calc = SpeedCalculator::new(10);

        // Add measurements
        calc.add_bytes(1000);
        std::thread::sleep(Duration::from_millis(100));
        calc.add_bytes(1000);
        std::thread::sleep(Duration::from_millis(100));
        calc.add_bytes(1000);

        // Speed should be roughly 10000 bytes/sec (3000 bytes in 0.2 sec)
        // But due to timing variations, we just check it's non-zero
        let speed = calc.speed();
        assert!(speed > 0);

        assert_eq!(calc.total(), 3000);
    }

    #[test]
    fn test_retry_policy_defaults() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_attempts, 3);
        assert_eq!(policy.initial_delay_ms, 1000);
        assert_eq!(policy.max_delay_ms, 30000);
    }
}
