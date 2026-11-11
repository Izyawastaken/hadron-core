# hadron-core

Typed GitHub-backed modpack publishing and collaboration for Tachyon and other
launchers. Version 0.0.2, MIT licensed, Rust 1.97+.

Hadron translates editable JSON into frontend-ready models and verified ZSTD-9
artifacts. Requests become PRs; the owner reviews, merges and explicitly publishes
from their machine. Neutrino codes identify repositories; GitHub controls access.
Manifests and configs ship separately, reusing unchanged config artifacts.

```toml
[dependencies]
hadron-core = "=0.0.2"
```

Use `default-features = false` for models/validation/diffs alone. Defaults include
GitHub, compression and disk cache. Supply an Octocrab client, Tokio runtime and
provider metadata. Disable Octocrab automatic redirect/retry middleware; Hadron
follows asset redirects without credentials and does not automatically replay writes.

Read the [full guide](docs/guide.md), [API docs](https://docs.rs/hadron-core), or
run `cargo run --example offline`. See [changelog](CHANGELOG.md).

```text
pack.json / mods.json / requests.json / config/
.hadron/latest.json -> release publication.json -> manifest/config .zst assets
```

Hadron does not install games, solve every mod dependency, execute pack scripts or
restore saves. The launcher applies file changes and resolves user config conflicts.
Native compression/TLS dependencies require platform build tools. Consult CI for
actual tested targets.
