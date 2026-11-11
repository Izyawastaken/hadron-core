# hadron-core

**Typed modpack data, verified artifacts, and owner-controlled GitHub workflows.**

Hadron is the reusable Rust layer between a launcher such as Tachyon and a GitHub
pack repository. It translates editable JSON into frontend-ready Rust models, and
explicit user actions into request PRs, source commits, and published artifacts.
Version **0.0.2** implements wire schema **1**. Pin this early API release while
integrating it. MIT-licensed source: <https://github.com/Izyawastaken/hadron-core>.

## Responsibilities and flows

```text
Neutrino code -> repository -> .hadron/latest.json -> publication.json
 -> verified manifest.json.zst + configs.tar.zst -> typed data -> Tachyon

User request -> requests.json PR -> owner review -> merge -> source commit
 -> owner's local ZSTD-9 build -> complete release -> latest pointer commit
```

Hadron owns schemas, validation, diffs, artifacts, repository identity, and explicit
GitHub workflows. Tachyon owns screens, login/token storage, provider resolution,
mod downloads/installation, optional-mod choices, local config conflicts, and game
launching. Hadron never changes worlds or saves, and never runs pack scripts. No
server, registry of codes, daemon, or automatic background watcher is required.

## Installation and features

```toml
[dependencies]
hadron-core = "=0.0.2"
```

Import as `hadron_core`. Minimum supported Rust: **1.97**, edition 2024.

| Features | Capabilities |
| --- | --- |
| No default features | Models, strict JSON, validation, diff, Neutrino |
| `compression` | ZSTD-9 encoding, bounded decoding, config tar |
| `github` | Octocrab, async workflows; implies compression |
| `cache` | Verified filesystem artifact cache |
| Default | `github` and `cache` |

For a manifest-only tool set `default-features = false`. GitHub operations require
a caller-owned Tokio runtime with I/O and time enabled. Hadron creates no runtime.
CPU-heavy workflow operations use bounded blocking workers. zstd compiles native C
code and requires a working platform compiler; TLS dependencies may also compile
native code. Use MSVC Build Tools on Windows and an appropriate compiler on Unix.
This is not a pure-Rust/WASM promise. CI covers Windows, Linux and macOS; consult
actual repository CI results for exercised targets.

## Offline quick start

```rust
use hadron_core::{Manifest, Pack, diff, validation::{self, Limits}};
use std::collections::BTreeMap;
let installed = Manifest {
    pack: Pack {
        schema_version: 1, id: "friends".into(), name: "Friends".into(),
        description: "Our pack".into(), game: "minecraft".into(),
        game_version: "1.21.1".into(), loader: "fabric".into(),
        loader_version: "0.16.10".into(),
    },
    mods: BTreeMap::new(), configs: BTreeMap::new(),
};
let limits = Limits::default();
validation::manifest(&installed, &limits)?;
let bytes = serde_json::to_vec(&installed)?;
let target: Manifest = validation::json(&bytes, &limits)?;
assert!(diff::between(&installed, &target).mods.is_empty());
# Ok::<(), hadron_core::Error>(())
```

[`validation::json`] bounds/deserializes input; call [`validation::manifest`] for
semantic checks. Workflows perform both. The public models support Serde so a
launcher can bridge results to any frontend. Large `u64` values should be encoded
as decimal strings by a JavaScript bridge rather than losing integer precision.

## Sharing, authentication and client setup

[`share::encode`] and [`share::decode`] wrap `neutrino-obscura`: eight Base32 symbols,
usually `XXXX-XXXX`, including a checksum. They reversibly identify a 35-bit GitHub
repository ID with no lookup registry. A code identifies a repository; it grants
no permission. Private repositories still require authorized GitHub access.
Checksum/passphrase scrambling is not an access-control mechanism.

One repository contains one pack. A share code selects its stable publication;
persist a [`Latest`] pointer to pin a historical version. Numeric IDs can survive
repository path changes, but availability/authorization still depend on GitHub.

