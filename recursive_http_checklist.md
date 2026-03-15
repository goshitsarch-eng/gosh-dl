# Recursive HTTP Implementation Checklist

This checklist breaks [recursive_http_design.md](/home/gosh/Github/gosh-dl/recursive_http_design.md) into issue-sized tasks that can be implemented incrementally without destabilizing the existing HTTP engine.

## Current Status

Completed:

- Milestone 0: feature gate and public recursive types
- Milestone 1: crawler module, normalization, scope checks, HTML link extraction, and filtering
- Milestone 2: manifest builder and public discovery API
- Milestone 3: recursive execution orchestration, option propagation, redirect scope, fail-fast, tracked parent jobs, and rollback on partial enqueue failure
- Milestone 4.1-4.3: unit and integration coverage for discovery, execution, restart behavior, parent lifecycle, and parent events
- Milestone 5.1: engine documentation
- Milestone 6.1-6.2: aggregate parent tracking and parent-job persistence

Still open:

- Milestone 4.4: fuzz coverage
- Milestone 5.2: CLI support in `gosh-dl-cli`
- Milestone 6.3: extended mirroring behavior
- persisted parent-level event/progress history and resumable discovery cursors

## Working Rules

- Do not change the behavior of `add_http()`.
- Keep recursive support behind a feature gate until the integration tests are stable.
- Reuse existing HTTP download execution for child files whenever possible.
- Prefer landing pure refactors and test scaffolding before feature behavior.

## Merge Order

1. feature gate and type scaffolding
2. URL normalization and scope filtering
3. HTML link extraction
4. manifest builder
5. engine orchestration
6. integration tests
7. CLI wiring
8. optional aggregate job follow-ups

## Milestone 0: Groundwork

### Task 0.1: Add feature gate

Scope:

- add `recursive-http` Cargo feature
- wire any new modules behind that feature

Files:

- [Cargo.toml](/home/gosh/Github/gosh-dl/Cargo.toml)
- [src/lib.rs](/home/gosh/Github/gosh-dl/src/lib.rs)
- [src/http/mod.rs](/home/gosh/Github/gosh-dl/src/http/mod.rs)

Acceptance criteria:

- default build remains unchanged
- enabling `recursive-http` compiles even before full implementation

### Task 0.2: Add public type scaffolding

Scope:

- add `RecursiveOptions`
- add `RecursiveManifest`
- add `RecursiveEntry`
- add `RecursiveJob`
- re-export them from the public API

Files:

- [src/protocol/options.rs](/home/gosh/Github/gosh-dl/src/protocol/options.rs)
- [src/types.rs](/home/gosh/Github/gosh-dl/src/types.rs)
- [src/lib.rs](/home/gosh/Github/gosh-dl/src/lib.rs)

Acceptance criteria:

- types are documented
- no existing type signatures change
- no new enum variants are added yet

## Milestone 1: Discovery Core

### Task 1.1: Create crawler module skeleton

Scope:

- add `src/http/crawl.rs`
- define internal structs for crawl state and traversal policy
- add placeholder entry points for discovery

Files:

- [src/http/crawl.rs](/home/gosh/Github/gosh-dl/src/http/crawl.rs)
- [src/http/mod.rs](/home/gosh/Github/gosh-dl/src/http/mod.rs)

Acceptance criteria:

- module compiles behind `recursive-http`
- no existing HTTP tests need to change

### Task 1.2: Implement URL normalization

Scope:

- resolve relative URLs against parent page URLs
- strip fragments
- reject unsupported schemes
- normalize path segments
- preserve enough query information to distinguish resources safely

Files:

- [src/http/crawl.rs](/home/gosh/Github/gosh-dl/src/http/crawl.rs)

Acceptance criteria:

- deterministic normalized URL output
- duplicate links normalize to one canonical representation
- parent-directory escapes are rejected

Suggested tests:

- relative path resolution
- `./` and `../` handling
- fragment stripping
- unsupported scheme rejection
- duplicate canonicalization

### Task 1.3: Implement traversal scope filtering

Scope:

- same-host policy
- root-path-prefix policy
- optional include/exclude pattern evaluation
- depth accounting

Files:

