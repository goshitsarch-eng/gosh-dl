# Recursive HTTP Directory Mirroring Design

## Status

Implemented behind the opt-in `recursive-http` feature flag as of 2026-03-15.

Current implementation includes:

- recursive discovery from HTML directory/index pages
- URL normalization, same-host filtering, root-prefix enforcement, depth limits, and include/exclude filters
- manifest generation and expansion into ordinary child HTTP downloads
- partial-enqueue rollback so failed child creation does not leave orphan downloads behind
- redirect-scope enforcement during discovery, child downloads, and resumed child downloads restored from storage
- optional in-memory and persisted `fail_fast` sibling cancellation
- tracked parent recursive jobs with aggregate status, lifecycle methods, dedicated parent events, and SQLite persistence

Still intentionally deferred:

- full `wget -r` parity
- JavaScript rendering or asset rewriting
- first-class parent jobs in the main download queue/state machine
- persisted parent-level event/progress history
- CLI integration in this repository

---

## Problem Statement

`gosh-dl` currently supports:

- explicit HTTP/HTTPS downloads via `add_http(url, options)`
- multi-file torrent downloads where the file tree is defined by torrent metadata

It does **not** currently support:

- crawling an HTML index page
- recursively discovering child links
- mirroring a remote directory tree to local storage

Users want a workflow closer to `wget -r`, but adding that behavior directly to the existing HTTP download path would create an unnecessarily large regression surface. The current HTTP path is intentionally built around one resource per download, with mature logic for segmented transfer, retry, progress, resume, persistence, and lifecycle management.

The design goal is to add recursive directory mirroring as a higher-level orchestration layer while preserving the current per-file HTTP download engine unchanged wherever possible.

---

## Goals

- Add opt-in recursive HTTP/HTTPS directory mirroring.
- Preserve the current `add_http()` API and semantics.
- Reuse existing HTTP download, queueing, retry, pause/resume, and persistence logic for each discovered file.
- Keep the initial implementation narrowly scoped to directory mirroring, not generic web crawling.
- Bound the blast radius of new code through modular discovery and strong test coverage.

## Non-Goals

- Full `wget -r` parity in the first version.
- Arbitrary website crawling across domains.
- Rendering JavaScript or executing browser logic.
- Parsing every possible HTML edge case in v1.
- Changing `DownloadKind`, `DownloadState`, or `DownloadEvent` in the initial rollout.
- Deep storage schema changes in the first implementation.

---

## Design Principles

1. Keep single-resource HTTP downloads as the stable primitive.
2. Treat recursive mirroring as orchestration plus discovery, not as a new transport.
3. Prefer additive APIs over changing existing method contracts.
4. Scope traversal tightly by default to avoid surprising or unsafe behavior.
5. Keep recursive behavior additive: parent job tracking should not change existing single-download semantics or `DownloadEvent`.

---

## User-Facing Scope

### Supported in MVP

- Recursive traversal starting from a root HTTP/HTTPS URL
- Relative link discovery from HTML directory/index pages
- Downloading file links under the configured root scope
- Preserving remote relative paths locally
- Auth/header propagation to discovered child requests
- Bounded recursion depth and bounded crawler concurrency

### Explicitly Deferred

- CSS, JS, image, and asset rewriting for offline page mirroring
- Cross-host crawling by default
- Robots.txt handling
- HTML form workflows
- Sitemap crawling
- Browser-like handling of dynamically generated links

---

## High-Level Architecture

The feature should be split into two layers:

### 1. Discovery Layer

A new module, proposed as `src/http/crawl.rs`, is responsible for:

- fetching candidate index pages
- parsing links
- resolving relative URLs
- normalizing and deduplicating URLs
- filtering links by traversal policy
- classifying URLs as directories or files
- producing a deterministic manifest of downloads

This layer does **not** write files to disk and does **not** perform segmented downloads.

### 2. Execution Layer

The engine orchestrates the manifest by converting each discovered file entry into a normal HTTP download using the existing code path:

- queue acquisition
- `HttpDownloader::download_segmented(...)`
- progress updates
- retries and resume
- storage persistence
- completion/error handling

This preserves most of the tested HTTP implementation in `src/http/mod.rs`.

---

## Why Not Extend `add_http()`?

Overloading `add_http()` with recursive behavior would make existing callers vulnerable to:

- changed semantics for directory-like URLs
- new error modes from HTML parsing and traversal
- new lifecycle complexity inside the current single-file path
- harder-to-predict persistence behavior

Keeping recursion in separate methods makes compatibility straightforward:

- `add_http()` remains one URL -> one download
- recursive mirroring is opt-in and explicit

---

## Proposed Public API

### New Types

