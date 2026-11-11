#![cfg(feature = "github")]
use hadron_core::{
    github::{Context, Github, Options, Repository},
    validation::Limits,
    workflow::Hadron,
    *,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use wiremock::{
    Mock, MockServer, Request as HttpRequest, Respond, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

fn pack() -> Pack {
    Pack {
        schema_version: 1,
        id: "test".into(),
        name: "Test".into(),
        description: String::new(),
        game: "minecraft".into(),
        game_version: "1.21.1".into(),
        loader: "fabric".into(),
        loader_version: "0.16.10".into(),
    }
}
fn entry() -> ModEntry {
    ModEntry {
        name: "Sodium".into(),
        file: FileSpec {
            path: "mods/sodium.jar".into(),
            size: 3,
            sha256: Hash::of(b"jar"),
        },
        source: Source::Modrinth {
            project: "sodium".into(),
            version: "exact".into(),
        },
        client: Requirement::Required,
        server: Requirement::Unsupported,
    }
}
fn repo() -> Repository {
    serde_json::from_value(json!({"id":1,"full_name":"owner/pack","default_branch":"main","permissions":{"push":true}})).unwrap()
}
fn sha(c: char) -> String {
    c.to_string().repeat(40)
}
fn request() -> Request {
    Request {
        id: "sodium-request".into(),
        base_commit: sha('a'),
        base_publication: Some("initial".into()),
        reason: "Please add Sodium".into(),
        operation: Operation::Add {
            key: "sodium".into(),
            entry: entry(),
        },
    }
}
fn client(server: &MockServer) -> Github {
    let octo = octocrab::Octocrab::builder()
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    Github::new(
        octo,
        Options {
            read_retries: 0,
            ..Options::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn bounded_auth_errors_retry_and_cancellation() {
    let server = MockServer::start().await;
    let g = client(&server);
    let ctx = Context::default();
    Mock::given(method("GET"))
        .and(path("/repositories/1"))
        .respond_with(ResponseTemplate::new(401).set_body_string("secret"))
        .mount(&server)
        .await;
    assert!(matches!(
        g.repository(1, &ctx).await,
        Err(Error::Permission)
    ));
    ctx.cancellation.cancel();
    assert!(matches!(g.repository(1, &ctx).await, Err(Error::Cancelled)));
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(200)))
        .mount(&server)
        .await;
    let octo = octocrab::Octocrab::builder()
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    let small = Github::new(
        octo,
        Options {
            limits: Limits {
                json_bytes: 32,
                ..Limits::default()
            },
            read_retries: 0,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(matches!(
        small.repository(1, &Context::default()).await,
        Err(Error::Limit(_))
    ));
    server.reset().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("GET"))
        .respond_with(move |_: &HttpRequest| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(503).insert_header("retry-after", "0")
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":1,"full_name":"owner/pack","default_branch":"main"}))
            }
        })
        .mount(&server)
        .await;
    let octo = octocrab::Octocrab::builder()
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    Github::new(
        octo,
        Options {
            read_retries: 1,
            ..Options::default()
        },
    )
    .unwrap()
    .repository(1, &Context::default())
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
        .mount(&server)
        .await;
    let octo = octocrab::Octocrab::builder()
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    let timed = Github::new(
        octo,
        Options {
            timeout: Duration::from_millis(10),
            read_retries: 0,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(matches!(
        timed.repository(1, &Context::default()).await,
        Err(Error::Network)
    ));
}
#[tokio::test]
async fn redirect_never_sends_auth_to_untrusted_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repositories/1/releases/assets/5"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://evil.example/steal"),
        )
        .mount(&server)
        .await;
    let octo = octocrab::Octocrab::builder()
        .base_uri(server.uri())
        .unwrap()
        .personal_token("fake-token")
        .build()
        .unwrap();
    let g = Github::new(octo, Options::default()).unwrap();
    let bytes = b"x";
    let asset = Asset {
        id: 5,
        name: "x".into(),
        size: 1,
        sha256: Hash::of(bytes),
        decoded_size: 1,
        decoded_sha256: Hash::of(bytes),
    };
    assert!(matches!(
        g.asset(&repo(), &asset, &Context::default()).await,
        Err(Error::Invalid(_))
    ));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.contains_key("authorization"));
}
#[tokio::test]
async fn conditional_pointer_and_safe_pagination() {
    let server = MockServer::start().await;
    let g = client(&server);
    let h = Hadron::new(g.clone());
    let a = Asset {
        id: 1,
        name: "publication.json".into(),
        size: 1,
        sha256: Hash::of(b"x"),
        decoded_size: 1,
        decoded_sha256: Hash::of(b"x"),
    };
    let latest = Latest {
        schema_version: 1,
        publication_id: "v1".into(),
        index: a,
    };
    Mock::given(method("GET"))
        .and(path("/repositories/1/contents/.hadron/latest.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"one\"")
                .set_body_json(&latest),
        )
        .mount(&server)
        .await;
    assert_eq!(
        g.latest(&repo(), &Context::default()).await.unwrap(),
        Some(latest.clone())
    );
    server.reset().await;
    Mock::given(method("GET"))
        .and(header("if-none-match", "\"one\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;
    assert_eq!(
        g.latest(&repo(), &Context::default()).await.unwrap(),
        Some(latest)
    );
    server.reset().await;
    let values:Vec<Value>=(0..100).map(|i|json!({"number":i,"html_url":"https://example.com","head":{"ref":format!("hadron-request-r{i}")}})).collect();
    Mock::given(path("/repositories/1/pulls"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(values))
        .mount(&server)
        .await;
    Mock::given(path("/repositories/1/pulls"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<Value>::new()))
        .mount(&server)
        .await;
    assert_eq!(
        h.proposals(&repo(), &Context::default())
            .await
            .unwrap()
            .len(),
        100
    );
}

#[derive(Clone)]
struct Fake(Arc<Mutex<State>>);
struct State {
    head: String,
    files: BTreeMap<String, Vec<u8>>,
    snapshots: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    blobs: BTreeMap<String, Vec<u8>>,
    trees: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    branches: BTreeMap<String, String>,
    pr: Option<Value>,
    releases: BTreeMap<u64, Value>,
    assets: BTreeMap<u64, (String, Vec<u8>, u64)>,
    next: u64,
    fail_pointer: bool,
    fail_upload: bool,
    pointer_writes: usize,
    asset_reads: usize,
    push: bool,
    allow_fork: bool,
    unrelated_pr_file: bool,
}
impl State {
    fn new() -> Self {
        let files = BTreeMap::from([
            ("pack.json".into(), serde_json::to_vec(&pack()).unwrap()),
            (
                "mods.json".into(),
                serde_json::to_vec(&ModList::default()).unwrap(),
            ),
            ("config/test.toml".into(), b"# unchanged\nx=1\n".to_vec()),
            (
                "requests.json".into(),
                serde_json::to_vec(&Requests::default()).unwrap(),
            ),
        ]);
        Self {
            head: sha('a'),
            snapshots: BTreeMap::from([(sha('a'), files.clone())]),
            files,
            blobs: BTreeMap::new(),
            trees: BTreeMap::new(),
            branches: BTreeMap::new(),
            pr: None,
            releases: BTreeMap::new(),
            assets: BTreeMap::new(),
            next: 10,
            fail_pointer: false,
            fail_upload: false,
            pointer_writes: 0,
            asset_reads: 0,
            push: true,
            allow_fork: true,
            unrelated_pr_file: false,
        }
    }
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    fn new_sha(&mut self) -> String {
        format!("{:040x}", self.id())
    }
}
fn response(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(v)
}
impl Respond for Fake {
    fn respond(&self, r: &HttpRequest) -> ResponseTemplate {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let mut s = self.0.lock().unwrap();
        let original_path = r.url.path();
        let normalized = original_path.replace("/repositories/2", "/repositories/1");
        let path = normalized.as_str();
        let method = r.method.as_str();
        let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        let prefix = "/repositories/1";
        if original_path == "/repositories/2" || path == "/repos/requester/pack" {
            if !s.allow_fork {
                return ResponseTemplate::new(403);
            }
            return response(
                json!({"id":2,"full_name":"requester/pack","default_branch":"main","parent":{"id":1},"permissions":{"push":true}}),
            );
        }
        if path == "/user" {
            return response(json!({"login":"requester"}));
        }
        if path == prefix {
            return response(
                json!({"id":1,"full_name":"owner/pack","default_branch":"main","permissions":{"push":s.push}}),
            );
        }
        if path == format!("{prefix}/git/ref/heads/main") {
            return response(json!({"object":{"sha":s.head}}));
        }
        if let Some(branch) = path.strip_prefix(&format!("{prefix}/git/ref/heads/")) {
            return s
                .branches
                .get(branch)
                .map(|sha| response(json!({"object":{"sha":sha}})))
                .unwrap_or_else(|| ResponseTemplate::new(404));
        }
        if path == format!("{prefix}/git/refs") && method == "POST" {
            let b = body["ref"]
                .as_str()
                .unwrap()
                .strip_prefix("refs/heads/")
                .unwrap()
                .to_owned();
            let sha = body["sha"].as_str().unwrap().to_owned();
            s.branches.insert(b, sha.clone());
            return response(json!({"object":{"sha":sha}}));
        }
        if let Some(file) = path.strip_prefix(&format!("{prefix}/contents/")) {
            let reference = r
                .url
                .query_pairs()
                .find(|(k, _)| k == "ref")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_else(|| s.head.clone());
            let snapshot = if reference == "main" {
                Some(&s.files)
            } else {
                s.snapshots.get(&reference)
            };
            return snapshot
                .and_then(|f| f.get(file))
                .map(|b| ResponseTemplate::new(200).set_body_bytes(b.clone()))
                .unwrap_or_else(|| ResponseTemplate::new(404));
        }
        if path.starts_with(&format!("{prefix}/git/commits/")) {
            return response(json!({"tree":{"sha":"base-tree"}}));
        }
        if path == format!("{prefix}/git/trees/base-tree") {
            let tree: Vec<Value> = s
                .files
                .iter()
                .map(|(p, b)| json!({"path":p,"type":"blob","mode":"100644","size":b.len()}))
                .collect();
            return response(json!({"truncated":false,"tree":tree}));
        }
        if path == format!("{prefix}/git/blobs") && method == "POST" {
            let id = s.new_sha();
            s.blobs.insert(
                id.clone(),
                STANDARD.decode(body["content"].as_str().unwrap()).unwrap(),
            );
            return response(json!({"sha":id}));
        }
        if path == format!("{prefix}/git/trees") && method == "POST" {
            let mut tree = s.files.clone();
            for item in body["tree"].as_array().unwrap() {
                tree.insert(
                    item["path"].as_str().unwrap().into(),
                    s.blobs[item["sha"].as_str().unwrap()].clone(),
                );
            }
            let id = s.new_sha();
            s.trees.insert(id.clone(), tree);
            return response(json!({"sha":id}));
        }
        if path == format!("{prefix}/git/commits") && method == "POST" {
            let id = s.new_sha();
            let tree = s.trees[body["tree"].as_str().unwrap()].clone();
            s.snapshots.insert(id.clone(), tree);
            return response(json!({"sha":id}));
        }
        if let Some(branch) = path.strip_prefix(&format!("{prefix}/git/refs/heads/")) {
            let sha = body["sha"].as_str().unwrap().to_owned();
            if branch == "main" {
                let tree = s.snapshots[&sha].clone();
                if tree.get(".hadron/latest.json") != s.files.get(".hadron/latest.json") {
                    if s.fail_pointer {
                        return ResponseTemplate::new(409);
                    }
                    s.pointer_writes += 1;
                }
                s.head = sha;
                s.files = tree;
            } else {
                s.branches.insert(branch.into(), sha);
            }
            return response(json!({"ok":true}));
        }
        if path == format!("{prefix}/pulls") {
            if method == "GET" {
                return response(Value::Array(s.pr.iter().cloned().collect()));
            }
            let branch = body["head"].as_str().unwrap().split(':').nth(1).unwrap();
            let head = s.branches[branch].clone();
            let pr = json!({"number":1,"html_url":"https://github.com/owner/pack/pull/1","merged":false,"base":{"sha":s.head,"ref":"main","repo":{"id":1}},"head":{"sha":head,"ref":branch,"repo":{"id":1}}});
            s.pr = Some(pr.clone());
            return response(pr);
        }
        if path == format!("{prefix}/pulls/1") {
            if method == "PATCH" {
                return response(body);
            }
            return s
                .pr
                .clone()
                .map(response)
                .unwrap_or_else(|| ResponseTemplate::new(404));
        }
        if path == format!("{prefix}/pulls/1/files") {
            if s.unrelated_pr_file {
                return response(json!([{"filename":"mods.json","status":"modified"}]));
            }
            return response(json!([{"filename":"requests.json","status":"modified"}]));
        }
        if path == format!("{prefix}/pulls/1/merge") {
            let mut pr = s.pr.clone().unwrap();
            if body["sha"] != pr["head"]["sha"] {
                return ResponseTemplate::new(409);
            }
            let tree = s.snapshots[pr["head"]["sha"].as_str().unwrap()].clone();
            let id = s.new_sha();
            s.snapshots.insert(id.clone(), tree.clone());
            s.files = tree;
            s.head = id;
            pr["merged"] = true.into();
            s.pr = Some(pr);
            return response(json!({"merged":true}));
        }
        if let Some(tag) = path.strip_prefix(&format!("{prefix}/releases/tags/")) {
            return s
                .releases
                .values()
                .find(|v| v["tag_name"] == tag)
                .cloned()
                .map(response)
                .unwrap_or_else(|| ResponseTemplate::new(404));
        }
        if path == format!("{prefix}/releases") && method == "POST" {
            let id = s.id();
            let mut release = body;
            release["id"] = id.into();
            release["upload_url"] = format!(
                "https://uploads.github.com/repos/owner/pack/releases/{id}/assets{{?name,label}}"
            )
            .into();
            s.releases.insert(id, release.clone());
            return response(release);
        }
        if let Some(id) = path.strip_prefix(&format!("{prefix}/releases/assets/")) {
            let id = id.parse::<u64>().unwrap();
            s.asset_reads += 1;
            return s
                .assets
                .get(&id)
                .map(|(_, b, _)| ResponseTemplate::new(200).set_body_bytes(b.clone()))
                .unwrap_or_else(|| ResponseTemplate::new(404));
        }
        if path.starts_with("/repos/owner/pack/releases/") && method == "POST" {
            if s.fail_upload {
                return ResponseTemplate::new(502);
            }
            let release = path.split('/').nth(5).unwrap().parse::<u64>().unwrap();
            let name = r
                .url
                .query_pairs()
                .find(|(k, _)| k == "name")
                .unwrap()
                .1
                .into_owned();
            let id = s.id();
            s.assets.insert(id, (name.clone(), r.body.clone(), release));
            return response(json!({"id":id,"name":name,"size":r.body.len()}));
        }
        if let Some(suffix) = path.strip_prefix(&format!("{prefix}/releases/")) {
            if let Some(id) = suffix
                .strip_suffix("/assets")
                .and_then(|v| v.parse::<u64>().ok())
            {
                let a:Vec<Value>=s.assets.iter().filter(|(_,(_,_,rel))|*rel==id).map(|(id,(name,b,_))|json!({"id":id,"name":name,"size":b.len(),"digest":format!("sha256:{}",Hash::of(b).as_str())})).collect();
                return response(Value::Array(a));
            }
            if method == "PATCH" {
                let id = suffix.parse::<u64>().unwrap();
                let mut v = s.releases[&id].clone();
                v["draft"] = false.into();
                s.releases.insert(id, v.clone());
                return response(v);
            }
        }
        ResponseTemplate::new(404)
    }
}
// Test transport maps even trusted upload-origin requests to the loopback fake.
// Production upload-origin validation remains enabled, with no test-only bypass.
fn full_client(server: &MockServer) -> Github {
    let root = server.uri();
    let http = reqwest::Client::new();
    let service = tower::service_fn(move |r: http::Request<octocrab::OctoBody>| {
        let root = root.clone();
        let http = http.clone();
        async move {
            use http_body_util::BodyExt;
            let (parts, body) = r.into_parts();
            let bytes = body.collect().await.unwrap().to_bytes();
            let uri = format!(
                "{}{}",
                root,
                parts
                    .uri
                    .path_and_query()
                    .map(|v| v.as_str())
                    .unwrap_or("/")
            );
            let response = http
                .request(parts.method, uri)
                .headers(parts.headers)
                .body(bytes)
                .send()
                .await?;
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = response.bytes().await?;
            let mut result = http::Response::new(http_body_util::Full::new(bytes));
            *result.status_mut() = status;
            *result.headers_mut() = headers;
            Ok::<_, reqwest::Error>(result)
        }
    });
    let crab = octocrab::OctocrabBuilder::new_empty()
        .with_service(service)
        .with_auth(octocrab::AuthState::None)
        .build()
        .unwrap();
    Github::new(
        crab,
        Options {
            read_retries: 0,
            ..Options::default()
        },
    )
    .unwrap()
}
#[tokio::test]
async fn owner_request_publish_incremental_rollback_and_resume() {
    let server = MockServer::start().await;
    let state = Arc::new(Mutex::new(State::new()));
    Mock::given(wiremock::matchers::any())
        .respond_with(Fake(state.clone()))
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let h = Hadron::new(full_client(&server));
    #[cfg(feature = "cache")]
    let h = h.with_cache(hadron_core::cache::Cache::new(directory.path()).unwrap());
    let _ = directory;
    let ctx = Context::default();
    let r = repo();
    let initial_head = h.github.head(&r, &ctx).await.unwrap();
    let initial = h
        .publish(&r, "initial", &initial_head, &initial_head, &ctx)
        .await
        .unwrap();
    let loaded = h.open(&share::encode(1).unwrap(), &ctx).await.unwrap();
    assert!(loaded.manifest.mods.is_empty());
    let reads = state.lock().unwrap().asset_reads;
    h.open(&share::encode(1).unwrap(), &ctx).await.unwrap();
    #[cfg(feature = "cache")]
    assert_eq!(state.lock().unwrap().asset_reads, reads);
    let _ = reads;
    let mut req = request();
    req.base_commit = h.github.head(&r, &ctx).await.unwrap();
    let p = h.submit(&r, req.clone(), &ctx).await.unwrap();
    assert_eq!(p.number, 1);
    assert_eq!(h.submit(&r, req, &ctx).await.unwrap().number, 1);
    let (_, reviewed) = h.inspect(&r, 1, &ctx).await.unwrap();
    state.lock().unwrap().unrelated_pr_file = true;
    assert!(h.inspect(&r, 1, &ctx).await.is_err());
    state.lock().unwrap().unrelated_pr_file = false;
    assert!(matches!(
        h.accept(&r, 1, &sha('f'), &ctx).await,
        Err(Error::Conflict(_))
    ));
    let applied = h.accept(&r, 1, &reviewed, &ctx).await.unwrap();
    assert!(matches!(
        h.reject(&r, 1, &ctx).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        h.reconcile(&r, 1, "sodium-request", &ctx).await.unwrap(),
        applied
    );
    state.lock().unwrap().fail_upload = true;
    assert!(
        h.publish(&r, "second", &applied, &applied, &ctx)
            .await
            .is_err()
    );
    assert_eq!(
        h.github.latest(&r, &ctx).await.unwrap(),
        Some(initial.latest.clone())
    );
    state.lock().unwrap().fail_upload = false;
    state.lock().unwrap().fail_pointer = true;
    assert!(matches!(
        h.publish(&r, "second", &applied, &applied, &ctx).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        h.github.latest(&r, &ctx).await.unwrap(),
        Some(initial.latest.clone())
    );
    state.lock().unwrap().fail_pointer = false;
    let second = h
        .publish(&r, "second", &applied, &applied, &ctx)
        .await
        .unwrap();
    assert_eq!(initial.publication.configs, second.publication.configs);
    let latest = h.open(&share::encode(1).unwrap(), &ctx).await.unwrap();
    assert_eq!(latest.manifest.mods.len(), 1);
    let old = h.historical(&r, "initial", &ctx).await.unwrap();
    assert_eq!(diff::between(&latest.manifest, &old.manifest).mods.len(), 1);
    let assets = state.lock().unwrap().assets.len();
    h.publish(&r, "second", &applied, &applied, &ctx)
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().assets.len(), assets);
    h.select(&r, &initial.latest, &second.pointer_commit, &ctx)
        .await
        .unwrap();
    assert!(
        h.open(&share::encode(1).unwrap(), &ctx)
            .await
            .unwrap()
            .manifest
            .mods
            .is_empty()
    );
}

#[tokio::test]
async fn fork_submission_and_private_fork_policy() {
    let server = MockServer::start().await;
    let mut initial = State::new();
    initial.push = false;
    let state = Arc::new(Mutex::new(initial));
    Mock::given(wiremock::matchers::any())
        .respond_with(Fake(state.clone()))
        .mount(&server)
        .await;
    let h = Hadron::new(full_client(&server));
    let ctx = Context::default();
    let r = h.github.repository(1, &ctx).await.unwrap();
    assert!(!r.permissions.push);
    assert_eq!(h.submit(&r, request(), &ctx).await.unwrap().number, 1);
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/repositories/2/git/refs")
    );
    state.lock().unwrap().pr = None;
    state.lock().unwrap().allow_fork = false;
    let mut req = request();
    req.id = "other".into();
    assert!(matches!(
        h.submit(&r, req, &ctx).await,
        Err(Error::Permission)
    ));
}

#[tokio::test]
async fn mutation_timeout_not_retried_and_stale_ref_not_forced() {
    let server = MockServer::start().await;
    let ctx = Context::default();
    let r = repo();
    Mock::given(method("GET"))
        .and(path(format!("/repositories/1/git/commits/{}", sha('a'))))
        .respond_with(response(json!({"tree":{"sha":"base"}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repositories/1/git/blobs"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_json(json!({"sha":sha('b')})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let crab = octocrab::Octocrab::builder()
        .base_uri(server.uri())
        .unwrap()
        .build()
        .unwrap();
    let g = Github::new(
        crab,
        Options {
            timeout: Duration::from_millis(30),
            read_retries: 2,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(matches!(
        g.commit_files(
            &r,
            "main",
            &sha('a'),
            &BTreeMap::from([("mods.json".into(), b"{}".to_vec())]),
            "test",
            &ctx
        )
        .await,
        Err(Error::Network)
    ));
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(response(json!({"tree":{"sha":"base"}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(response(json!({"sha":sha('b')})))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(wiremock::matchers::body_json(
            json!({"sha":sha('b'),"force":false}),
        ))
        .respond_with(ResponseTemplate::new(422))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server)
            .commit_files(
                &r,
                "main",
                &sha('a'),
                &BTreeMap::from([("mods.json".into(), b"{}".to_vec())]),
                "test",
                &ctx
            )
            .await,
        Err(Error::Conflict(_))
    ));
}