- [src/http/crawl.rs](/home/gosh/Github/gosh-dl/src/http/crawl.rs)

Acceptance criteria:

- out-of-scope URLs are never enqueued for discovery or download
- scope decisions are covered by unit tests

Suggested tests:

- sibling directory rejection
- parent escape rejection
- cross-host rejection
- include/exclude precedence

### Task 1.4: Select and integrate HTML parser

Scope:

- choose a small HTML parser crate
- extract `<a href>` values robustly from imperfect directory index pages
- avoid custom parsing beyond glue code

Files:

- [Cargo.toml](/home/gosh/Github/gosh-dl/Cargo.toml)
- [src/http/crawl.rs](/home/gosh/Github/gosh-dl/src/http/crawl.rs)

Acceptance criteria:

- parser handles minimal Apache/nginx-style index pages
- malformed HTML does not panic

### Task 1.5: Implement link extraction

Scope:

- fetch HTML page
- enforce maximum discovery page size
- parse links
- normalize and filter them
- classify directory candidates vs file candidates

Files:

- [src/http/crawl.rs](/home/gosh/Github/gosh-dl/src/http/crawl.rs)

Acceptance criteria:

- only filtered, normalized candidates are returned
- non-HTML responses are treated as file resources
- redirects still respect traversal policy

Suggested tests:

- plain directory page
- malformed HTML
- HTML with absolute and relative links
- redirect within scope
- redirect out of scope

## Milestone 2: Manifest Builder

### Task 2.1: Implement local path mapping

Scope:

- derive relative local paths from discovered URLs
- preserve nested directories
- reject invalid path components
- define collision behavior

Files:

- [src/http/crawl.rs](/home/gosh/Github/gosh-dl/src/http/crawl.rs)

Acceptance criteria:

- every manifest entry has a deterministic relative path
- mapping cannot escape the local root
- collisions fail clearly unless explicitly allowed

Suggested tests:

- nested path preservation
- root-relative file mapping
- invalid component rejection
- collision detection

### Task 2.2: Build recursive manifest

Scope:

- walk discovery frontier breadth-first or depth-first
- dedupe visited pages and discovered files
- enforce page and file caps
- return `RecursiveManifest`

Files:

- [src/http/crawl.rs](/home/gosh/Github/gosh-dl/src/http/crawl.rs)

Acceptance criteria:

- manifest order is stable for testability
- duplicates produce one entry
- crawl terminates on cycles

Suggested tests:

- nested directories
- duplicate links across multiple pages
- self-referential page cycle
- file count cap

### Task 2.3: Add public discovery API

Scope:

- implement `discover_http_recursive()` on `DownloadEngine`
- validate root URL and options
- delegate to crawler module

Files:

- [src/engine.rs](/home/gosh/Github/gosh-dl/src/engine.rs)
- [src/http/crawl.rs](/home/gosh/Github/gosh-dl/src/http/crawl.rs)

Acceptance criteria:

- callers can preview recursive downloads without starting transfers
- errors are surfaced before any child download is created

## Milestone 3: Execution Orchestration

### Task 3.1: Implement child download expansion

Scope:

- implement `add_http_recursive()`
- expand manifest entries into ordinary child `add_http()` calls
- map entry parent directories into `save_dir`
- map basename into `filename`

Files:

- [src/engine.rs](/home/gosh/Github/gosh-dl/src/engine.rs)

Acceptance criteria:

- child downloads are standard `Http` downloads
- no new `DownloadKind` is required
- existing queueing and retry behavior is reused

### Task 3.2: Propagate request options correctly

Scope:

- ensure headers, cookies, user agent, referer, priority, mirrors, and checksum behavior are handled correctly for child entries
- explicitly decide which `DownloadOptions` fields are inherited and which are overridden

Files:

- [src/engine.rs](/home/gosh/Github/gosh-dl/src/engine.rs)

Acceptance criteria:

- authenticated directory downloads work for both discovery and child file fetches
- inherited options are documented and tested

### Task 3.3: Define partial failure behavior

Scope:

- decide how `add_http_recursive()` behaves when some child downloads are created and a later child creation fails
- document whether the method is best-effort or all-or-nothing

