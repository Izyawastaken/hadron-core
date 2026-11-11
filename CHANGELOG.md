# Changelog

## 0.0.2 — 2026-11-12

### Fixed

- Pin PR validation to complete immutable Git trees and the comparison merge base,
  removing the race with GitHub's mutable PR file listing. Truncated trees fail closed.
- Reconcile merged requests using the target repository's merge/squash result and
  first parent. Deleted forks and subsequently edited head branches no longer block
  recovery. Already-applied requests return through their atomic ledger marker.
- Resume an uploaded publication using its existing verified index, retaining exact
  asset IDs even after another release is selected. Revalidate against source before
  finishing visibility and guarded pointer advancement.
- Publish fresh artifacts when optional prior metadata/configs are missing or corrupt.
  Permission, transport and cancellation errors still propagate.
- Enforce the consumer JSON limit during manifest preparation; separate config read
  budgets from JSON budgets; allow bounded base64 expansion for Git blob envelopes.
- Validate request publication identifiers and nonzero applied PR numbers.

### Improved

- Compare immutable config Git entries before rebuilding: unchanged bundles skip
  source downloads/compression, and changed bundles reuse verified unchanged files.
  Config source reads run with bounded concurrency and a prechecked total byte budget.
- Stream deterministic tar encoding into ZSTD-9 and decode archives directly into
  verified files, removing complete uncompressed tar buffers. Hash/count verification
  includes the whole decoded stream. Honor smaller caller decoder windows at encode.
- Move upload/cache buffers instead of cloning mutation bodies and cache insertions.
- Limit simultaneous heavy workflows through `Options::workflows` (default 1), with
  cancellable admission. This controls workflow count, not a hard process RAM quota.
- Target submission retry lookup by head owner/branch and target branch, independent
  of unrelated PR history. Add `Hadron::proposals_page` for bounded history browsing.
- Bound conditional pointer metadata to 128 repositories; add `Github::clear_metadata`.

### Added

- `CachePolicy::Recover` opt-in refetch after local corruption and tolerance for local
  cache I/O failure. Strict remains default. Remote integrity errors never become hits.
- Explicit cache `size`, `remove` and deterministic `prune` operations. Eviction is
  caller-controlled and is not an automatic cross-process disk quota.
- `Hadron::archive_requests` atomically moves applied requests to per-ID source
  records while reserving IDs and preserving replay provenance. Pending requests
  cannot be archived. Upgrade all writers before using archives; 0.0.1 did not read them.
- `Stage::ReuseSkipped` and `Stage::CacheSkipped` diagnostics.
- Regression coverage for mutable PR heads, target advancement, truncated trees,
  deleted forks, publication selection changes, broken prior assets, config reuse,
  size boundaries, cache recovery/eviction, metadata bounds and queued cancellation.

### Compatibility

- Wire artifact schema remains 1; existing publications remain readable. Compression
  output may change with streaming/settings; cross-version compressed identity is not
  promised. `Options` struct literals without `..Default::default()` need `workflows`;
  exhaustive `Stage` matches need the two new variants. Rust 1.97 minimum is unchanged.
- Multi-commit rebases whose final commit does not contain the complete request delta
  fail closed. Hadron creates a single request commit and accepts via merge commit.
- Integration tests use local mock GitHub transport; no live modpack was modified.

## 0.0.1

- Strict schema-1 pack, mod, request, publication and pointer models.
- Portable path validation, exact operations and deterministic update plans.
- Neutrino repository share codes without a central registry.
- ZSTD-9 manifests and deterministic configuration tar artifacts with bounded decoding.
- Verified atomic artifact caching.
- Octocrab transport with bounded responses, cancellation and conservative read retries.
- Request PR review, exact-head merge, atomic source reconciliation and resumable
  release publication with guarded stable selection.
- Detailed workflow, format, authentication, compatibility and limitations guide.
