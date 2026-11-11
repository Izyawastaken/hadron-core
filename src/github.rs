//! Bounded GitHub REST adapter built on caller-configured Octocrab.
//!
//! Use a client without redirect/retry middleware. Hadron follows asset redirects
//! with a separate unauthenticated client and never retries mutations itself.
use crate::{
    Error, Result,
    model::*,
    share,
    validation::{self, Limits},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http::{HeaderMap, Method, StatusCode};
use http_body_util::BodyExt;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
pub use tokio_util::sync::CancellationToken;

/// Remote repository identity returned by GitHub, not by pack JSON.
#[derive(Clone, Debug, Deserialize)]
pub struct Repository {
    /// Numeric repository ID; stable across path changes.
    pub id: u64,
    /// Current owner/name.
    pub full_name: String,
    /// Default branch (not necessarily `main`).
    pub default_branch: String,
    /// Whether authenticated caller may push, when GitHub supplies permissions.
    #[serde(default)]
    pub permissions: Permissions,
}
/// Relevant repository permissions, as reported for this identity.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Permissions {
    /// Content write permission.
    #[serde(default)]
    pub push: bool,
    /// Repository administrator permission.
    #[serde(default)]
    pub admin: bool,
    /// Repository maintain permission.
    #[serde(default)]
    pub maintain: bool,
}
impl Repository {
    pub(crate) fn route(&self) -> String {
        format!("/repositories/{}", self.id)
    }
}
/// Stage notification. No UI framework or credentials are exposed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    /// Resolving source/repository metadata.
    Fetching,
    /// Validating/compressing a source snapshot.
    Preparing,
    /// Uploading artifacts.
    Uploading,
    /// Advancing the stable pointer.
    Publishing,
    /// Optional prior-artifact reuse was skipped because its data was unavailable.
    ReuseSkipped,
    /// Recover-mode cache read or write failed; verified remote data is used.
    CacheSkipped,
}
/// Per-operation cancellation and optional progress observer.
#[derive(Clone, Default)]
pub struct Context {
    /// Cooperative cancellation; remote mutations already accepted are not undone.
    pub cancellation: CancellationToken,
    /// Fast, nonblocking stage callback. Called on the async task; must not panic.
    pub progress: Option<Arc<dyn Fn(Stage) + Send + Sync>>,
}
impl Context {
    pub(crate) fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    pub(crate) fn emit(&self, stage: Stage) -> Result<()> {
        self.check()?;
        if let Some(f) = &self.progress {
            f(stage);
        }
        Ok(())
    }
}
/// Bounded network and decoding settings shared by all operations.
#[derive(Clone, Debug)]
pub struct Options {
    /// Resource budgets.
    pub limits: Limits,
    /// Deadline for a complete individual request, including body consumption.
    pub timeout: Duration,
    /// Concurrent network/CPU operations per client (minimum one).
    pub concurrency: usize,
    /// Additional retries of transient GETs, capped at five.
    pub read_retries: usize,
    /// Concurrent memory-heavy pack operations per adapter (1..=64).
    pub workflows: usize,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            timeout: Duration::from_secs(60),
            concurrency: 4,
            read_retries: 2,
            workflows: 1,
        }
    }
}
type PointerCache = BTreeMap<u64, (String, Vec<u8>)>;
/// Reusable adapter. API calls use Octocrab; redirected downloads never carry its auth.
#[derive(Clone)]
pub struct Github {
    pub(crate) crab: Arc<octocrab::Octocrab>,
    pub(crate) options: Options,
    downloads: reqwest::Client,
    network: Arc<Semaphore>,
    pub(crate) cpu: Arc<Semaphore>,
    pub(crate) workflows: Arc<Semaphore>,
    pointers: Arc<Mutex<PointerCache>>,
}
pub(crate) struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: Vec<u8>,
}
impl Github {
    /// Reuse a configured client and its auth. Disable Octocrab redirect/retry features
    /// and equivalent custom middleware, including features enabled elsewhere in your
    /// dependency graph. The default Hadron dependency disables both.
    pub fn new(crab: octocrab::Octocrab, options: Options) -> Result<Self> {
        if options.concurrency == 0
            || options.concurrency > 64
            || options.read_retries > 5
            || options.timeout.is_zero()
            || !(1..=64).contains(&options.workflows)
        {
            return Err(Error::Invalid("invalid client options".into()));
        }
        let downloads = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(options.timeout)
            .build()
            .map_err(|_| Error::Network)?;
        Ok(Self {
            crab: Arc::new(crab),
            network: Arc::new(Semaphore::new(options.concurrency)),
            cpu: Arc::new(Semaphore::new(options.concurrency)),
            workflows: Arc::new(Semaphore::new(options.workflows)),
            options,
            downloads,
            pointers: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    /// Resolve a share code and validate GitHub's returned repository identity.
    pub async fn resolve(&self, code: &str, ctx: &Context) -> Result<Repository> {
        let id = share::decode(code)?;
        self.repository(id, ctx).await
    }
    /// Fetch repository metadata by numeric REST ID, with caller's authorization.
    pub async fn repository(&self, id: u64, ctx: &Context) -> Result<Repository> {
        ctx.emit(Stage::Fetching)?;
        let repo: Repository = self.get(&format!("/repositories/{id}"), ctx).await?;
        if repo.id != id {
            return Err(Error::Integrity);
        }
        Ok(repo)
    }
    pub(crate) async fn wait<T>(
        &self,
        ctx: &Context,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::select! {
            _=ctx.cancellation.cancelled()=>Err(Error::Cancelled),
            v=tokio::time::timeout(self.options.timeout,future)=>v.map_err(|_| Error::Network)?,
        }
    }
    pub(crate) async fn raw(
        &self,
        method: Method,
        route: &str,
        body: Vec<u8>,
        headers: HeaderMap,
        max: u64,
        ctx: &Context,
    ) -> Result<Reply> {
        // Routes are generated internally. Absolute requests are only permitted for
        // a validated GitHub upload URL, never for a URL found in a manifest.
        let attempts = if method == Method::GET {
            self.options.read_retries + 1
        } else {
            1
        };
        let mut body = Some(body);
        for attempt in 0..attempts {
            ctx.check()?;
            let result = self
                .wait(ctx, async {
                    let _permit = self.network.acquire().await.map_err(|_| Error::Cancelled)?;
                    let mut builder = http::Request::builder()
                        .method(method.clone())
                        .uri(route)
                        .header(
                            "user-agent",
                            concat!("hadron-core/", env!("CARGO_PKG_VERSION")),
                        );
                    for (k, v) in &headers {
                        builder = builder.header(k, v);
                    }
                    let request = builder
                        .body(if method == Method::GET {
                            body.as_ref().cloned().unwrap_or_default()
                        } else {
                            body.take().unwrap_or_default()
                        })
                        .map_err(|_| Error::Invalid("invalid internal HTTP request".into()))?;
                    let response = self
                        .crab
                        .execute(request)
                        .await
                        .map_err(|_| Error::Network)?;
                    let status = response.status();
                    let headers = response.headers().clone();
                    // Error bodies can contain URLs/secrets and are not needed for decisions.
                    if !status.is_success() {
                        return Ok(Reply {
                            status,
                            headers,
                            bytes: Vec::new(),
                        });
                    }
                    if headers
                        .get("content-length")
                        .and_then(|h| h.to_str().ok())
                        .and_then(|h| h.parse::<u64>().ok())
                        .is_some_and(|n| n > max)
                    {
                        return Err(Error::Limit("HTTP body"));
                    }
                    let mut body = response.into_body();
                    let mut bytes = Vec::new();
                    while let Some(frame) = body.frame().await {
                        if let Ok(data) = frame.map_err(|_| Error::Network)?.into_data() {
                            if (bytes.len() as u64).saturating_add(data.len() as u64) > max {
                                return Err(Error::Limit("HTTP body"));
                            }
                            bytes.extend_from_slice(&data);
                        }
                    }
                    Ok(Reply {
                        status,
                        headers,
                        bytes,
                    })
                })
                .await;
            let retry = match &result {
                Ok(r) => {
                    r.status == StatusCode::TOO_MANY_REQUESTS
                        || r.status.is_server_error()
                        || (r.status == StatusCode::FORBIDDEN && delay(&r.headers).is_some())
                }
                Err(Error::Network) => true,
                _ => false,
            };
            if retry && attempt + 1 < attempts {
                let seconds = result
                    .as_ref()
                    .ok()
                    .and_then(|r| delay(&r.headers))
                    .unwrap_or(1u64 << attempt);
                // Do not turn a rate-limited request into an unbounded sleep.
                if seconds > 30 {
                    return result.and_then(|r| Err(api_error(&r)));
                }
                tokio::select! { _=ctx.cancellation.cancelled()=>return Err(Error::Cancelled), _=tokio::time::sleep(Duration::from_secs(seconds))=>{} }
                continue;
            }
            return result;
        }
        Err(Error::Network)
    }
    pub(crate) async fn get<T: DeserializeOwned>(&self, route: &str, ctx: &Context) -> Result<T> {
        self.json_call(Method::GET, route, None, ctx).await
    }
    pub(crate) async fn json_call<T: DeserializeOwned>(
        &self,
        method: Method,
        route: &str,
        body: Option<&Value>,
        ctx: &Context,
    ) -> Result<T> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()?
            .unwrap_or_default();
        let outbound_limit = if route.ends_with("/git/blobs") {
            self.options
                .limits
                .json_bytes
                .saturating_add(2)
                .saturating_div(3)
                .saturating_mul(4)
                .saturating_add(64)
        } else {
            self.options.limits.json_bytes
        };
        if bytes.len() as u64 > outbound_limit {
            return Err(Error::Limit("outbound JSON"));
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            http::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "content-type",
            http::HeaderValue::from_static("application/json"),
        );
        let r = self
            .raw(
                method,
                route,
                bytes,
                headers,
                self.options.limits.json_bytes,
                ctx,
            )
            .await?;
        if !r.status.is_success() {
            return Err(api_error(&r));
        }
        validation::json(&r.bytes, &self.options.limits)
    }
    /// Current default-branch head; pass returned SHA to guarded writes.
    pub async fn head(&self, repo: &Repository, ctx: &Context) -> Result<String> {
        let v: Value = self
            .get(
                &format!(
                    "{}/git/ref/heads/{}",
                    repo.route(),
                    segment(&repo.default_branch)
                ),
                ctx,
            )
            .await?;
        let sha = str_field(&v["object"], "sha")?.to_owned();
        validation::commit(&sha)?;
        Ok(sha)
    }
    /// Fetch a bounded source file at an exact commit, with raw contents media type.
    pub async fn file(
        &self,
        repo: &Repository,
        path: &str,
        commit: &str,
        ctx: &Context,
    ) -> Result<Vec<u8>> {
        self.file_bounded(repo, path, commit, self.options.limits.json_bytes, ctx)
            .await
    }
    /// Fetch a raw source file with an explicit byte budget, independent of JSON.
    pub async fn file_bounded(
        &self,
        repo: &Repository,
        path: &str,
        commit: &str,
        maximum: u64,
        ctx: &Context,
    ) -> Result<Vec<u8>> {
        validation::path(path)?;
        validation::commit(commit)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            http::HeaderValue::from_static("application/vnd.github.raw+json"),
        );
        let r = self
            .raw(
                Method::GET,
                &format!(
                    "{}/contents/{}?ref={}",
                    repo.route(),
                    path.split('/').map(segment).collect::<Vec<_>>().join("/"),
                    commit
                ),
                Vec::new(),
                headers,
                maximum,
                ctx,
            )
            .await?;
        if !r.status.is_success() {
            return Err(api_error(&r));
        }
        Ok(r.bytes)
    }
    pub(crate) async fn ledger(
        &self,
        repo: &Repository,
        sha: &str,
        ctx: &Context,
    ) -> Result<Requests> {
        let r = match self.file(repo, "requests.json", sha, ctx).await {
            Ok(bytes) => validation::json(&bytes, &self.options.limits)?,
            Err(Error::NotFound) => Requests::default(),
            Err(e) => return Err(e),
        };
        validation::requests(&r, &self.options.limits)?;
        Ok(r)
    }
    /// Atomically replace several UTF-8 source files in one commit. Fast-forward-only
    /// ref update prevents loss of concurrent commits. An ambiguous timeout may leave
    /// the new commit selected; inspect the branch before retrying.
    pub async fn commit_files(
        &self,
        repo: &Repository,
        branch: &str,
        parent: &str,
        files: &BTreeMap<String, Vec<u8>>,
        message: &str,
        ctx: &Context,
    ) -> Result<String> {
        validation::commit(parent)?;
        if files.is_empty() || files.len() > self.options.limits.entries {
            return Err(Error::Limit("commit entries"));
        }
        let base: Value = self
            .get(&format!("{}/git/commits/{parent}", repo.route()), ctx)
            .await?;
        let tree_sha = str_field(&base["tree"], "sha")?;
        let mut tree = Vec::new();
        for (path, bytes) in files {
            validation::path(path)?;
            if bytes.len() as u64 > self.options.limits.json_bytes {
                return Err(Error::Limit("source file"));
            }
            let blob: Value = self
                .json_call(
                    Method::POST,
                    &format!("{}/git/blobs", repo.route()),
                    Some(&json!({"encoding":"base64","content":STANDARD.encode(bytes)})),
                    ctx,
                )
                .await?;
            tree.push(
                json!({"path":path,"mode":"100644","type":"blob","sha":str_field(&blob,"sha")?}),
            );
        }
        let t: Value = self
            .json_call(
                Method::POST,
                &format!("{}/git/trees", repo.route()),
                Some(&json!({"base_tree":tree_sha,"tree":tree})),
                ctx,
            )
            .await?;
        let c: Value = self
            .json_call(
                Method::POST,
                &format!("{}/git/commits", repo.route()),
                Some(&json!({"message":message,"tree":str_field(&t,"sha")?,"parents":[parent]})),
                ctx,
            )
            .await?;
        let sha = str_field(&c, "sha")?.to_owned();
        validation::commit(&sha)?;
        let _: Value = self
            .json_call(
                Method::PATCH,
                &format!("{}/git/refs/heads/{}", repo.route(), segment(branch)),
                Some(&json!({"sha":sha,"force":false})),
                ctx,
            )
            .await?;
        Ok(sha)
    }
    pub(crate) async fn pages(&self, route: &str, ctx: &Context) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        for page in 1..=self.options.limits.entries.max(1) {
            let sep = if route.contains('?') { '&' } else { '?' };
            let values: Vec<Value> = self
                .get(&format!("{route}{sep}per_page=100&page={page}"), ctx)
                .await?;
            let done = values.len() < 100;
            if all.len().saturating_add(values.len()) > self.options.limits.entries {
                return Err(Error::Limit("API collection"));
            }
            all.extend(values);
            if done {
                return Ok(all);
            }
        }
        Err(Error::Limit("API pages"))
    }
    /// Discard conditional pointer metadata for one repository, or all repositories.
    pub fn clear_metadata(&self, repository: Option<u64>) -> Result<()> {
        let mut cache = self.pointers.lock().map_err(|_| Error::Network)?;
        if let Some(id) = repository {
            cache.remove(&id);
        } else {
            cache.clear();
        }
        Ok(())
    }
    pub(crate) async fn tree(
        &self,
        repo: &Repository,
        commit: &str,
        ctx: &Context,
    ) -> Result<BTreeMap<String, Value>> {
        validation::commit(commit)?;
        let c: Value = self
            .get(&format!("{}/git/commits/{commit}", repo.route()), ctx)
            .await?;
        let sha = str_field(&c["tree"], "sha")?;
        validation::commit(sha)?;
        let v: Value = self
            .get(
                &format!("{}/git/trees/{sha}?recursive=1", repo.route()),
                ctx,
            )
            .await?;
        if v["truncated"].as_bool() != Some(false) {
            return Err(Error::Limit("truncated Git tree"));
        }
        let entries = v["tree"].as_array().ok_or(Error::Integrity)?;
        if entries.len() > self.options.limits.entries {
            return Err(Error::Limit("Git tree entries"));
        }
        let mut result = BTreeMap::new();
        for e in entries {
            let path = str_field(e, "path")?;
            if e["type"] == "tree" {
                continue;
            }
            validation::commit(str_field(e, "sha")?)?;
            // API blob URLs differ across forks; compare only immutable Git data.
            let identity = json!({"type":str_field(e,"type")?,"mode":str_field(e,"mode")?,"sha":str_field(e,"sha")?,"size":e["size"]});
            if result.insert(path.into(), identity).is_some() {
                return Err(Error::Integrity);
            }
        }
        Ok(result)
    }
    pub(crate) async fn merge_base(
        &self,
        repo: &Repository,
        base: &str,
        head: &str,
        ctx: &Context,
    ) -> Result<String> {
        validation::commit(base)?;
        validation::commit(head)?;
        let v: Value = self
            .get(
                &format!("{}/compare/{base}...{head}?per_page=1", repo.route()),
                ctx,
            )
            .await?;
        let sha = str_field(&v["merge_base_commit"], "sha")?.to_owned();
        validation::commit(&sha)?;
        Ok(sha)
    }
    /// Conditional pointer read. Cache is in-memory per adapter; a fresh process
    /// re-reads discovery metadata. `None` means this repository has no pointer.
    pub async fn latest(&self, repo: &Repository, ctx: &Context) -> Result<Option<Latest>> {
        let cached = self
            .pointers
            .lock()
            .map_err(|_| Error::Network)?
            .get(&repo.id)
            .cloned();
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            http::HeaderValue::from_static("application/vnd.github.raw+json"),
        );
        if let Some((etag, _)) = &cached {
            headers.insert("if-none-match", etag.parse().map_err(|_| Error::Integrity)?);
        }
        let r = self
            .raw(
                Method::GET,
                &format!(
                    "{}/contents/.hadron/latest.json?ref={}",
                    repo.route(),
                    segment(&repo.default_branch)
                ),
                Vec::new(),
                headers,
                self.options.limits.json_bytes,
                ctx,
            )
            .await?;
        if r.status == StatusCode::NOT_FOUND {
            self.pointers
                .lock()
                .map_err(|_| Error::Network)?
                .remove(&repo.id);
            return Ok(None);
        }
        let bytes = if r.status == StatusCode::NOT_MODIFIED {
            cached.ok_or(Error::Integrity)?.1
        } else {
            if !r.status.is_success() {
                return Err(api_error(&r));
            }
            if let Some(etag) = r.headers.get("etag").and_then(|h| h.to_str().ok()) {
                let mut cache = self.pointers.lock().map_err(|_| Error::Network)?;
                // Bound metadata even when a launcher visits many repositories.
                if cache.len() >= 128 && !cache.contains_key(&repo.id) {
                    cache.pop_first();
                }
                cache.insert(repo.id, (etag.to_owned(), r.bytes.clone()));
            } else {
                self.pointers
                    .lock()
                    .map_err(|_| Error::Network)?
                    .remove(&repo.id);
            }
            r.bytes
        };
        let p: Latest = validation::json(&bytes, &self.options.limits)?;
        validation::schema(p.schema_version)?;
        validation::key(&p.publication_id)?;
        validation::asset(&p.index, &self.options.limits)?;
        Ok(Some(p))
    }
    /// Download an asset by ID, with strict bounds. Redirect requests use no auth.
    pub async fn asset(&self, repo: &Repository, asset: &Asset, ctx: &Context) -> Result<Vec<u8>> {
        validation::asset(asset, &self.options.limits)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            http::HeaderValue::from_static("application/octet-stream"),
        );
        let r = self
            .raw(
                Method::GET,
                &format!("{}/releases/assets/{}", repo.route(), asset.id),
                Vec::new(),
                headers,
                asset.size,
                ctx,
            )
            .await?;
        let bytes = if r.status.is_redirection() {
            let location = r
                .headers
                .get("location")
                .and_then(|h| h.to_str().ok())
                .ok_or(Error::Integrity)?;
            self.download(location, asset.size, ctx).await?
        } else {
            if !r.status.is_success() {
                return Err(api_error(&r));
            }
            r.bytes
        };
        crate::codec::verify(&bytes, asset)?;
        Ok(bytes)
    }
    async fn download(&self, location: &str, max: u64, ctx: &Context) -> Result<Vec<u8>> {
        let mut target = location.to_owned();
        for _ in 0..4 {
            allowed_download(&target)?;
            let r = self
                .wait(ctx, async {
                    let _permit = self.network.acquire().await.map_err(|_| Error::Cancelled)?;
                    let mut response = self
                        .downloads
                        .get(&target)
                        .send()
                        .await
                        .map_err(|_| Error::Network)?;
                    let status = response.status();
                    let headers = response.headers().clone();
                    if status.is_redirection() {
                        return Ok(Reply {
                            status,
                            headers,
                            bytes: Vec::new(),
                        });
                    }
                    if !status.is_success() {
                        return Err(Error::Network);
                    }
                    if response.content_length().is_some_and(|n| n > max) {
                        return Err(Error::Limit("redirected body"));
                    }
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Network)? {
                        if (bytes.len() as u64).saturating_add(chunk.len() as u64) > max {
                            return Err(Error::Limit("redirected body"));
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(Reply {
                        status,
                        headers,
                        bytes,
                    })
                })
                .await?;
            if r.status.is_redirection() {
                target = r
                    .headers
                    .get("location")
                    .and_then(|h| h.to_str().ok())
                    .ok_or(Error::Integrity)?
                    .to_owned();
            } else {
                return Ok(r.bytes);
            }
        }
        Err(Error::Invalid("asset redirect limit".into()))
    }
    pub(crate) async fn upload(
        &self,
        repo: &Repository,
        release: &Value,
        name: &str,
        bytes: &[u8],
        ctx: &Context,
    ) -> Result<Asset> {
        validation::path(name)?;
        if bytes.len() as u64 > self.options.limits.compressed_bytes {
            return Err(Error::Limit("upload bytes"));
        }
        let release_id = number(release, "id")?;
        let assets = self
            .pages(
                &format!("{}/releases/{release_id}/assets", repo.route()),
                ctx,
            )
            .await?;
        for a in assets {
            if a["name"].as_str() == Some(name) {
                let id = number(&a, "id")?;
                let asset = Asset {
                    id,
                    name: name.into(),
                    size: bytes.len() as u64,
                    sha256: Hash::of(bytes),
                    decoded_size: bytes.len() as u64,
                    decoded_sha256: Hash::of(bytes),
                };
                self.asset(repo, &asset, ctx).await?;
                return Ok(asset);
            }
        }
        let template = str_field(release, "upload_url")?;
        let base = template.split('{').next().ok_or(Error::Integrity)?;
        let mut url = url::Url::parse(base).map_err(|_| Error::Integrity)?;
        // Uploads use GitHub's separate origin, authenticated by Octocrab.
        let expected = format!("/repos/{}/releases/{release_id}/assets", repo.full_name);
        if url.scheme() != "https"
            || url.host_str() != Some("uploads.github.com")
            || url.path() != expected
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::Invalid("untrusted upload URL".into()));
        }
        url.query_pairs_mut().append_pair("name", name);
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            http::HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(
            "accept",
            http::HeaderValue::from_static("application/vnd.github+json"),
        );
        let r = self
            .raw(
                Method::POST,
                url.as_str(),
                bytes.to_vec(),
                headers,
                self.options.limits.json_bytes,
                ctx,
            )
            .await?;
        if !r.status.is_success() {
            return Err(api_error(&r));
        }
        let v: Value = validation::json(&r.bytes, &self.options.limits)?;
        if number(&v, "size")? != bytes.len() as u64 {
            return Err(Error::Integrity);
        }
        let asset = Asset {
            id: number(&v, "id")?,
            name: name.into(),
            size: bytes.len() as u64,
            sha256: Hash::of(bytes),
            decoded_size: bytes.len() as u64,
            decoded_sha256: Hash::of(bytes),
        };
        self.asset(repo, &asset, ctx).await?;
        Ok(asset)
    }
}
fn allowed_download(value: &str) -> Result<()> {
    let url = url::Url::parse(value).map_err(|_| Error::Integrity)?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(
            url.host_str(),
            Some(
                "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
                    | "github.com"
            )
        )
    {
        return Err(Error::Invalid("untrusted asset redirect".into()));
    }
    Ok(())
}
fn delay(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            if headers.get("x-ratelimit-remaining")?.to_str().ok()? != "0" {
                return None;
            }
            let reset = headers
                .get("x-ratelimit-reset")?
                .to_str()
                .ok()?
                .parse::<u64>()
                .ok()?;
            Some(
                reset
                    .saturating_sub(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs())
                    .saturating_add(1),
            )
        })
}
fn api_error(r: &Reply) -> Error {
    match r.status.as_u16() {
        404 => Error::NotFound,
        401 => Error::Permission,
        403 if delay(&r.headers).is_none() => Error::Permission,
        409 | 422 => {
            Error::Conflict("GitHub rejected stale ref, duplicate or invalid mutation".into())
        }
        status => Error::Api {
            status,
            retry_after: delay(&r.headers),
        },
    }
}
pub(crate) fn segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
pub(crate) fn str_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| Error::Invalid(format!("GitHub response missing {key}")))
}
pub(crate) fn number(value: &Value, key: &str) -> Result<u64> {
    value[key]
        .as_u64()
        .ok_or_else(|| Error::Invalid(format!("GitHub response missing {key}")))
}
