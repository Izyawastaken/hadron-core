//! Owner-controlled requests and resumable publication workflows.
//!
//! Every mutating method is explicit. No method watches GitHub in the background,
//! installs mods, executes pack scripts, or changes saves/worlds.
use crate::{
    Error, Result,
    codec::{self, Prepared},
    github::{self, Context, Github, Repository, Stage},
    model::*,
    validation,
};
use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Fully verified consumer data. Configs are bytes, not extracted filesystem paths.
#[derive(Clone, Debug)]
pub struct Loaded {
    /// Exact immutable publication selected.
    pub publication: Publication,
    /// Frontend-ready typed manifest.
    pub manifest: Manifest,
    /// Verified configs keyed by instance-relative destination.
    pub configs: BTreeMap<String, Vec<u8>>,
}
/// Request PR identity returned to the launcher.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    /// GitHub PR number.
    pub number: u64,
    /// Browser URL, returned by GitHub.
    pub url: String,
    /// Request ID used to reconcile retries.
    pub request_id: String,
}
/// A prepared source snapshot. Construction fetches its exact Git commit.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Exact Git source commit.
    pub commit: String,
    /// Deterministically built local artifacts.
    pub prepared: Prepared,
}
/// Published result. The commit advancing the pointer may differ from source commit.
#[derive(Clone, Debug)]
pub struct Published {
    /// Complete immutable index.
    pub publication: Publication,
    /// Consumer discovery pointer.
    pub latest: Latest,
    /// Commit advancing the default branch.
    pub pointer_commit: String,
}
/// High-level workflows sharing one bounded GitHub adapter.
#[derive(Clone)]
pub struct Hadron {
    /// Reusable GitHub transport and options.
    pub github: Github,
    #[cfg(feature = "cache")]
    cache: Option<crate::cache::Cache>,
}
impl Hadron {
    /// Create without persistent artifact cache.
    pub fn new(github: Github) -> Self {
        Self {
            github,
            #[cfg(feature = "cache")]
            cache: None,
        }
    }
    /// Enable caller-owned disk cache. It is safe to reuse across repositories:
    /// keys are exact content hashes, but private artifact bytes remain on disk.
    #[cfg(feature = "cache")]
    pub fn with_cache(mut self, cache: crate::cache::Cache) -> Self {
        self.cache = Some(cache);
        self
    }
    async fn blocking<T: Send + 'static>(
        &self,
        ctx: &Context,
        f: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self.github.cpu.clone().acquire_owned();
        let permit = tokio::select! { _=ctx.cancellation.cancelled()=>return Err(Error::Cancelled),p=permit=>p.map_err(|_|Error::Cancelled)? };
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f()
        });
        // A running blocking codec cannot be interrupted. Limits bound its work and
        // its permit stays held until it exits, even if this future is cancelled.
        tokio::select! { _=ctx.cancellation.cancelled()=>Err(Error::Cancelled),v=task=>v.map_err(|_|Error::Codec)? }
    }
    async fn bytes(&self, repo: &Repository, asset: &Asset, ctx: &Context) -> Result<Vec<u8>> {
        #[cfg(feature = "cache")]
        if let Some(cache) = self.cache.clone() {
            let asset = asset.clone();
            let max = self.github.options.limits.compressed_bytes;
            if let Some(bytes) = self
                .blocking(ctx, move || cache.get(&asset.sha256, asset.size, max))
                .await?
            {
                return Ok(bytes);
            }
        }
        let bytes = self.github.asset(repo, asset, ctx).await?;
        #[cfg(feature = "cache")]
        if let Some(cache) = self.cache.clone() {
            let hash = asset.sha256.clone();
            let copy = bytes.clone();
            self.blocking(ctx, move || cache.put(&hash, &copy)).await?;
        }
        Ok(bytes)
    }
    /// Load current publication, returning a clear error for a non-Hadron repository.
    pub async fn open(&self, code: &str, ctx: &Context) -> Result<Loaded> {
        let repo = self.github.resolve(code, ctx).await?;
        let latest = self
            .github
            .latest(&repo, ctx)
            .await?
            .ok_or(Error::NotFound)?;
        self.load(&repo, &latest, ctx).await
    }
    /// Load an index selected by a current or historical verified pointer.
    pub async fn load(&self, repo: &Repository, latest: &Latest, ctx: &Context) -> Result<Loaded> {
        validation::schema(latest.schema_version)?;
        let index = self.bytes(repo, &latest.index, ctx).await?;
        if latest.index.decoded_size != latest.index.size
            || latest.index.decoded_sha256 != latest.index.sha256
        {
            return Err(Error::Integrity);
        }
        let publication: Publication = validation::json(&index, &self.github.options.limits)?;
        if publication.id != latest.publication_id {
            return Err(Error::Integrity);
        }
        validation::publication(&publication, repo.id, &self.github.options.limits)?;
        let bytes = self.bytes(repo, &publication.manifest, ctx).await?;
        let asset = publication.manifest.clone();
        let limits = self.github.options.limits.clone();
        let manifest: Manifest = self
            .blocking(ctx, move || {
                validation::json(&codec::decode(&bytes, &asset, &limits)?, &limits)
            })
            .await?;
        validation::manifest(&manifest, &self.github.options.limits)?;
        if manifest.pack.id != publication.pack_id {
            return Err(Error::Integrity);
        }
        let configs = match &publication.configs {
            Some(asset) => {
                let bytes = self.bytes(repo, asset, ctx).await?;
                let a = asset.clone();
                let m = manifest.clone();
                let l = self.github.options.limits.clone();
                self.blocking(ctx, move || codec::configs(&bytes, &a, &m, &l))
                    .await?
            }
            None if manifest.configs.is_empty() => BTreeMap::new(),
            None => return Err(Error::Integrity),
        };
        Ok(Loaded {
            publication,
            manifest,
            configs,
        })
    }
    /// Discover and load a previous release by publication ID. Historical asset hash
    /// is learned from the authenticated release response/download; persist a Latest
    /// value locally when stronger historical pinning is required.
    pub async fn historical(&self, repo: &Repository, id: &str, ctx: &Context) -> Result<Loaded> {
        validation::key(id)?;
        let release: Value = self
            .github
            .get(&format!("{}/releases/tags/hadron-{id}", repo.route()), ctx)
            .await?;
        let assets = self
            .github
            .pages(
                &format!(
                    "{}/releases/{}/assets",
                    repo.route(),
                    github::number(&release, "id")?
                ),
                ctx,
            )
            .await?;
        let a = assets
            .iter()
            .find(|a| a["name"] == "publication.json")
            .ok_or(Error::NotFound)?;
        let size = github::number(a, "size")?;
        let digest = github::str_field(a, "digest")?
            .strip_prefix("sha256:")
            .ok_or(Error::Integrity)?;
        let hash = Hash::parse(digest)?;
        let asset = Asset {
            id: github::number(a, "id")?,
            name: "publication.json".into(),
            size,
            sha256: hash.clone(),
            decoded_size: size,
            decoded_sha256: hash,
        };
        self.load(
            repo,
            &Latest {
                schema_version: 1,
                publication_id: id.into(),
                index: asset,
            },
            ctx,
        )
        .await
    }
    /// Read authoritative source at an exact SHA and build locally, off async workers.
    /// Enumerates the `config/` directory from a complete recursive Git tree.
    pub async fn prepare(
        &self,
        repo: &Repository,
        commit: &str,
        ctx: &Context,
    ) -> Result<Snapshot> {
        validation::commit(commit)?;
        let pack: Pack = validation::json(
            &self.github.file(repo, "pack.json", commit, ctx).await?,
            &self.github.options.limits,
        )?;
        let mods: ModList = validation::json(
            &self.github.file(repo, "mods.json", commit, ctx).await?,
            &self.github.options.limits,
        )?;
        let commit_data: Value = self
            .github
            .get(&format!("{}/git/commits/{commit}", repo.route()), ctx)
            .await?;
        let tree: Value = self
            .github
            .get(
                &format!(
                    "{}/git/trees/{}?recursive=1",
                    repo.route(),
                    github::str_field(&commit_data["tree"], "sha")?
                ),
                ctx,
            )
            .await?;
        if tree["truncated"].as_bool() != Some(false) {
            return Err(Error::Limit("truncated Git tree"));
        }
        let entries = tree["tree"].as_array().ok_or(Error::Integrity)?;
        if entries.len() > self.github.options.limits.entries {
            return Err(Error::Limit("Git tree entries"));
        }
        let mut configs = BTreeMap::new();
        let mut total = 0u64;
        for entry in entries {
            let path = github::str_field(entry, "path")?;
            if !path.starts_with("config/") {
                continue;
            }
            validation::path(path)?;
            if entry["type"] == "tree" {
                continue;
            }
            if entry["type"] != "blob"
                || !matches!(entry["mode"].as_str(), Some("100644" | "100755"))
            {
                return Err(Error::Invalid("config must be a regular Git file".into()));
            }
            let size = github::number(entry, "size")?;
            if size > self.github.options.limits.file_bytes {
                return Err(Error::Limit("config file"));
            }
            total = total.checked_add(size).ok_or(Error::Limit("configs"))?;
            if total > self.github.options.limits.decoded_bytes {
                return Err(Error::Limit("configs"));
            }
            let bytes = self.github.file(repo, path, commit, ctx).await?;
            if bytes.len() as u64 != size {
                return Err(Error::Integrity);
            }
            configs.insert(path.into(), bytes);
        }
        ctx.emit(Stage::Preparing)?;
        let limits = self.github.options.limits.clone();
        let prepared = self
            .blocking(ctx, move || {
                codec::prepare(
                    Manifest {
                        pack,
                        mods: mods.mods,
                        configs: BTreeMap::new(),
                    },
                    &configs,
                    &limits,
                )
            })
            .await?;
        Ok(Snapshot {
            commit: commit.into(),
            prepared,
        })
    }
    /// List request PRs (including closed/merged) identified by Hadron's branch prefix.
    pub async fn proposals(&self, repo: &Repository, ctx: &Context) -> Result<Vec<Proposal>> {
        let prs = self
            .github
            .pages(&format!("{}/pulls?state=all", repo.route()), ctx)
            .await?;
        prs.iter()
            .filter_map(|p| {
                p["head"]["ref"]
                    .as_str()
                    .and_then(|r| r.strip_prefix("hadron-request-"))
                    .map(|id| (p, id))
            })
            .map(|(p, id)| proposal(p, id))
            .collect()
    }
    /// Open an exact request PR. On a retry, returns the existing PR for its unique
    /// branch. Fork creation may be asynchronous: retry after NotFound/Conflict.
    pub async fn submit(
        &self,
        repo: &Repository,
        request: Request,
        ctx: &Context,
    ) -> Result<Proposal> {
        validation::key(&request.id)?;
        validation::commit(&request.base_commit)?;
        let branch = format!("hadron-request-{}", request.id);
        let existing = self.proposals(repo, ctx).await?;
        if let Some(p) = existing.into_iter().find(|p| p.request_id == request.id) {
            if self.inspect(repo, p.number, ctx).await?.0 != request {
                return Err(Error::Conflict(
                    "request ID belongs to different proposal".into(),
                ));
            }
            return Ok(p);
        }
        let current = self.github.head(repo, ctx).await?;
        if current != request.base_commit {
            return Err(Error::Conflict("request base no longer current".into()));
        }
        let mut ledger = self.github.ledger(repo, &current, ctx).await?;
        if ledger.requests.contains_key(&request.id) {
            return Err(Error::Conflict("request ID already recorded".into()));
        }
        let mut candidate = self.source_manifest(repo, &current, ctx).await?;
        let changed = request.operation.apply(&ModList {
            mods: candidate.mods.clone(),
        })?;
        candidate.mods = changed.mods;
        validation::manifest(&candidate, &self.github.options.limits)?;
        ledger.requests.insert(request.id.clone(), request.clone());
        validation::requests(&ledger, &self.github.options.limits)?;
        let writable = if repo.permissions.push {
            repo.clone()
        } else {
            let user: Value = self.github.get("/user", ctx).await?;
            let login = github::str_field(&user, "login")?;
            let name = repo.full_name.split('/').nth(1).ok_or(Error::Integrity)?;
            let path = format!(
                "/repos/{}/{}",
                github::segment(login),
                github::segment(name)
            );
            let fork: Value = match self.github.get(&path, ctx).await {
                Ok(v) => v,
                Err(Error::NotFound) => {
                    self.github
                        .json_call(
                            Method::POST,
                            &format!("{}/forks", repo.route()),
                            Some(&json!({"default_branch_only":true})),
                            ctx,
                        )
                        .await?
                }
                Err(e) => return Err(e),
            };
            if fork["parent"]["id"].as_u64() != Some(repo.id)
                && fork["source"]["id"].as_u64() != Some(repo.id)
            {
                return Err(Error::Conflict(
                    "existing repository is not a fork of target".into(),
                ));
            }
            serde_json::from_value::<Repository>(fork)?
        };
        let branch_route = format!(
            "{}/git/ref/heads/{}",
            writable.route(),
            github::segment(&branch)
        );
        let branch_head: Value = match self.github.get(&branch_route, ctx).await {
            Ok(v) => v,
            Err(Error::NotFound) => {
                self.github
                    .json_call(
                        Method::POST,
                        &format!("{}/git/refs", writable.route()),
                        Some(&json!({"ref":format!("refs/heads/{branch}"),"sha":current})),
                        ctx,
                    )
                    .await?
            }
            Err(e) => return Err(e),
        };
        let branch_sha = github::str_field(&branch_head["object"], "sha")?;
        if branch_sha == current {
            self.github
                .commit_files(
                    &writable,
                    &branch,
                    &current,
                    &BTreeMap::from([(
                        "requests.json".into(),
                        serde_json::to_vec_pretty(&ledger)?,
                    )]),
                    &format!("Request {}", request.id),
                    ctx,
                )
                .await?;
        } else {
            let existing = self.github.ledger(&writable, branch_sha, ctx).await?;
            if existing != ledger {
                return Err(Error::Conflict(
                    "request branch already contains different data".into(),
                ));
            }
        }
        let owner = writable
            .full_name
            .split('/')
            .next()
            .ok_or(Error::Integrity)?;
        let pr:Value=self.github.json_call(Method::POST,&format!("{}/pulls",repo.route()),Some(&json!({"title":format!("Mod request: {}",request.id),"body":request.reason,"head":format!("{owner}:{branch}"),"base":repo.default_branch})),ctx).await?;
        proposal(&pr, &request.id)
    }
    /// Inspect a PR's exact request delta and reviewed SHA. Rejects unrelated edits,
    /// changed/deleted ledger records, and multiple request additions.
    pub async fn inspect(
        &self,
        repo: &Repository,
        number: u64,
        ctx: &Context,
    ) -> Result<(Request, String)> {
        let pr: Value = self
            .github
            .get(&format!("{}/pulls/{number}", repo.route()), ctx)
            .await?;
        if pr["base"]["repo"]["id"].as_u64() != Some(repo.id)
            || pr["base"]["ref"].as_str() != Some(&repo.default_branch)
        {
            return Err(Error::Invalid("PR targets another pack or branch".into()));
        }
        let files = self
            .github
            .pages(&format!("{}/pulls/{number}/files", repo.route()), ctx)
            .await?;
        if files.len() != 1
            || files[0]["filename"] != "requests.json"
            || !matches!(files[0]["status"].as_str(), Some("added" | "modified"))
        {
            return Err(Error::Invalid(
                "request PR must change only requests.json".into(),
            ));
        }
        let base_sha = github::str_field(&pr["base"], "sha")?;
        let head_sha = github::str_field(&pr["head"], "sha")?.to_owned();
        let head_repo = self
            .github
            .repository(github::number(&pr["head"]["repo"], "id")?, ctx)
            .await?;
        let base = self.github.ledger(repo, base_sha, ctx).await?;
        let head = self.github.ledger(&head_repo, &head_sha, ctx).await?;
        let request = request_delta(&base, &head)?;
        if pr["head"]["ref"].as_str() != Some(&format!("hadron-request-{}", request.id)) {
            return Err(Error::Invalid("request branch ID mismatch".into()));
        }
        Ok((request, head_sha))
    }
    /// Merge only the SHA reviewed by the owner, then reconcile source exactly once.
    /// Permission is checked fresh; branch protection may still reject the merge.
    pub async fn accept(
        &self,
        repo: &Repository,
        number: u64,
        reviewed_sha: &str,
        ctx: &Context,
    ) -> Result<String> {
        owner(&self.github.repository(repo.id, ctx).await?)?;
        let (request, sha) = self.inspect(repo, number, ctx).await?;
        if sha != reviewed_sha {
            return Err(Error::Conflict("PR changed since owner review".into()));
        }
        let pr: Value = self
            .github
            .get(&format!("{}/pulls/{number}", repo.route()), ctx)
            .await?;
        if pr["merged"].as_bool() != Some(true) {
            let merge: Value = self
                .github
                .json_call(
                    Method::PUT,
                    &format!("{}/pulls/{number}/merge", repo.route()),
                    Some(&json!({"sha":reviewed_sha,"merge_method":"merge"})),
                    ctx,
                )
                .await?;
            if merge["merged"].as_bool() != Some(true) {
                return Err(Error::Conflict("PR not merged".into()));
            }
        }
        self.reconcile(repo, number, &request.id, ctx).await
    }
    /// Close a request PR. A merged request cannot be rejected retroactively.
    pub async fn reject(&self, repo: &Repository, number: u64, ctx: &Context) -> Result<()> {
        owner(&self.github.repository(repo.id, ctx).await?)?;
        self.inspect(repo, number, ctx).await?;
        let pr: Value = self
            .github
            .get(&format!("{}/pulls/{number}", repo.route()), ctx)
            .await?;
        if pr["merged"].as_bool() == Some(true) {
            return Err(Error::Conflict("merged request cannot be rejected".into()));
        }
        let _: Value = self
            .github
            .json_call(
                Method::PATCH,
                &format!("{}/pulls/{number}", repo.route()),
                Some(&json!({"state":"closed"})),
                ctx,
            )
            .await?;
        Ok(())
    }
    /// Apply a merged request to latest authoritative source. Handles merges made
    /// outside Tachyon, and retries after interrupted `accept` calls.
    pub async fn reconcile(
        &self,
        repo: &Repository,
        number: u64,
        id: &str,
        ctx: &Context,
    ) -> Result<String> {
        owner(&self.github.repository(repo.id, ctx).await?)?;
        let (reviewed, _) = self.inspect(repo, number, ctx).await?;
        let pr: Value = self
            .github
            .get(&format!("{}/pulls/{number}", repo.route()), ctx)
            .await?;
        if pr["merged"].as_bool() != Some(true) || reviewed.id != id {
            return Err(Error::Invalid("request not merged or mismatched ID".into()));
        }
        let head = self.github.head(repo, ctx).await?;
        let mut ledger = self.github.ledger(repo, &head, ctx).await?;
        if ledger.requests.get(id) != Some(&reviewed) {
            return Err(Error::Conflict("merged request changed in source".into()));
        }
        if let Some(applied) = ledger.applied.get(id) {
            if *applied != number {
                return Err(Error::Conflict("request applied by different PR".into()));
            }
            return Ok(head);
        }
        let mut candidate = self.source_manifest(repo, &head, ctx).await?;
        let next = reviewed.operation.apply(&ModList {
            mods: candidate.mods.clone(),
        })?;
        candidate.mods = next.mods.clone();
        validation::manifest(&candidate, &self.github.options.limits)?;
        ledger.applied.insert(id.into(), number);
        self.github
            .commit_files(
                repo,
                &repo.default_branch,
                &head,
                &BTreeMap::from([
                    ("mods.json".into(), serde_json::to_vec_pretty(&next)?),
                    ("requests.json".into(), serde_json::to_vec_pretty(&ledger)?),
                ]),
                &format!("Apply request {id} (PR #{number})"),
                ctx,
            )
            .await
    }
    async fn source_manifest(
        &self,
        repo: &Repository,
        sha: &str,
        ctx: &Context,
    ) -> Result<Manifest> {
        let pack = validation::json(
            &self.github.file(repo, "pack.json", sha, ctx).await?,
            &self.github.options.limits,
        )?;
        let mods: ModList = validation::json(
            &self.github.file(repo, "mods.json", sha, ctx).await?,
            &self.github.options.limits,
        )?;
        let manifest = Manifest {
            pack,
            mods: mods.mods,
            configs: BTreeMap::new(),
        };
        validation::manifest(&manifest, &self.github.options.limits)?;
        Ok(manifest)
    }
    /// Prepare exact source, upload/reconcile assets, then advance the stable pointer.
    /// `id` must be unique and reused when resuming. `expected_head` is the default
    /// branch head reviewed by the owner; stale publication never overwrites it.
    pub async fn publish(
        &self,
        repo: &Repository,
        id: &str,
        source_commit: &str,
        expected_head: &str,
        ctx: &Context,
    ) -> Result<Published> {
        validation::key(id)?;
        validation::commit(source_commit)?;
        validation::commit(expected_head)?;
        owner(&self.github.repository(repo.id, ctx).await?)?;
        let snapshot = self.prepare(repo, source_commit, ctx).await?;
        let tag = format!("hadron-{id}");
        let mut release=match self.github.get::<Value>(&format!("{}/releases/tags/{tag}",repo.route()),ctx).await {
            Ok(v)=>v,
            Err(Error::NotFound)=>self.github.json_call(Method::POST,&format!("{}/releases",repo.route()),Some(&json!({"tag_name":tag,"target_commitish":source_commit,"name":tag,"draft":true,"make_latest":"false","body":format!("Hadron publication {id}; source {source_commit}")})),ctx).await?,
            Err(e)=>return Err(e),
        };
        if release["target_commitish"].as_str() != Some(source_commit)
            || release["tag_name"].as_str() != Some(&tag)
        {
            return Err(Error::Conflict(
                "publication ID already belongs to another source".into(),
            ));
        }
        let prior = self.github.latest(repo, ctx).await?;
        let previous = if let Some(pointer) = prior.as_ref() {
            Some(self.load(repo, pointer, ctx).await?)
        } else {
            None
        };
        ctx.emit(Stage::Uploading)?;
        let prepared = snapshot.prepared;
        let m = &prepared.manifest_artifact;
        let uploaded = self
            .github
            .upload(
                repo,
                &release,
                &format!("manifest-{}.json.zst", m.sha256.as_str()),
                &m.bytes,
                ctx,
            )
            .await?;
        let manifest = m.asset(uploaded.id, uploaded.name);
        let configs = if let Some(c) = &prepared.config_artifact {
            if let Some(reusable) = previous
                .as_ref()
                .and_then(|p| p.publication.configs.as_ref())
                .filter(|a| {
                    a.decoded_sha256 == c.decoded_sha256 && a.decoded_size == c.decoded_size
                })
            {
                Some(reusable.clone())
            } else {
                let uploaded = self
                    .github
                    .upload(
                        repo,
                        &release,
                        &format!("configs-{}.tar.zst", c.sha256.as_str()),
                        &c.bytes,
                        ctx,
                    )
                    .await?;
                Some(c.asset(uploaded.id, uploaded.name))
            }
        } else {
            None
        };
        let publication = Publication {
            schema_version: 1,
            repository_id: repo.id,
            pack_id: prepared.manifest.pack.id,
            id: id.into(),
            source_commit: source_commit.into(),
            manifest,
            configs,
        };
        let index_bytes = serde_json::to_vec(&publication)?;
        let index = self
            .github
            .upload(repo, &release, "publication.json", &index_bytes, ctx)
            .await?;
        let latest = Latest {
            schema_version: 1,
            publication_id: id.into(),
            index,
        };
        // Confirm the entire consumer path before advertising this version.
        self.load(repo, &latest, ctx).await?;
        let release_id = github::number(&release, "id")?;
        if release["draft"].as_bool() == Some(true) {
            release = self
                .github
                .json_call(
                    Method::PATCH,
                    &format!("{}/releases/{release_id}", repo.route()),
                    Some(&json!({"draft":false,"make_latest":"false"})),
                    ctx,
                )
                .await?;
            if release["draft"].as_bool() != Some(false) {
                return Err(Error::Conflict("release still draft".into()));
            }
        }
        ctx.emit(Stage::Publishing)?;
        // An interrupted pointer write is idempotent when already selected.
        let current = self.github.latest(repo, ctx).await?;
        if current.as_ref() == Some(&latest) {
            return Ok(Published {
                publication,
                latest,
                pointer_commit: self.github.head(repo, ctx).await?,
            });
        }
        if self.github.head(repo, ctx).await? != expected_head {
            return Err(Error::Conflict(format!(
                "complete publication {id} uploaded; default branch advanced, retry with reviewed head"
            )));
        }
        let pointer_commit = self
            .github
            .commit_files(
                repo,
                &repo.default_branch,
                expected_head,
                &BTreeMap::from([(
                    ".hadron/latest.json".into(),
                    serde_json::to_vec_pretty(&latest)?,
                )]),
                &format!("Publish pack {id}"),
                ctx,
            )
            .await?;
        Ok(Published {
            publication,
            latest,
            pointer_commit,
        })
    }
    /// Owner-wide rollback: verify old publication then guardedly repoint stable.
    /// This does not modify source modlist or downgrade local worlds/saves.
    pub async fn select(
        &self,
        repo: &Repository,
        target: &Latest,
        expected_head: &str,
        ctx: &Context,
    ) -> Result<String> {
        owner(&self.github.repository(repo.id, ctx).await?)?;
        self.load(repo, target, ctx).await?;
        self.github
            .commit_files(
                repo,
                &repo.default_branch,
                expected_head,
                &BTreeMap::from([(
                    ".hadron/latest.json".into(),
                    serde_json::to_vec_pretty(target)?,
                )]),
                &format!("Select pack {}", target.publication_id),
                ctx,
            )
            .await
    }
}
fn owner(repo: &Repository) -> Result<()> {
    if repo.permissions.push || repo.permissions.maintain || repo.permissions.admin {
        Ok(())
    } else {
        Err(Error::Permission)
    }
}
fn proposal(pr: &Value, id: &str) -> Result<Proposal> {
    Ok(Proposal {
        number: github::number(pr, "number")?,
        url: github::str_field(pr, "html_url")?.into(),
        request_id: id.into(),
    })
}
fn request_delta(base: &Requests, head: &Requests) -> Result<Request> {
    if base.applied != head.applied
        || base.requests.len() + 1 != head.requests.len()
        || base
            .requests
            .iter()
            .any(|(id, r)| head.requests.get(id) != Some(r))
    {
        return Err(Error::Invalid(
            "PR must add exactly one request without changing ledger".into(),
        ));
    }
    head.requests
        .iter()
        .find(|(id, _)| !base.requests.contains_key(*id))
        .map(|(_, r)| r.clone())
        .ok_or_else(|| Error::Invalid("request missing".into()))
}