Files:

- [src/engine.rs](/home/gosh/Github/gosh-dl/src/engine.rs)
- [recursive_http_design.md](/home/gosh/Github/gosh-dl/recursive_http_design.md)

Acceptance criteria:

- behavior is deterministic
- tests cover partial enqueue failure

Implemented behavior:

- recursive enqueue is transactional at the engine boundary: if child creation fails partway through, already-added children are rolled back before returning the error

## Milestone 4: Test Matrix

### Task 4.1: Add crawler unit tests

Scope:

- add dedicated tests for normalization, scope, path mapping, dedupe, and cycle handling

Files:

- [src/http/crawl.rs](/home/gosh/Github/gosh-dl/src/http/crawl.rs)

Acceptance criteria:

- edge-case behavior is covered without needing the full engine

### Task 4.2: Add HTTP integration tests for recursive discovery

Scope:

- use `wiremock` to simulate directory indexes and files
- validate discovered manifest contents

Files:

- [tests/integration_tests.rs](/home/gosh/Github/gosh-dl/tests/integration_tests.rs)

Acceptance criteria:

- manifest generation works against realistic mock servers

Suggested scenarios:

- one-level directory
- nested directories
- duplicate links
- external links ignored
- redirect handling
- auth propagation

### Task 4.3: Add HTTP integration tests for recursive execution

Scope:

- validate that child files are actually downloaded to the expected local paths
- verify child completion and error behavior

Files:

- [tests/integration_tests.rs](/home/gosh/Github/gosh-dl/tests/integration_tests.rs)

Acceptance criteria:

- files land in the correct directories
- current non-recursive HTTP tests still pass unchanged

### Task 4.4: Add fuzz coverage

Status: remaining

Scope:

- add a fuzz target for HTML link extraction or URL normalization

Files:

- [fuzz/Cargo.toml](/home/gosh/Github/gosh-dl/fuzz/Cargo.toml)
- [fuzz/fuzz_targets](/home/gosh/Github/gosh-dl/fuzz/fuzz_targets)

Acceptance criteria:

- at least one parser-facing fuzz target exists
- no panics on malformed discovery inputs

## Milestone 5: Documentation and CLI

### Task 5.1: Document engine API

Status: completed

Scope:

- document recursive APIs and current scope limitations
- clarify that this is directory mirroring, not full web mirroring

Files:

- [README.md](/home/gosh/Github/gosh-dl/README.md)
- [technical_spec.md](/home/gosh/Github/gosh-dl/technical_spec.md)

Acceptance criteria:

- docs match shipped behavior exactly

### Task 5.2: CLI support

Status: remaining

Scope:

- add `--recursive`
- add depth and filtering flags
- surface a preview mode if practical

External repo:

- `gosh-dl-cli`

Acceptance criteria:

- CLI behavior maps cleanly onto engine APIs
- CLI help text explains limitations

## Milestone 6: Optional Follow-Ups

These should not block the first release.

### Task 6.1: Aggregate recursive job tracking

Status: completed

Scope:

- parent job IDs
- aggregate progress
- recursive-specific events

### Task 6.2: Parent-job persistence

Status: completed

Scope:

- persist recursive job metadata and discovery state
- add schema migration only after the parent model is stable

### Task 6.3: Extended mirroring behavior

Status: remaining

Scope:

- optional cross-host traversal
- robots.txt handling
- offline page asset rewriting
- machine-readable listing formats

## Recommended First PR Stack

To minimize review risk, the first few PRs should be:

1. `recursive-http` feature gate + public types
2. crawler module skeleton + URL normalization tests
3. scope filtering + link extraction
4. manifest builder + engine discovery API
5. recursive execution via child `add_http()` calls
6. integration tests
7. docs
8. parent aggregate tracking + persistence + events

## Exit Criteria for Initial Release

The feature is ready for first release when all of the following are true:

- recursive support is still opt-in
- `add_http()` behavior is unchanged
- manifest discovery is deterministic and bounded
- recursive execution uses the existing HTTP download path
- existing HTTP integration tests pass unchanged
- new recursive unit and integration tests are green
- docs clearly state supported and unsupported behavior