```rust,no_run
# #[cfg(feature = "github")]
async fn load_pack(token: String, code: &str) -> Result<(), Box<dyn std::error::Error>> {
    use hadron_core::{github::{Github, Options, Context}, workflow::Hadron};
    let octocrab = octocrab::Octocrab::builder().personal_token(token).build()?;
    let hadron = Hadron::new(Github::new(octocrab, Options::default())?);
    let loaded = hadron.open(code, &Context::default()).await?;
    println!("{}: {} mods", loaded.manifest.pack.name, loaded.manifest.mods.len());
    Ok(())
}
```

Public reads can be anonymous, within GitHub rate limits. Private reads require
repository contents access. Submission requires a writable branch or permitted
fork plus PR creation rights. Owner workflows require contents and PR write rights.
Fresh push/maintain/admin metadata is checked, but GitHub enforces actual token
permissions and branch protection. Push-capable maintainers can accept requests;
configure review rules if only named owners may approve. JSON authors cannot grant
permissions. Missing private resources may appear as 404.

**Disable Octocrab redirect and retry middleware**, including Cargo features
enabled elsewhere or custom service layers. Hadron's dependency disables both.
Hadron follows asset redirects with a separate unauthenticated client and never
automatically retries a mutation. A custom client that auto-follows redirects or
replays writes can violate these guarantees. Hadron does not copy tokens into
download URLs or retrieve them for redirected requests.

## Consumer operations and cache

With `cache`, attach a caller-owned `cache::Cache` through `Hadron::with_cache`.
The directory must be trusted against concurrent filesystem replacement. Private
artifact bytes remain on disk; they are not automatically encrypted or evicted.

`Hadron::open` resolves repository identity, fetches the stable pointer, verifies the
index, downloads uncached artifacts, verifies compressed and decoded hashes,
validates the manifest and checks each config against its inventory. It returns
config bytes in memory without extracting paths into an instance. Tachyon must
check local symlinks and resolve user edits before writing files.

Repeated `Github::latest` calls use ETag conditions in the same client. A fresh
process re-reads discovery metadata. A warm disk cache avoids artifact bytes, not
all API traffic. Reads revalidate hash/size; corruption is reported. Writes verify
content and atomically promote temporary files without clobbering another writer.
No automatic eviction deletes files needed by older publications.

Strict cache errors remain default. Opt into `cache::CachePolicy::Recover` with
`Hadron::with_cache_policy` to remove a corrupt content key and refetch, or continue
after local cache I/O failure. Remote content must still pass all integrity checks.
`Stage::CacheSkipped` reports the local fallback. `Cache::size` counts regular
hash-named entries; `Cache::prune(maximum)` explicitly removes entries in hash order
until that byte budget is reached, preserving foreign/temporary files. Coordinate
eviction with other users of this trusted directory; this is not a cross-process
quota. `Cache::remove` deletes only one caller-selected content key.

Pointer metadata is bounded to 128 repository entries per adapter. Eviction is
deterministic by repository ID, not LRU. `Github::clear_metadata(Some(id))` invalidates
one entry; `None` clears all entries.

[`diff::between`] produces an [`diff::UpdatePlan`] with old/new pack metadata,
mods and configs. It supports update and rollback previews. Provider references
remain exact download specifications for Tachyon. Hadron validates structure;
it does not prove that a combination of mods will run together.

## Editable schema

```text
pack.json / mods.json / requests.json / config/<files>
.hadron/latest.json
```

Typed records reject unknown fields and unsupported schema numbers. Version 1 has
no implicit migration. Portable paths are ASCII, relative, slash-separated, and
reject traversal, drive/UNC paths, Windows devices, trailing dots/spaces, case
collisions and file/directory aliases. Mods live under `mods/`; configs under
`config/`. Stable keys use ASCII alphanumeric characters, hyphens and underscores.

`pack.json` is a [`Pack`]:

```json
{
  "schema_version": 1,
  "id": "friends",
  "name": "Friends",
  "description": "Our reviewed pack",
  "game": "minecraft",
  "game_version": "1.21.1",
  "loader": "fabric",
  "loader_version": "0.16.10"
}
```

`mods.json` is a [`ModList`]. The following digest and version are illustrative,
not real downloadable Sodium metadata:

```json
{
  "mods": {
    "sodium": {
      "name": "Sodium",
      "file": {
        "path": "mods/sodium.jar",
        "size": 123456,
        "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
      },
      "source": { "kind": "modrinth", "project": "AANobbMI", "version": "EXACT_VERSION_ID" },
      "client": "required",
      "server": "unsupported"
    }
  }
}
```

[`Source`] variants: `modrinth` with `project`/`version`; `curseforge` with decimal
string `project`/`file`; `direct` with a durable HTTPS `url` without credentials,
query or fragment. Every file requires exact byte size and SHA-256. SHA-1-only
provider metadata must be resolved/downloaded and strongly hashed by the caller
before publication. There is no implicit `latest` resolution. Client/server values
are `required`, `optional` or `unsupported`, with at least one supported side.
Empty config files are supported; empty mod binaries are rejected.

Published [`Manifest`] combines `pack`, direct `mods` map and `configs` mapping
paths to [`FileSpec`]. Preparation derives config inventory from exact byte content;
comments, encoding and whitespace are preserved.

## Requests, owner review and state

Initial `requests.json` is a [`Requests`] ledger:

```json
{ "schema_version": 1, "requests": {}, "applied": {} }
```

The `requests` map stores complete [`Request`] records by unique ID. Each has `id`,
40-digit `base_commit`, nullable `base_publication`, `reason`, and [`Operation`].
Operation shapes are `{"kind":"add","key":...,"entry":...}`,
`{"kind":"remove","key":...,"expected":...}`, or
`{"kind":"update","key":...,"expected":...,"entry":...}`. Entries/expected
values are complete [`ModEntry`] records. An existing mod must equal `expected`
before an operation applies. Dependencies must be proposed explicitly.

`Hadron::submit` creates/reuses `hadron-request-<id>` and opens a PR changing only
`requests.json`. The request starts from the reviewed current source head. A
non-writer uses a verified fork; fork provisioning is asynchronous and may require
another explicit attempt. An existing unrelated repository with the same name
is rejected. Private repository fork policy remains enforced.

`Hadron::inspect` checks target repo/branch and complete immutable Git trees at the
captured head and its merge base with the captured target SHA. It does not depend
on GitHub's moving PR file listing or truncated comparison file arrays. Truncated
Git trees fail closed. The claimed request base must be an ancestor of the comparison
base. Only one added request is permitted, with existing records/applied ledger
unchanged. It returns a request and head SHA. Owner reviews these, then passes that
SHA to `accept`. GitHub's merge SHA precondition rejects intervening PR edits.
`proposals` returns typed open/closed/merged state. Merged status is distinct from
the ledger's applied marker and the selected publication's source commit.

```text
proposed -> merged -> applied source -> prepared -> published
    |         |            |                        |
 rejected   failure     conflict                publish failure
```

`accept` merges and reconciles; `reconcile` can also handle a merge made through
GitHub's UI. It rereads current source and atomically commits modlist changes plus
`applied[request_id] = PR_NUMBER`. The containing source commit identifies the
application; a commit cannot contain its own final hash. Replaying the same ID
does not apply a change twice. `reject` closes an unmerged PR; an applied change
needs a new proposal to reverse it. `base_publication` is context, while the exact
base commit and expected entry govern edits.