```rust
pub struct RecursiveOptions {
    pub max_depth: usize,
    pub same_host_only: bool,
    pub allowed_prefix: Option<String>,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub preserve_paths: bool,
    pub overwrite_existing: bool,
    pub fail_fast: bool,
    pub max_discovery_concurrency: usize,
}

pub struct RecursiveManifest {
    pub root_url: String,
    pub entries: Vec<RecursiveEntry>,
}

pub struct RecursiveEntry {
    pub url: String,
    pub relative_path: std::path::PathBuf,
    pub size_hint: Option<u64>,
}

pub struct RecursiveJob {
    pub root_url: String,
    pub child_ids: Vec<DownloadId>,
}
```

### New Engine Methods

```rust
impl DownloadEngine {
    pub async fn discover_http_recursive(
        &self,
        root_url: &str,
        options: &DownloadOptions,
        recursive: &RecursiveOptions,
    ) -> Result<RecursiveManifest>;

    pub async fn add_http_recursive(
        &self,
        root_url: &str,
        options: DownloadOptions,
        recursive: RecursiveOptions,
    ) -> Result<RecursiveJob>;
}
```

### Compatibility Notes

- `DownloadOptions` remains unchanged in v1.
- Recursive behavior is not hidden behind an extra field on `DownloadOptions`.
- Child downloads are standard HTTP downloads and continue to appear as `DownloadKind::Http`.

This avoids immediate API churn in `src/protocol/types.rs` and `src/protocol/events.rs`.

---

## Traversal Rules

Default traversal policy should be intentionally conservative.

### Default Behavior

- only `http` and `https`
- only URLs on the same host as the root URL
- only URLs whose normalized path is under the root path prefix
- no fragment-based duplication
- no `..` escape outside the root scope
- no query-only duplicates unless explicitly configured later

### URL Normalization

Before deduplication and filtering:

- resolve relative URLs against the parent page URL
- strip fragments
- normalize path segments
- preserve query strings for file URLs in v1 only if they are required to distinguish resources
- reject unsupported schemes such as `file:`, `data:`, `javascript:`, `mailto:`

### Directory vs File Heuristics

The crawler needs a pragmatic classifier:

- URLs ending with `/` are treated as directory candidates
- responses with `text/html` are treated as crawlable pages
- non-HTML responses are treated as file resources
- links to parent directories should be ignored if they escape the configured root prefix

This is intentionally narrower than a browser crawler.

---

## Local Path Mapping

The manifest should carry a relative local path for each discovered file.

Example:

- root URL: `https://example.com/pub/`
- discovered file: `https://example.com/pub/releases/v1/app.tar.gz`
- local relative path: `releases/v1/app.tar.gz`

### Mapping Rules

- compute paths relative to the configured root URL path
- preserve nested directories by default
- reject or sanitize invalid path components
- reuse the existing path traversal protections already present in the HTTP download path
- handle filename collisions deterministically

### Collision Policy

For MVP:

- identical normalized URL -> one manifest entry
- different URLs mapping to the same local relative path -> fail discovery unless `overwrite_existing` is explicitly enabled

Silently renaming collisions would make reproducibility and resume semantics harder.

---

## Engine Integration

`add_http_recursive()` should behave as a thin coordinator:

1. Validate root URL and recursive options.
2. Run discovery and build a manifest.
3. For each manifest entry:
   - clone the base `DownloadOptions`
   - set `save_dir` to the root save directory plus the entry's parent path
   - set `filename` to the entry's basename
   - call existing `add_http(entry.url, child_options)`
4. Return the list of child `DownloadId` values.

This keeps all transfer execution in the already-tested HTTP path in `src/engine.rs` and `src/http/mod.rs`.

### Why Child Downloads Instead of a New Aggregate Download Type?

Using standard child downloads gives us, for free:

- existing progress events
- queueing and priority handling
- pause/resume/cancel behavior
- segmented HTTP support
- storage persistence
- compatibility with existing CLI/UI assumptions

The tradeoff is that the recursive job itself is tracked outside the main download queue/state machine. That remains acceptable for the current implementation.

---

## Persistence Strategy

### Current Implementation

Recursive persistence now has two layers:

- child HTTP downloads persist exactly as normal downloads, plus a runtime metadata sidecar for redirect scope and fail-fast context
- tracked parent recursive jobs persist independently in the `recursive_jobs` table

Current limitation:

- if the process stops mid-discovery, the crawl itself is not resumable from a discovery cursor
- already-enqueued child downloads resume normally
- tracked parent jobs restore as aggregate views over those child downloads

### Future Extension

If recursive mirroring needs resumable discovery, add optional parent-job state such as:

- manifest checksum / discovery cursor state
- page frontier persistence
- parent-level event/progress history

---

## Event Model

### Current Implementation

`DownloadEvent` is still unchanged for child downloads.

Recursive parent jobs now also emit a separate event stream:

- `RecursiveJobEvent::Added`
- `RecursiveJobEvent::Updated`
- `RecursiveJobEvent::Removed`

This keeps existing consumers stable while giving callers an aggregate parent-facing stream.

### Optional Later Additions

If consumers need richer recovery or analytics, add persisted parent event/progress history later instead of expanding `DownloadEvent`.

---

## Failure Semantics

### Discovery Failures

Examples:

