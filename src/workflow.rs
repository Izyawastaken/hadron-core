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
    /// GitHub PR state; merged is distinct from applied and published.
    pub state: ProposalState,
}
/// GitHub review lifecycle exposed without leaking Octocrab models.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    /// Awaiting review or merge.
    Open,
    /// Closed without merging.
    Closed,
    /// Merged; source reconciliation/publication may still be pending.
    Merged,
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
    #[cfg(feature = "cache")]
    cache_policy: crate::cache::CachePolicy,
}
impl Hadron {
    /// Create without persistent artifact cache.
    pub fn new(github: Github) -> Self {
        Self {
            github,
            #[cfg(feature = "cache")]
            cache: None,
            #[cfg(feature = "cache")]
            cache_policy: crate::cache::CachePolicy::Strict,
        }
    }
    /// Enable caller-owned disk cache. It is safe to reuse across repositories:
    /// keys are exact content hashes, but private artifact bytes remain on disk.
    #[cfg(feature = "cache")]
    pub fn with_cache(mut self, cache: crate::cache::Cache) -> Self {
        self.cache = Some(cache);
        self
    }
    /// Select strict cache errors (default), or recovery using verified remote data.
    #[cfg(feature = "cache")]
    pub fn with_cache_policy(mut self, policy: crate::cache::CachePolicy) -> Self {
        self.cache_policy = policy;
        self
    }
    async fn admission(&self, ctx: &Context) -> Result<tokio::sync::OwnedSemaphorePermit> {
        ctx.check()?;
        tokio::select! {
            _ = ctx.cancellation.cancelled() => Err(Error::Cancelled),
            p = self.github.workflows.clone().acquire_owned() => p.map_err(|_| Error::Cancelled),
        }
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
            let a = asset.clone();
            let max = self.github.options.limits.compressed_bytes;
            let policy = self.cache_policy;
            let result = self
                .blocking(ctx, move || match cache.get(&a.sha256, a.size, max) {
                    Err(Error::Integrity) if policy == crate::cache::CachePolicy::Recover => {
                        cache.remove(&a.sha256)?;
                        Err(Error::Integrity)
                    }
                    other => other,
                })
                .await;
            match result {
                Ok(Some(bytes)) => return Ok(bytes),
                Ok(None) => {}
                Err(Error::Integrity | Error::Io(_))
                    if policy == crate::cache::CachePolicy::Recover =>
                {
                    ctx.emit(Stage::CacheSkipped)?
                }
                Err(e) => return Err(e),
            }
        }
        let bytes = self.github.asset(repo, asset, ctx).await?;
        #[cfg(feature = "cache")]
        if let Some(cache) = self.cache.clone() {
            let hash = asset.sha256.clone();
            // Move the buffer through blocking I/O rather than copying artifacts.
            let policy = self.cache_policy;
            let (bytes, result) = self
                .blocking(ctx, move || {
                    let result = cache.put(&hash, &bytes);
                    Ok((bytes, result))
                })
                .await?;
            match result {
                Ok(()) => {}
                Err(Error::Integrity | Error::Io(_))
                    if policy == crate::cache::CachePolicy::Recover =>
                {
                    ctx.emit(Stage::CacheSkipped)?
                }
                Err(e) => return Err(e),
            }
            return Ok(bytes);
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
        let _admission = self.admission(ctx).await?;
        self.load_inner(repo, latest, ctx).await
    }
    async fn index(
        &self,
        repo: &Repository,
        latest: &Latest,
        ctx: &Context,
    ) -> Result<Publication> {
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
        Ok(publication)
    }
    async fn metadata(
        &self,
        repo: &Repository,
        latest: &Latest,
        ctx: &Context,
    ) -> Result<(Publication, Manifest)> {
        let publication = self.index(repo, latest, ctx).await?;
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
        Ok((publication, manifest))
    }
    async fn load_inner(
        &self,
        repo: &Repository,
        latest: &Latest,
        ctx: &Context,
    ) -> Result<Loaded> {
        let (publication, manifest) = self.metadata(repo, latest, ctx).await?;
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
        let _admission = self.admission(ctx).await?;
        Ok(self.prepare_inner(repo, commit, None, ctx).await?.0)
    }
    async fn prepare_inner(
        &self,
        repo: &Repository,
        commit: &str,
        previous: Option<&(Publication, Manifest)>,
        ctx: &Context,
    ) -> Result<(Snapshot, Option<Asset>)> {
        validation::commit(commit)?;
        let pack: Pack = validation::json(
            &self.github.file(repo, "pack.json", commit, ctx).await?,
            &self.github.options.limits,
        )?;
        let mods: ModList = validation::json(
            &self.github.file(repo, "mods.json", commit, ctx).await?,
            &self.github.options.limits,
        )?;
        let entries = self.github.tree(repo, commit, ctx).await?;
        let mut configs = BTreeMap::new();
        let mut total = 0u64;
        let mut wanted = Vec::new();
        for (path, entry) in &entries {
            if !path.starts_with("config/") {
                continue;
            }
            validation::path(path)?;
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
            wanted.push((path.clone(), size));
        }
        ctx.emit(Stage::Preparing)?;
        if let Some((publication, manifest)) = previous {
            let reuse = async {
                let old = self
                    .github
                    .tree(repo, &publication.source_commit, ctx)
                    .await?;
                let filtered = |tree: &BTreeMap<String, Value>| -> BTreeMap<String, Value> {
                    tree.iter()
                        .filter(|(p, _)| p.starts_with("config/"))
                        .map(|(p, v)| (p.clone(), v.clone()))
                        .collect()
                };
                let unchanged = filtered(&old) == filtered(&entries);
                let mut next = Manifest {
                    pack: pack.clone(),
                    mods: mods.mods.clone(),
                    configs: manifest.configs.clone(),
                };
                let old_configs = if let Some(asset) = &publication.configs {
                    let bytes = self.github.asset(repo, asset, ctx).await?;
                    let a = asset.clone();
                    let m = manifest.clone();
                    let limits = self.github.options.limits.clone();
                    self.blocking(ctx, move || codec::configs(&bytes, &a, &m, &limits))
                        .await?
                } else if !wanted.is_empty() {
                    if unchanged {
                        return Err(Error::Integrity);
                    }
                    BTreeMap::new()
                } else {
                    BTreeMap::new()
                };
                if !unchanged {
                    // Only reuse bytes whose immutable Git blob identity, mode and
                    // size are unchanged, after verifying the entire old archive.
                    for (path, bytes) in old_configs {
                        if entries.contains_key(&path) && old.get(&path) == entries.get(&path) {
                            configs.insert(path, bytes);
                        }
                    }
                    return Ok(None);
                }
                // A config-empty source must also have an empty published inventory.
                if wanted.is_empty() {
                    next.configs.clear();
                }
                let limits = self.github.options.limits.clone();
                let prepared = self
                    .blocking(ctx, move || {
                        Ok(Prepared {
                            manifest_artifact: codec::encode_manifest(&next, &limits)?,
                            manifest: next,
                            config_artifact: None,
                        })
                    })
                    .await?;
                Ok(Some((
                    Snapshot {
                        commit: commit.into(),
                        prepared,
                    },
                    publication.configs.clone(),
                )))
            }
            .await;
            match reuse {
                Ok(Some(value)) => return Ok(value),
                Ok(None) => {}
                Err(e) if reusable_failure(&e) => ctx.emit(Stage::ReuseSkipped)?,
                Err(e) => return Err(e),
            }
        }
        // A bounded JoinSet admits at most concurrency source buffers at a time.
        let mut tasks = tokio::task::JoinSet::new();
        let mut wanted = wanted
            .into_iter()
            .filter(|(path, _)| !configs.contains_key(path))
            .collect::<Vec<_>>()
            .into_iter();
        loop {
            while tasks.len() < self.github.options.concurrency {
                let Some((path, size)) = wanted.next() else {
                    break;
                };
                let github = self.github.clone();
                let repo = repo.clone();
                let commit = commit.to_owned();
                let ctx = ctx.clone();
                tasks.spawn(async move {
                    let bytes = github
                        .file_bounded(&repo, &path, &commit, size, &ctx)
                        .await?;
                    if bytes.len() as u64 != size {
                        return Err(Error::Integrity);
                    }
                    Ok((path, bytes))
                });
            }
            if tasks.is_empty() {
                break;
            }
            let result = tokio::select! { _ = ctx.cancellation.cancelled() => return Err(Error::Cancelled), r = tasks.join_next() => r };
            let (path, bytes) = result
                .ok_or(Error::Cancelled)?
                .map_err(|_| Error::Network)??;
            configs.insert(path, bytes);
        }
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
        Ok((
            Snapshot {
                commit: commit.into(),
                prepared,
            },
            None,
        ))
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
    /// List one bounded page of PR history. An empty page ends iteration; filtering
    /// may yield an empty proposal list even when the raw PR page is nonempty.
    /// Returns `(proposals, has_more)`; request the next page when `has_more` is true.
    pub async fn proposals_page(
        &self,
        repo: &Repository,
        page: u32,
        ctx: &Context,
    ) -> Result<(Vec<Proposal>, bool)> {
        if page == 0 {
            return Err(Error::Invalid("page starts at one".into()));
        }
        let prs: Vec<Value> = self
            .github
            .get(
                &format!("{}/pulls?state=all&per_page=100&page={page}", repo.route()),
                ctx,
            )
            .await?;
        if prs.len() > 100 {
            return Err(Error::Limit("PR page"));
        }
        let more = prs.len() == 100;
        let proposals = prs
            .iter()
            .filter_map(|p| {
                p["head"]["ref"]
                    .as_str()
                    .and_then(|r| r.strip_prefix("hadron-request-"))
                    .map(|id| (p, id))
            })
            .map(|(p, id)| proposal(p, id))
            .collect::<Result<_>>()?;
        Ok((proposals, more))
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
        let current = self.github.head(repo, ctx).await?;
        if let Some(record) = self.archived(repo, &current, &request.id, ctx).await? {
            if record.requests.get(&request.id) != Some(&request) {
                return Err(Error::Conflict(
                    "request ID is archived with different data".into(),
                ));
            }
            let number = *record.applied.get(&request.id).ok_or(Error::Integrity)?;
            let pr: Value = self
                .github
                .get(&format!("{}/pulls/{number}", repo.route()), ctx)
                .await?;
            if pr["merged"].as_bool() != Some(true) {
                return Err(Error::Integrity);
            }
            return proposal(&pr, &request.id);
        }
        let branch = format!("hadron-request-{}", request.id);
        let login = if repo.permissions.push {
            repo.full_name
                .split('/')
                .next()
                .ok_or(Error::Integrity)?
                .to_owned()
        } else {
            let user: Value = self.github.get("/user", ctx).await?;
            github::str_field(&user, "login")?.to_owned()
        };
        let existing: Vec<Value> = self
            .github
            .pages(
                &format!(
                    "{}/pulls?state=all&head={}&base={}",
                    repo.route(),
                    github::segment(&format!("{login}:{branch}")),
                    github::segment(&repo.default_branch)
                ),
                ctx,
            )
            .await?;
        let existing = existing
            .iter()
            .map(|p| proposal(p, &request.id))
            .collect::<Result<Vec<_>>>()?;
        if let Some(p) = existing.into_iter().find(|p| p.request_id == request.id) {
            if self.inspect(repo, p.number, ctx).await?.0 != request {
                return Err(Error::Conflict(
                    "request ID belongs to different proposal".into(),
                ));
            }
            return Ok(p);
        }
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
        let reviewed_sha = github::str_field(&pr["head"], "sha")?.to_owned();
        validation::commit(&reviewed_sha)?;
        let merged = pr["merged"].as_bool() == Some(true);
        let (base_sha, head_sha, head_repo) = if merged {
            // Merge/squash/rebase result survives deleted forks and moving heads.
            // A multi-commit rebase whose final commit is not the complete request
            // delta fails closed; Hadron itself uses a single merge commit.
            let sha = github::str_field(&pr, "merge_commit_sha")?.to_owned();
            validation::commit(&sha)?;
            let c: Value = self
                .github
                .get(&format!("{}/git/commits/{sha}", repo.route()), ctx)
                .await?;
            let parent = c["parents"]
                .as_array()
                .and_then(|p| p.first())
                .ok_or(Error::Integrity)?;
            (
                github::str_field(parent, "sha")?.to_owned(),
                sha,
                repo.clone(),
            )
        } else {
            let base = github::str_field(&pr["base"], "sha")?;
            let merge_base = self
                .github
                .merge_base(repo, base, &reviewed_sha, ctx)
                .await?;
            let head_repo = Repository {
                id: github::number(&pr["head"]["repo"], "id")?,
                ..repo.clone()
            };
            (merge_base, reviewed_sha.clone(), head_repo)
        };
        let mut before = self.github.tree(repo, &base_sha, ctx).await?;
        let mut after = self.github.tree(&head_repo, &head_sha, ctx).await?;
        let old_request = before.remove("requests.json");
        let new_request = after.remove("requests.json").ok_or(Error::Integrity)?;
        if before != after
            || old_request.as_ref() == Some(&new_request)
            || new_request["type"] != "blob"
            || new_request["mode"] != "100644"
        {
            return Err(Error::Invalid(
                "request PR must change only regular requests.json".into(),
            ));
        }
        let base = self.github.ledger(repo, &base_sha, ctx).await?;
        let head = self.github.ledger(&head_repo, &head_sha, ctx).await?;
        let request = request_delta(&base, &head)?;
        if self
            .github
            .merge_base(repo, &request.base_commit, &base_sha, ctx)
            .await?
            != request.base_commit
        {
            return Err(Error::Invalid(
                "request base is not an ancestor of comparison base".into(),
            ));
        }
        if pr["head"]["ref"].as_str() != Some(&format!("hadron-request-{}", request.id)) {
            return Err(Error::Invalid("request branch ID mismatch".into()));
        }
        Ok((request, reviewed_sha))
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
        let pr: Value = self
            .github
            .get(&format!("{}/pulls/{number}", repo.route()), ctx)
            .await?;
        if pr["merged"].as_bool() != Some(true)
            || pr["base"]["repo"]["id"].as_u64() != Some(repo.id)
            || pr["base"]["ref"].as_str() != Some(&repo.default_branch)
        {
            return Err(Error::Invalid("request not merged or mismatched ID".into()));
        }
        let head = self.github.head(repo, ctx).await?;
        let mut ledger = self.github.ledger(repo, &head, ctx).await?;
        if !ledger.requests.contains_key(id)
            && let Some(record) = self.archived(repo, &head, id, ctx).await?
        {
            if record.applied.get(id) == Some(&number) {
                return Ok(head);
            }
            return Err(Error::Conflict(
                "request archived under different PR".into(),
            ));
        }
        if let Some(applied) = ledger.applied.get(id) {
            if *applied != number {
                return Err(Error::Conflict("request applied by different PR".into()));
            }
            return Ok(head);
        }
        let (reviewed, _) = self.inspect(repo, number, ctx).await?;
        if reviewed.id != id || ledger.requests.get(id) != Some(&reviewed) {
            return Err(Error::Conflict("merged request changed in source".into()));
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
    async fn archived(
        &self,
        repo: &Repository,
        sha: &str,
        id: &str,
        ctx: &Context,
    ) -> Result<Option<Requests>> {
        validation::key(id)?;
        let bytes = match self
            .github
            .file(repo, &format!(".hadron/requests/{id}.json"), sha, ctx)
            .await
        {
            Ok(bytes) => bytes,
            Err(Error::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        let record: Requests = validation::json(&bytes, &self.github.options.limits)?;
        validation::requests(&record, &self.github.options.limits)?;
        if record.requests.len() != 1
            || record.applied.len() != 1
            || !record.requests.contains_key(id)
        {
            return Err(Error::Integrity);
        }
        Ok(Some(record))
    }
    /// Explicitly archive applied requests into immutable per-ID source records.
    /// Ledger removal and archive creation share a guarded atomic commit. Archived
    /// IDs remain reserved and reconciliation remains idempotent. Pending requests
    /// cannot be archived. Old 0.0.1 clients must not write to an archived repository.
    pub async fn archive_requests(
        &self,
        repo: &Repository,
        ids: &[String],
        expected_head: &str,
        ctx: &Context,
    ) -> Result<String> {
        owner(&self.github.repository(repo.id, ctx).await?)?;
        if ids.is_empty() || ids.len().saturating_add(1) > self.github.options.limits.entries {
            return Err(Error::Limit("archive entries"));
        }
        let mut ledger = self.github.ledger(repo, expected_head, ctx).await?;
        let mut files = BTreeMap::new();
        for id in ids {
            validation::key(id)?;
            if self.archived(repo, expected_head, id, ctx).await?.is_some() {
                return Err(Error::Conflict("request already archived".into()));
            }
            let number = ledger
                .applied
                .remove(id)
                .ok_or_else(|| Error::Conflict("only applied requests can be archived".into()))?;
            let request = ledger.requests.remove(id).ok_or(Error::Integrity)?;
            let record = Requests {
                schema_version: 1,
                requests: BTreeMap::from([(id.clone(), request)]),
                applied: BTreeMap::from([(id.clone(), number)]),
            };
            files.insert(
                format!(".hadron/requests/{id}.json"),
                serde_json::to_vec_pretty(&record)?,
            );
        }
        files.insert("requests.json".into(), serde_json::to_vec_pretty(&ledger)?);
        self.github
            .commit_files(
                repo,
                &repo.default_branch,
                expected_head,
                &files,
                "Archive applied Hadron requests",
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
        let _admission = self.admission(ctx).await?;
        validation::key(id)?;
        validation::commit(source_commit)?;
        validation::commit(expected_head)?;
        owner(&self.github.repository(repo.id, ctx).await?)?;
        let tag = format!("hadron-{id}");
        let mut release = match self
            .github
            .get::<Value>(&format!("{}/releases/tags/{tag}", repo.route()), ctx)
            .await
        {
            Ok(v) => v,
            Err(Error::NotFound) => {
                // The tag endpoint is specified for published releases. Drafts
                // left by interrupted uploads must also be found before creating.
                let releases = self
                    .github
                    .pages(&format!("{}/releases", repo.route()), ctx)
                    .await?;
                if let Some(existing) = releases
                    .into_iter()
                    .find(|v| v["tag_name"].as_str() == Some(&tag))
                {
                    existing
                } else {
                    self.github.json_call(Method::POST,&format!("{}/releases",repo.route()),Some(&json!({"tag_name":tag,"target_commitish":source_commit,"name":tag,"draft":true,"make_latest":"false","body":format!("Hadron publication {id}; source {source_commit}")})),ctx).await?
                }
            }
            Err(e) => return Err(e),
        };
        if release["target_commitish"].as_str() != Some(source_commit)
            || release["tag_name"].as_str() != Some(&tag)
        {
            return Err(Error::Conflict(
                "publication ID already belongs to another source".into(),
            ));
        }
        let existing = self.release_pointer(repo, &release, id, ctx).await?;
        let (publication, latest) = if let Some(latest) = existing {
            let loaded = self.load_inner(repo, &latest, ctx).await?;
            if loaded.publication.source_commit != source_commit {
                return Err(Error::Integrity);
            }
            let snapshot = self.prepare_inner(repo, source_commit, None, ctx).await?.0;
            if loaded.manifest != snapshot.prepared.manifest {
                return Err(Error::Integrity);
            }
            match (
                &loaded.publication.configs,
                &snapshot.prepared.config_artifact,
            ) {
                (Some(a), Some(b))
                    if a.decoded_sha256 == b.decoded_sha256 && a.decoded_size == b.decoded_size => {
                }
                (None, None) => {}
                _ => return Err(Error::Integrity),
            }
            (loaded.publication, latest)
        } else {
            let previous = match async {
                match self.github.latest(repo, ctx).await? {
                    Some(pointer) => self.metadata(repo, &pointer, ctx).await.map(Some),
                    None => Ok(None),
                }
            }
            .await
            {
                Ok(p) => p,
                Err(e) if reusable_failure(&e) => {
                    ctx.emit(Stage::ReuseSkipped)?;
                    None
                }
                Err(e) => return Err(e),
            };
            let (snapshot, reused) = self
                .prepare_inner(repo, source_commit, previous.as_ref(), ctx)
                .await?;
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
            } else {
                reused
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
            if index_bytes.len() as u64 > self.github.options.limits.json_bytes {
                return Err(Error::Limit("publication JSON"));
            }
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
            self.load_inner(repo, &latest, ctx).await?;
            (publication, latest)
        };
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
        let current = match self.github.latest(repo, ctx).await {
            Ok(current) => current,
            Err(e) if reusable_failure(&e) => {
                ctx.emit(Stage::ReuseSkipped)?;
                None
            }
            Err(e) => return Err(e),
        };
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
    async fn release_pointer(
        &self,
        repo: &Repository,
        release: &Value,
        id: &str,
        ctx: &Context,
    ) -> Result<Option<Latest>> {
        let assets = self
            .github
            .pages(
                &format!(
                    "{}/releases/{}/assets",
                    repo.route(),
                    github::number(release, "id")?
                ),
                ctx,
            )
            .await?;
        let Some(a) = assets.iter().find(|a| a["name"] == "publication.json") else {
            return Ok(None);
        };
        let size = github::number(a, "size")?;
        let digest = github::str_field(a, "digest")?
            .strip_prefix("sha256:")
            .ok_or(Error::Integrity)?;
        let hash = Hash::parse(digest)?;
        Ok(Some(Latest {
            schema_version: 1,
            publication_id: id.into(),
            index: Asset {
                id: github::number(a, "id")?,
                name: "publication.json".into(),
                size,
                sha256: hash.clone(),
                decoded_size: size,
                decoded_sha256: hash,
            },
        }))
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
fn reusable_failure(e: &Error) -> bool {
    matches!(
        e,
        Error::NotFound
            | Error::Integrity
            | Error::Codec
            | Error::Schema(_)
            | Error::Json(_)
            | Error::Invalid(_)
            | Error::Limit(_)
    )
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
        state: if pr["merged"].as_bool() == Some(true) || pr["merged_at"].is_string() {
            ProposalState::Merged
        } else if pr["state"].as_str() == Some("closed") {
            ProposalState::Closed
        } else {
            ProposalState::Open
        },
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
