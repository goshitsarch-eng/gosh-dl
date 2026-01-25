//! Resume Detection and Validation
//!
//! This module handles detecting resume capability and validating
//! that a partially downloaded file can be safely resumed.

use crate::error::{EngineError, ProtocolErrorKind, Result};
use reqwest::Client;
use std::path::Path;
use tokio::fs;

/// Information about resume capability
#[derive(Debug, Clone)]
pub struct ResumeInfo {
    /// Whether the server supports Range requests
    pub supports_range: bool,
    /// ETag for validation
    pub etag: Option<String>,
    /// Last-Modified for validation
    pub last_modified: Option<String>,
    /// Content-Length
    pub content_length: Option<u64>,
    /// Can safely resume from existing partial file
    pub can_resume: bool,
    /// Size of existing partial file
    pub existing_size: u64,
}

/// Check if a download can be resumed
pub async fn check_resume(
    client: &Client,
    url: &str,
    user_agent: &str,
    part_path: &Path,
    saved_etag: Option<&str>,
    saved_last_modified: Option<&str>,
) -> Result<ResumeInfo> {
    // Check if partial file exists
    let existing_size = if part_path.exists() {
        fs::metadata(part_path).await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    // Send HEAD request to check server capabilities
    let response = client
        .head(url)
        .header("User-Agent", user_agent)
        .send()
        .await
        .map_err(|e| {
            EngineError::protocol(
                ProtocolErrorKind::InvalidResponse,
                format!("HEAD request failed: {}", e),
            )
        })?;

    if !response.status().is_success() {
        return Err(EngineError::protocol(
            ProtocolErrorKind::InvalidResponse,
            format!("HEAD request returned: {}", response.status()),
        ));
    }

    let headers = response.headers();

    // Check Accept-Ranges header
    let supports_range = headers
        .get("accept-ranges")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("bytes"))
        .unwrap_or(false);

    // Get ETag
    let etag = headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Get Last-Modified
    let last_modified = headers
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Get Content-Length
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    // Determine if we can resume
    let can_resume = if existing_size == 0 {
        // No partial file, nothing to resume
        false
    } else if !supports_range {
        // Server doesn't support ranges
        false
    } else {
        // Validate ETag or Last-Modified if we have saved values
        let etag_valid = match (saved_etag, &etag) {
            (Some(saved), Some(current)) => saved == current,
            (Some(_), None) => false, // Had ETag, now missing
            (None, _) => true,        // Didn't have ETag, can't validate
        };

        let last_modified_valid = match (saved_last_modified, &last_modified) {
            (Some(saved), Some(current)) => saved == current,
            (Some(_), None) => false,
            (None, _) => true,
        };

        // Must pass both validations
        etag_valid && last_modified_valid
    };

    Ok(ResumeInfo {
        supports_range,
        etag,
        last_modified,
        content_length,
        can_resume,
        existing_size,
    })
}

/// Verify that a Range request returns the expected response
pub async fn verify_range_support(client: &Client, url: &str, user_agent: &str) -> Result<bool> {
    // Request just the first byte
    let response = client
        .get(url)
        .header("User-Agent", user_agent)
        .header("Range", "bytes=0-0")
        .send()
        .await
        .map_err(|e| {
            EngineError::protocol(
                ProtocolErrorKind::InvalidResponse,
                format!("Range request failed: {}", e),
            )
        })?;

    // Should get 206 Partial Content
    Ok(response.status() == reqwest::StatusCode::PARTIAL_CONTENT)
}

/// Calculate the range header value for resuming
pub fn calculate_range_header(start: u64, end: Option<u64>) -> String {
    match end {
        Some(end) => format!("bytes={}-{}", start, end),
        None => format!("bytes={}-", start),
    }
}

/// Parse Content-Range header to extract byte positions
///
/// Format: "bytes start-end/total" or "bytes start-end/*"
pub fn parse_content_range(header: &str) -> Option<(u64, u64, Option<u64>)> {
    let header = header.strip_prefix("bytes ")?;
    let parts: Vec<&str> = header.split('/').collect();
    if parts.len() != 2 {
        return None;
    }

    let range_parts: Vec<&str> = parts[0].split('-').collect();
    if range_parts.len() != 2 {
        return None;
    }

    let start = range_parts[0].parse::<u64>().ok()?;
    let end = range_parts[1].parse::<u64>().ok()?;
    let total = if parts[1] == "*" {
        None
    } else {
        parts[1].parse::<u64>().ok()
    };

    Some((start, end, total))
}

/// Validate that a resumed download starts at the expected position
pub fn validate_resumed_position(expected_start: u64, content_range: &str) -> Result<()> {
    let (actual_start, _, _) = parse_content_range(content_range).ok_or_else(|| {
        EngineError::protocol(
            ProtocolErrorKind::InvalidResponse,
            format!("Invalid Content-Range header: {}", content_range),
        )
    })?;

    if actual_start != expected_start {
        return Err(EngineError::protocol(
            ProtocolErrorKind::InvalidResponse,
            format!(
                "Resume position mismatch: expected {}, got {}",
                expected_start, actual_start
            ),
        ));
    }

    Ok(())
}

/// Determine if a partial file should be deleted and restarted
pub async fn should_restart(
    part_path: &Path,
    expected_size: Option<u64>,
    saved_etag: Option<&str>,
    current_etag: Option<&str>,
) -> bool {
    // If file doesn't exist, no need to restart
    if !part_path.exists() {
        return false;
    }

    // If ETag changed, must restart
    if let (Some(saved), Some(current)) = (saved_etag, current_etag) {
        if saved != current {
            return true;
        }
    }

    // If we have expected size and partial is larger, restart
    if let Some(expected) = expected_size {
        if let Ok(metadata) = fs::metadata(part_path).await {
            if metadata.len() > expected {
                return true;
            }
        }
    }

    false
}

/// Clean up a partial file that can't be resumed
pub async fn cleanup_partial(part_path: &Path) -> Result<()> {
    if part_path.exists() {
        fs::remove_file(part_path)
            .await
            .map_err(|e| EngineError::Internal(format!("Failed to remove partial file: {}", e)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_range_header() {
        assert_eq!(calculate_range_header(0, None), "bytes=0-");
        assert_eq!(calculate_range_header(100, None), "bytes=100-");
        assert_eq!(calculate_range_header(0, Some(99)), "bytes=0-99");
        assert_eq!(calculate_range_header(1000, Some(1999)), "bytes=1000-1999");
    }

    #[test]
    fn test_parse_content_range() {
        assert_eq!(
            parse_content_range("bytes 0-99/100"),
            Some((0, 99, Some(100)))
        );

        assert_eq!(
            parse_content_range("bytes 100-199/1000"),
            Some((100, 199, Some(1000)))
        );

        assert_eq!(parse_content_range("bytes 0-99/*"), Some((0, 99, None)));

        assert_eq!(parse_content_range("invalid"), None);
        assert_eq!(parse_content_range("bytes invalid"), None);
    }

    #[test]
    fn test_validate_resumed_position() {
        // Valid cases
        assert!(validate_resumed_position(0, "bytes 0-99/100").is_ok());
        assert!(validate_resumed_position(100, "bytes 100-199/1000").is_ok());

        // Invalid cases
        assert!(validate_resumed_position(50, "bytes 0-99/100").is_err());
        assert!(validate_resumed_position(0, "invalid header").is_err());
    }
}