Merged recovery reads the immutable `merge_commit_sha` and first-parent tree in
the target repository, so deleted forks and post-merge head edits do not prevent
reconciliation. Merge and squash results are supported. For a multi-commit rebase,
the final commit must contain the complete request delta; otherwise inspection
fails closed. Hadron itself creates one request commit and merges with a merge
commit. GitHub documents these result SHA semantics in its
[pull request API](https://docs.github.com/en/rest/pulls/pulls#get-a-pull-request).
Applied markers short-circuit repeated reconciliation after confirming the PR is
merged into the expected target. The commit containing the marker is the source
of application provenance.

Submission retries query PRs for the caller's specific head owner/branch and target
branch. Unrelated PR history no longer counts toward that lookup budget. Use
`Hadron::proposals_page(repo, page, ctx)` to browse 100 PRs at a time; the returned
boolean indicates whether another raw page may exist, even when Hadron filtering
returns no proposals. The original `proposals` remains a bounded whole-list helper.

For long-running packs, `Hadron::archive_requests(repo, ids, expected_head, ctx)`
atomically moves selected applied records out of `requests.json` and into
`.hadron/requests/<id>.json`. Each file is a schema-1 `Requests` record with exactly
one request and one applied PR number. IDs remain reserved; retries and reconciliation
consult these records. Pending requests cannot be archived. Archive only after all
writers upgrade to 0.0.2: 0.0.1 does not consult archives. History still consumes Git
storage and counts toward the complete-tree budget; adjust limits deliberately for
very large repositories. There is no automatic history deletion.

Owner approval does not execute local work automatically. An explicit local publish
operation runs next. Concurrent proposals editing the shared file can conflict;
preserve/review both requests rather than regenerating a stale whole ledger. PR
identity and GitHub permissions are authoritative, not an author name in JSON.

Typed request creation (the caller supplies a unique persistent ID and exact mod):

```rust,no_run
# #[cfg(feature = "github")]
async fn request_mod(
    h: &hadron_core::workflow::Hadron,
    repo: &hadron_core::github::Repository,
    id: String,
    entry: hadron_core::ModEntry,
) -> hadron_core::Result<hadron_core::workflow::Proposal> {
    use hadron_core::{Request, Operation, github::Context};
    let ctx = Context::default();
    let head = h.github.head(repo, &ctx).await?;
    let latest = h.github.latest(repo, &ctx).await?;
    h.submit(repo, Request {
        id, base_commit: head,
        base_publication: latest.map(|v| v.publication_id),
        reason: "Please add this tested Sodium version".into(),
        operation: Operation::Add { key: "sodium".into(), entry },
    }, &ctx).await
}
```

Proposal validation reads pack/mod JSON only, without fetching or recompressing
configs. Complete configuration validation happens during publication preparation.

## Source preparation and publication

`Hadron::prepare` fetches `pack.json`, `mods.json` and configs at an exact Git commit.
It requires a complete recursive Git tree and rejects symlinks/submodules inside
configs. Config files are fetched with bounded concurrency after prechecking total
source bytes. The
workflow builds on blocking workers. With `compression`, `codec::prepare` provides
offline construction from an explicit byte map, whose provenance is caller-owned.

`Hadron::publish(repo, id, source_commit, expected_head, context)`:

1. Checks fresh maintainer permissions and exact identifiers.
2. Creates/reuses draft release `hadron-<id>` and prepares exact source locally.
3. Uploads manifest/config assets and verifies uploaded bytes.
4. Reuses verified prior configs when immutable config Git entries match. A mod-only
   change skips config source downloads and tar compression. A partial config edit
   fetches only changed source files after verifying the old archive.
5. Uploads raw `publication.json` and verifies the complete consumer path.
6. Makes the complete release readable.
7. Advances `.hadron/latest.json` through a fast-forward-only commit based on
   the owner's reviewed `expected_head`.

Retry an interrupted operation using the **same publication ID and source SHA**.
Existing assets are checked rather than overwritten. A conflicting ID is an error.
Once `publication.json` exists, retries validate that index and its complete content
against source and retain its exact asset IDs, independently of the currently selected
release. A missing or corrupt optional previous release instead causes fresh artifacts
and `Stage::ReuseSkipped`; cancellation, permission and transport failures propagate.
Old config assets are checked remotely before reuse, even with a warm local cache,
because their continued remote availability matters to new consumers.
Do not delete older assets/releases: newer publications can reference unchanged
config artifacts in them. The API supports GitHub.com upload origins, not arbitrary
GitHub Enterprise asset hosts in this release.

GitHub cannot transact every endpoint together. A failed upload leaves stable on
the previous complete version. A failed pointer update may leave a complete but
unselected release. Inspect the new branch head, review changes, and resume with
the same ID. Mutations are never automatically replayed after ambiguous network
timeouts. A resumed operation recognizes an already-selected exact pointer.

Upload origin/path is restricted to the expected GitHub release. Asset redirects
use HTTPS and enumerated GitHub hosts with a four-request redirect bound. The
redirect client has no API auth. Pack JSON cannot choose an authenticated API host.
Source is treated as data: no script, build hook or command executes from a PR.

An owner operation after the UI has reviewed the exact head SHA:

```rust,no_run
# #[cfg(feature = "github")]
async fn publish_reviewed(
    h: &hadron_core::workflow::Hadron,
    repo: &hadron_core::github::Repository,
    pr_number: u64,
    reviewed_head: &str,
    publication_id: &str,
) -> hadron_core::Result<hadron_core::workflow::Published> {
    let ctx = hadron_core::github::Context::default();
    let source = h.accept(repo, pr_number, reviewed_head, &ctx).await?;
    h.publish(repo, publication_id, &source, &source, &ctx).await
}
```

Persist the returned `Published.latest` for a hash-pinned rollback. If publication
fails after acceptance, resume with the applied source SHA and same publication ID;
do not manufacture a second request. Branch-protected repos may require owner
configuration or another review before source/pointer commits can be pushed.

## Published schema, format and hashes

```text
hadron-<id> release:
  manifest-<compressed-sha256>.json.zst
  configs-<compressed-sha256>.tar.zst  # optional or reused by asset ID
  publication.json
```

[`Publication`] contains `schema_version`, `repository_id`, `pack_id`, `id`,
`source_commit`, `manifest` and nullable `configs`. [`Latest`] contains
`schema_version`, `publication_id`, `index`. Each [`Asset`] is:

```json
{
  "id": 123,
  "name": "manifest-example.json.zst",
  "size": 100,
  "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "decoded_size": 1000,
  "decoded_sha256": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
}
```

This example is structural, with illustrative values. Asset names are never local
paths. Compressed hashes cover entire uploaded frames; decoded hashes cover exact
JSON/tar bytes, including tar padding. Config inventory hashes cover file bytes.
The raw index's decoded and encoded sizes/hashes must agree. Hashes are canonical
lowercase SHA-256. Repository identity and pack identity are checked on reads.

JSON field order and map ordering are stable. Tar files are sorted, regular entries
with mode 0644 and zero UID/GID/mtime. The publisher uses ZSTD-9; the reader accepts
supported frames independently of compression level. Determinism applies to fixed
source, settings and encoder version; upgrades may change compressed bytes without
changing decoded meaning. No cross-version bitstream identity is promised.

## Compression and performance

User measurements: 4,725,479 raw bytes compressed to 596,370 with ZSTD-9, versus
532,300 with LZMA-6/9. Separate ZSTD-9 manifest/config outputs totaled 597,866,
only 1,496 above combined. This motivated speed/size tradeoff and separate downloads.
These are supplied results, not independently reproduced throughput guarantees.
Unchanged config bundles cost no artifact-download bytes in mod-only updates.
Mod JARs are separate downloads; metadata compression does not shrink them.

Default [`validation::Limits`]: JSON/source JSON/decoded manifest 8 MiB; compressed artifact 64 MiB;
decoded artifact 256 MiB; individual config 16 MiB; 10,000 entries; zstd maximum
window log 27 (128 MiB). Config source reads use the config budget, independently of
the JSON budget. Git blob outbound envelopes allow base64 expansion plus 64 bytes
overhead without raising inbound JSON limits. Manifest preparation enforces the same
JSON limit as loading. Tar overhead counts toward decoded artifact bytes.
Streaming removes complete uncompressed tar buffers; returned configs, compressed
bytes, JSON and codec windows still coexist. These limits are not a process RAM quota.

`github::Options` defaults: four network/CPU operations per client, 60 seconds per
HTTP attempt including body reading, two extra transient GET attempts, and one heavy
workflow (`Options::workflows`, allowed range 1..=64). Admission happens before heavy
fetches for load/prepare/publish and is cancellable. Independent adapters have independent
budgets. A cancelled blocking task may finish after its workflow returns, but keeps
its CPU permit. Numeric
Retry-After and exhausted rate reset headers are honored; waits above 30 seconds
return to caller. Each request/attempt has a deadline; workflows have no implicit
overall deadline. Owned models favor integration clarity over zero-copy complexity.
Use caching and unchanged artifacts before optimizing small serialization costs.

`github::Context` supports cancellation and stage events. A callback must be fast,
nonblocking and nonpanicking. Cancelling cannot undo remote writes already accepted.
Running blocking codec tasks finish under their held semaphore permit; input limits
bound residual work. Dictionaries, fine-grained chunks and binary delta patches are
deferred pending actual workload measurements.

### Reproducible synthetic smoke measurement

Run `cargo run --release --no-default-features --features compression --example measure`.
This is deliberately synthetic, not evidence about real modpacks. A local Windows
x86-64 run on Intel Core i7-13620H, Rust 1.97.1, zstd crate 0.13.4 / native 1.5.7,
one encoder thread, 500 mod entries and 100 repetitive TOML configs reported for
0.0.2 (single local smoke run, not a controlled before/after comparison):

| Measurement | Observed |
| --- | ---: |
| Manifest raw / compressed | 154,717 / 27,641 bytes |
| Config tar raw / compressed | 359,424 / 775 bytes |
| Full prepare (single sample) | 8,798 microseconds |
| Manifest decode (100-loop mean) | 389 microseconds |
| Parse + validate + diff (100-loop mean) | 1,050 microseconds |
| Cold artifact bytes / warm artifact bytes | 28,416 / 0 |
| One mod edit artifact bytes | 27,650 |
| One config edit artifact bytes | 28,424 |
| Host-sampled process peak working set | 17,960,960 bytes |

Artifact totals exclude discovery/index traffic and mod binaries. These timings
do not measure a hard memory bound; host memory sampling includes the complete example
and its retained snapshots. Timings vary with load and are not throughput guarantees. Real packs should
be measured before changing codec/bundle granularity.

Local mocked workflow regressions separately assert zero config source requests and
zero config uploads for unchanged bundles, and one config source request for a
one-file edit among two configs. These are deterministic request-count checks;
loopback timing is not a claim about live GitHub throughput.

## Rollback, trust and recovery

Load a saved [`Latest`] for hash-pinned historical retrieval. `Hadron::historical`
discovers an old tag and requires GitHub's SHA-256 asset digest for its index. If
digest metadata is unavailable, use a previously saved pointer rather than skipping
integrity checks. Compare with [`diff::between`] to preview local rollback.
`Hadron::select` verifies a previous pointer and guardedly advances stable. It does
not rewrite source, Git history, worlds or saves; subsequent source publication is
another owner decision. Missing upstream mods need a retained cache/permitted mirror.

A hash establishes equality with selected metadata, not that a publisher is benign.
GitHub permissions/control establish publisher trust. Release immutability is an
application policy unless enforced independently on GitHub; cached and downloaded
content is still verified. Untrusted filesystem writers must not replace cache
parents. Tachyon must avoid links when writing configs and review user-file conflicts.

Common errors:

- [`Error::Schema`]: unsupported schema; upgrade reader/export supported data.
- [`Error::NotFound`]: missing **or inaccessible** resource; inspect membership,
  token permissions, source layout and selected release.
- [`Error::Permission`]: token/repository/fork policy denied action.
- [`Error::Conflict`]: stale head/entry or conflicting remote data; reread and review.
- [`Error::Integrity`] / [`Error::Codec`]: reject corrupted/replaced/invalid content.
- [`Error::Limit`]: inspect expected size before explicitly changing budgets.
- [`Error::Api`]: HTTP status and retry information without remote response body.
- [`Error::Network`] / [`Error::Cancelled`]: reconcile mutation outcome before retry.
- [`Error::Io`]: trusted cache access, disk capacity or filesystem failure.

Transport errors omit URLs/response bodies to avoid leaking credentials or signed
URLs. Launcher callbacks/logs must also avoid tokens and passphrases. Older mods may
reject newer saves, so save restoration remains a separate user-controlled operation.

## Compatibility and examples

Wire schema and crate semver evolve independently. Initial unknown fields/versions
fail rather than being silently discarded. Future migrations must document retained
historical read behavior. Pin 0.0.x APIs while adopting them. No automatic OAuth UI,
provider dependency solver, game installer, compression trainer or Enterprise upload
support is promised.

Run the packaged `examples/offline.rs` for an artifact round trip. Repository tests
exercise a mocked requester/owner/consumer lifecycle with upload failure, pointer
conflict, config reuse, cache hits, replay reconciliation and historical selection.
Default tests never need credentials or modify a real pack.