- root URL is unreachable
- root URL is not HTML and not a directory-like path
- HTML parsing fails catastrophically
- link normalization hits an invalid state

Default behavior:

- fail the recursive request before enqueueing child downloads when discovery itself fails
- roll back already-enqueued children if enqueueing fails partway through

### Child Download Failures

Once a manifest has been built and downloads have been enqueued:

- each child file uses existing HTTP error semantics
- recursive orchestration does not cancel sibling downloads by default

`fail_fast` is now implemented. When enabled, the first child failure propagates a synthetic non-retryable failure to queued/active siblings in the same recursive group. The default remains independent child behavior.

---

## Security and Safety

This feature expands the attack surface and needs explicit safeguards.

### Must-Have Safeguards

- enforce scheme allowlist: `http`, `https`
- reject cross-host traversal by default
- reject traversal outside the configured root path prefix
- strip fragments before dedupe
- preserve existing local path traversal protections
- bound discovery depth
- bound discovery concurrency
- bound the total number of discovered entries with a configurable cap

### Recommended Additional Safeguards

- cap HTML response size for discovery pages
- cap total discovered pages
- ignore duplicate pages after normalization
- enforce a redirect policy that still respects host/path scope

---

## Implementation Plan

### Phase 1: Discovery Core

Status: completed

- add `src/http/crawl.rs`
- implement:
  - `RecursiveOptions`
  - URL normalization
  - scope filtering
  - HTML link extraction
  - manifest generation
- add unit tests for canonicalization and filtering

### Phase 2: Engine Orchestration

Status: completed

- add `discover_http_recursive()`
- add `add_http_recursive()`
- route manifest entries into existing `add_http()`
- ensure option propagation for headers, cookies, user agent, referer, and priority

### Phase 3: Integration Tests

Status: completed for engine coverage

Add `wiremock` scenarios for:

- simple directory index with files
- nested directories
- external links ignored
- duplicate links deduped
- redirects within scope
- redirects out of scope rejected
- authenticated discovery and child downloads
- path collision handling
- root path escape attempts

### Phase 4: CLI Integration

Status: not started in this repository

After engine tests are stable:

- expose a CLI flag such as `--recursive`
- expose depth and filtering flags
- document current scope as directory mirroring, not generic website mirroring

### Phase 5: Optional Aggregate Model

Status: partially completed

Implemented:

- parent recursive job tracking
- aggregate progress/state
- recursive-specific events
- tracked parent-job persistence

Still open:

- persistent manifest/discovery cursor state
- first-class parent job scheduling semantics
- persisted parent event/progress history

---

## Testing Strategy

### Unit Tests

- URL normalization
- root scope enforcement
- relative link resolution
- local path mapping
- collision detection
- redirect scope validation

### Integration Tests

Use `wiremock` to verify end-to-end behavior:

- discovery fetches only expected pages
- enqueued child downloads use the existing HTTP path
- child progress/completion events continue to work
- current `add_http()` tests remain unchanged

### Regression Protection

The most important regression check remains that existing HTTP integration tests continue to pass unchanged. Recursive support is additive and intentionally reuses the existing HTTP transfer path for child downloads.

### Fuzzing

Add at least one fuzz target for:

- HTML href extraction, or
- URL normalization and path mapping

Parser edge cases are one of the highest-risk areas for accidental regressions or security bugs.

---

## Cargo Feature Gating

To reduce rollout risk, ship the first version behind a feature flag:

```toml
[features]
default = ["http", "torrent", "storage"]
recursive-http = []
```

This lets maintainers:

- merge incrementally
- keep the default API stable
- gather feedback before making it a default capability

If the extra dependency footprint is acceptable and the implementation proves stable, the feature can be folded into the default `http` feature later.

---

## Dependencies

The crawler likely needs an HTML parser. Candidate approaches:

- use a small HTML parsing crate and extract `<a href>`
- avoid writing a custom parser beyond trivial tokenization

Selection criteria:

- low dependency footprint
- tolerant parsing for imperfect directory index pages
- no browser/runtime requirements

This choice should be made before Phase 1 implementation.

---

## Open Questions

1. Should query strings be part of local filename derivation in v1, or should they only affect deduplication?
2. Should `index.html` itself be downloaded when it is used only as a discovery page?
3. Should non-HTML directory listings with machine-readable formats be supported later?
4. Do we want a hard default cap for total discovered files, even if not exposed publicly at first?
5. Should recursive discovery and child downloads share one auth configuration, or should callers be able to override child request options separately?

---

## Recommended First Milestone

The current implementation follows this narrow contract:

- crawl HTML directory indexes only
- same-host only
- same-root-path-prefix only
- preserve remote directory structure locally
- enqueue discovered files as ordinary HTTP downloads
- tracked parent jobs persisted outside the main download model
- no new variants added to `DownloadEvent`
- feature-gated behind `recursive-http`

The remaining follow-up work is around resumable discovery state, fuzzing, CLI wiring, and broader mirroring semantics.
