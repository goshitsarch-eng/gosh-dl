# 0.6.1 rollout audit

Audit baseline: `d475e65` (`main`, version 0.6.0). The latest GitHub release
at audit time was 0.5.0. No open GitHub issues were returned.

## Defects addressed

| Area | Observable failure | Resolution |
| --- | --- | --- |
| HTTP authentication | The initial HEAD probe omitted authorization, Referer, and cookies, so valid authenticated downloads could fail before GET. | Apply request credentials to probes and the single-stream metadata request. |
| HEAD fallback | A failed HEAD request prevented the documented GET fallback. Error-page headers could also become download metadata. | Fall back to GET and use only successful HEAD metadata. |
| Output paths | Unsafe filenames were stored before worker-side validation; small nested downloads could not create their parent directories. | Validate before enqueue and create nested parents. |
| Nested lifecycle | Completion kept only the basename, so verify/read/repair/delete later targeted the wrong file. | Preserve the path relative to the download directory. |
| Streaming lifetime | Dropping a reader while it waited for data left its task and engine reference alive. | Abort the pump on reader drop. |
| Missing completed files | Readers retried missing/truncated completed HTTP files indefinitely. | Terminate the stream; callers can detect short reads from the expected size. |
| Segmented streaming | Paused/restarted downloads used aggregate byte counts as readable prefixes, exposing preallocated holes. | Restore segment state and compute the gap-free prefix. |
| Torrent pause/resume | Queued torrents could start after pause; paused torrents held concurrency slots. | Stop the old worker and resume through a fresh queued worker. |
| Torrent repair | Changing the engine state did not restart a finished worker or leave seeding mode. | Reconstruct the downloader with its metainfo and restart through the queue. |
| Torrent restart APIs | Verify and streaming depended on a live downloader handle. | Build a verified piece bitmap from persisted metainfo without starting peers. |
| Verification | Queued downloads could be verified while about to start; directories could satisfy an HTTP size-only check. | Reject queued verification and require regular files. |
| Metalink | Truncated/multiple roots were accepted; an FTP URL could prevent use of an available HTTP mirror. | Enforce root closure and choose supported transports. |
| CI and release | Recursive HTTP was absent from platform tests; no automated release gate existed; fuzz lockfile still described 0.5.0. | Expand the matrix, refresh lockfiles, and gate source-package releases on CI. |

The first five new HTTP regressions were run against the unmodified
implementation and all failed. The corrected implementation passes them.
Further tests cover restored segmented reads, torrent queue/repair/restart,
and malformed or mixed-transport Metalinks.

## Validation and release gate

- Full feature tests exercise HTTP retries/resume, checksums, mirrors,
  recursive jobs, storage, Metalink, mock torrent peers, inbound connections,
  and uTP transfers.
- Minimal-feature tests check that HTTP/torrent integrations remain optional.
- Formatting, Clippy, Rust 1.85 compatibility with the committed lockfile,
  and verified Cargo packaging are release gates.
- GitHub CI runs the feature matrix on Linux, macOS, and Windows, plus the
  four existing fuzz smoke targets. The release workflow runs this suite
  before creating a version tag and attaching the `.crate` source package.
- This is a library release, not a standalone application binary. Publishing
  to crates.io requires a separate registry publishing setup; the workflow
  publishes a GitHub release only.

## Remaining scope and validation work

| Priority | Area | Remaining work |
| --- | --- | --- |
| Before broad network rollout | DHT, PEX, LPD, encryption, trackers and proxies | Run interoperability and long-duration tests with representative real servers/clients. DHT network tests are intentionally ignored locally; proxy coverage is still missing. Fixture tests do not prove every real-world combination. |
| Before depending on inbound encryption | MSE responder | Inbound MSE remains unimplemented. Outgoing encryption is implemented; inbound peers are plaintext-only under compatible policies. |
| Before depending on IPv6-only trackerless discovery | DHT IPv6 | Remains dependent on upstream `mainline` support. |
| Before using Metalink as a complete RFC implementation | Metalink subsets | Torrent `metaurl`, piece hashes, and unsupported whole-file hash algorithms remain ignored; declared size is parsed but not an independent download integrity constraint. Supply a supported whole-file checksum. |
| Before enabling experimental features broadly | uTP and recursive mirroring | Keep opt-in during deployment trials. Test large swarms, lossy/WAN paths, authentication redirects, large directory trees, and filesystem behavior used by the consuming app. |
| Follow-up hardening | Persistence and lifecycle concurrency | Add process-kill/power-loss recovery tests and simultaneous pause/resume/cancel stress tests. Current tests cover selected restart and queue scenarios, not every operation interleaving. |

The 0.6.1 patch addresses the reproducible failures above. It does not claim
that every optional feature is production-proven, and it is not a 1.0 API
stability declaration.
