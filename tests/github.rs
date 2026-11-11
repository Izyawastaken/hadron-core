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
    let (page, more) = h
        .proposals_page(&repo(), 1, &Context::default())
        .await
        .unwrap();
    assert_eq!(page.len(), 100);
    assert!(more);
    assert!(
        !h.proposals_page(&repo(), 2, &Context::default())
            .await
            .unwrap()
            .1
    );
}

#[derive(Clone)]
struct Fake(Arc<Mutex<State>>);
struct State {
    head: String,
    files: BTreeMap<String, Vec<u8>>,
    snapshots: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    parents: BTreeMap<String, Vec<String>>,
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
    truncated_tree: bool,
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
            parents: BTreeMap::new(),
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
            truncated_tree: false,
        }
    }
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    fn new_sha(&mut self) -> String {
        format!("{:040x}", self.id())
    }
    fn ancestor(&self, base: &str, head: &str) -> bool {
        if base == head {
            return true;
        }
        self.parents
            .get(head)
            .is_some_and(|parents| parents.iter().any(|p| self.ancestor(base, p)))
    }
    fn advance(&mut self) -> String {
        let id = self.new_sha();
        self.parents.insert(id.clone(), vec![self.head.clone()]);
        self.snapshots.insert(id.clone(), self.files.clone());
        self.head = id.clone();
        id
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
        if let Some(pair) = path.strip_prefix(&format!("{prefix}/compare/")) {
            let (base, head) = pair.split_once("...").unwrap();
            let ancestor = if s.ancestor(base, head) {
                base.to_owned()
            } else {
                s.snapshots
                    .keys()
                    .find(|c| s.ancestor(c, base) && s.ancestor(c, head))
                    .cloned()
                    .unwrap_or_else(|| sha('a'))
            };
            return response(json!({"merge_base_commit":{"sha":ancestor}}));
        }
        if let Some(commit) = path.strip_prefix(&format!("{prefix}/git/commits/")) {
            if !s.snapshots.contains_key(commit) {
                return ResponseTemplate::new(404);
            }
            let parents: Vec<Value> = s
                .parents
                .get(commit)
                .into_iter()
                .flatten()
                .map(|p| json!({"sha":p}))
                .collect();
            return response(json!({"tree":{"sha":commit},"parents":parents}));
        }
        if let Some(commit) = path.strip_prefix(&format!("{prefix}/git/trees/")) {
            let Some(files) = s.snapshots.get(commit) else {
                return ResponseTemplate::new(404);
            };
            let mut tree: Vec<Value> = files
                .iter()
                .map(|(p, b)| json!({"path":p,"type":"blob","mode":"100644","sha":&Hash::of(b).as_str()[..40],"size":b.len(),"url":format!("https://api.github.com{}?blob={p}",original_path)}))
                .collect();
            if s.unrelated_pr_file && s.pr.as_ref().is_some_and(|p| p["head"]["sha"] == commit) {
                tree.push(json!({"path":"extra.txt","type":"blob","mode":"100644","sha":sha('f'),"size":1}));
            }
            return response(json!({"truncated":s.truncated_tree,"tree":tree}));
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
            let mut tree = s.snapshots[body["base_tree"].as_str().unwrap()].clone();
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
            s.parents.insert(
                id.clone(),
                body["parents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p.as_str().unwrap().to_owned())
                    .collect(),
            );
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
            let mut tree = s.files.clone();
            tree.insert(
                "requests.json".into(),
                s.snapshots[pr["head"]["sha"].as_str().unwrap()]["requests.json"].clone(),
            );
            let id = s.new_sha();
            let parents = vec![
                s.head.clone(),
                pr["head"]["sha"].as_str().unwrap().to_owned(),
            ];
            s.parents.insert(id.clone(), parents);
            s.snapshots.insert(id.clone(), tree.clone());
            s.files = tree;
            s.head = id.clone();
            pr["merged"] = true.into();
            pr["merge_commit_sha"] = id.into();
            s.pr = Some(pr);
            return response(json!({"merged":true}));
        }
        if let Some(tag) = path.strip_prefix(&format!("{prefix}/releases/tags/")) {
            return s
                .releases
                .values()
                .find(|v| v["tag_name"] == tag && v["draft"] == false)
                .cloned()
                .map(response)
                .unwrap_or_else(|| ResponseTemplate::new(404));
        }
        if path == format!("{prefix}/releases") && method == "GET" {
            return response(Value::Array(s.releases.values().cloned().collect()));
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
    full_client_options(
        server,
        Options {
            read_retries: 0,
            ..Options::default()
        },
    )
}
fn full_client_options(server: &MockServer, options: Options) -> Github {
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
    Github::new(crab, options).unwrap()
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
    assert_eq!(
        h.proposals(&r, &ctx).await.unwrap()[0].state,
        hadron_core::workflow::ProposalState::Merged
    );
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
    state.lock().unwrap().pr.as_mut().unwrap()["head"]["repo"]["id"] = 2.into();
    h.inspect(&r, 1, &ctx).await.unwrap();
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

async fn fixture() -> (MockServer, Arc<Mutex<State>>, Hadron) {
    let server = MockServer::start().await;
    let state = Arc::new(Mutex::new(State::new()));
    Mock::given(wiremock::matchers::any())
        .respond_with(Fake(state.clone()))
        .mount(&server)
        .await;
    let hadron = Hadron::new(full_client(&server));
    (server, state, hadron)
}

#[tokio::test]
async fn immutable_review_and_merged_provenance_survive_branch_lifecycle() {
    let (server, state, h) = fixture().await;
    let ctx = Context::default();
    let r = repo();
    h.submit(&r, request(), &ctx).await.unwrap();
    assert!(
        h.archive_requests(&r, &["sodium-request".into()], &sha('a'), &ctx)
            .await
            .is_err()
    );
    // Base advances after submission, including target-only source changes.
    {
        let mut s = state.lock().unwrap();
        s.files
            .insert("target-only.txt".into(), b"owner edit".to_vec());
        let base = s.advance();
        s.pr.as_mut().unwrap()["base"]["sha"] = base.into();
    }
    let (_, reviewed) = h.inspect(&r, 1, &ctx).await.unwrap();
    state.lock().unwrap().unrelated_pr_file = true;
    assert!(h.inspect(&r, 1, &ctx).await.is_err());
    state.lock().unwrap().unrelated_pr_file = false;
    state.lock().unwrap().truncated_tree = true;
    assert!(matches!(h.inspect(&r, 1, &ctx).await, Err(Error::Limit(_))));
    state.lock().unwrap().truncated_tree = false;
    h.accept(&r, 1, &reviewed, &ctx).await.unwrap();
    {
        let mut s = state.lock().unwrap();
        let pr = s.pr.as_mut().unwrap();
        pr["head"]["repo"] = Value::Null;
        pr["head"]["sha"] = sha('f').into(); // branch moved after merge
    }
    assert_eq!(h.inspect(&r, 1, &ctx).await.unwrap().0, request());
    h.reconcile(&r, 1, "sodium-request", &ctx).await.unwrap();
    let head = h.github.head(&r, &ctx).await.unwrap();
    let archived = h
        .archive_requests(&r, &["sodium-request".into()], &head, &ctx)
        .await
        .unwrap();
    let ledger: Requests = serde_json::from_slice(
        &h.github
            .file(&r, "requests.json", &archived, &ctx)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(ledger.requests.is_empty());
    assert_eq!(
        h.reconcile(&r, 1, "sodium-request", &ctx).await.unwrap(),
        archived
    );
    assert_eq!(h.submit(&r, request(), &ctx).await.unwrap().number, 1);
    let mut collision = request();
    collision.reason = "different data".into();
    assert!(matches!(
        h.submit(&r, collision, &ctx).await,
        Err(Error::Conflict(_))
    ));
    let traffic = server.received_requests().await.unwrap();
    assert!(
        !traffic
            .iter()
            .any(|r| r.url.path().ends_with("/pulls/1/files"))
    );
    assert!(
        traffic
            .iter()
            .filter(|r| r.url.path().ends_with("/pulls") && r.method == "GET")
            .all(|r| r.url.query_pairs().any(|(key, _)| key == "head"))
    );
}

#[tokio::test]
async fn publication_recovers_broken_prior_and_resumes_original_index_after_selection_change() {
    let (server, state, h) = fixture().await;
    let ctx = Context::default();
    let r = repo();
    let source = sha('a');
    let a = h.publish(&r, "a", &source, &source, &ctx).await.unwrap();
    let old_config = a.publication.configs.clone().unwrap();
    let removed = state.lock().unwrap().assets.remove(&old_config.id).unwrap();
    let head = h.github.head(&r, &ctx).await.unwrap();
    let b = h.publish(&r, "b", &source, &head, &ctx).await.unwrap();
    assert_ne!(a.publication.configs, b.publication.configs);
    state.lock().unwrap().assets.insert(old_config.id, removed);
    h.select(&r, &a.latest, &b.pointer_commit, &ctx)
        .await
        .unwrap();
    let head = h.github.head(&r, &ctx).await.unwrap();
    state.lock().unwrap().fail_pointer = true;
    assert!(h.publish(&r, "p", &source, &head, &ctx).await.is_err());
    state.lock().unwrap().fail_pointer = false;
    let (index_id, index_bytes) = {
        let s = state.lock().unwrap();
        let p = s
            .releases
            .values()
            .find(|v| v["tag_name"] == "hadron-p")
            .unwrap()["id"]
            .as_u64()
            .unwrap();
        let (id, (_, bytes, _)) = s
            .assets
            .iter()
            .find(|(_, (name, _, rel))| name == "publication.json" && *rel == p)
            .unwrap();
        (*id, bytes.clone())
    };
    h.select(&r, &b.latest, &head, &ctx).await.unwrap();
    let head = h.github.head(&r, &ctx).await.unwrap();
    let p = h.publish(&r, "p", &source, &head, &ctx).await.unwrap();
    assert_eq!(p.latest.index.id, index_id);
    assert_eq!(serde_json::to_vec(&p.publication).unwrap(), index_bytes);
    assert_eq!(p.publication.configs, a.publication.configs);
    // A new mod-only publication fetches no config source and uploads no config.
    let before = server.received_requests().await.unwrap().len();
    let q = h
        .publish(&r, "q", &source, &p.pointer_commit, &ctx)
        .await
        .unwrap();
    assert_eq!(p.publication.configs, q.publication.configs);
    let traffic = server.received_requests().await.unwrap();
    assert!(
        !traffic[before..]
            .iter()
            .any(|r| r.url.path().contains("/contents/config/"))
    );
    assert!(!traffic[before..].iter().any(|r| {
        r.method == "POST"
            && r.url
                .query_pairs()
                .any(|(k, v)| k == "name" && v.starts_with("configs-"))
    }));
}

#[tokio::test]
async fn config_fetch_and_blob_envelope_have_independent_budgets() {
    let (server, state, _) = fixture().await;
    {
        let mut s = state.lock().unwrap();
        s.files.insert("config/test.toml".into(), vec![b'x'; 3000]);
        let files = s.files.clone();
        s.snapshots.insert(sha('a'), files);
    }
    let g = full_client_options(
        &server,
        Options {
            limits: Limits {
                json_bytes: 2048,
                file_bytes: 4096,
                ..Limits::default()
            },
            read_retries: 0,
            ..Options::default()
        },
    );
    let h = Hadron::new(g.clone());
    let ctx = Context::default();
    let snapshot = h.prepare(&repo(), &sha('a'), &ctx).await.unwrap();
    assert_eq!(
        snapshot.prepared.manifest.configs["config/test.toml"].size,
        3000
    );
    assert!(matches!(
        g.file(&repo(), "config/test.toml", &sha('a'), &ctx).await,
        Err(Error::Limit(_))
    ));
    g.commit_files(
        &repo(),
        "main",
        &sha('a'),
        &BTreeMap::from([("large.json".into(), vec![b' '; 2048])]),
        "boundary",
        &ctx,
    )
    .await
    .unwrap();
    assert!(matches!(
        g.commit_files(
            &repo(),
            "main",
            &sha('a'),
            &BTreeMap::from([("large.json".into(), vec![b' '; 2049])]),
            "over",
            &ctx
        )
        .await,
        Err(Error::Limit(_))
    ));
}

#[tokio::test]
async fn partial_config_edit_fetches_only_changed_source_files() {
    let (server, state, h) = fixture().await;
    let ctx = Context::default();
    let r = repo();
    {
        let mut s = state.lock().unwrap();
        s.files.insert("config/other.toml".into(), b"x=2".to_vec());
        let files = s.files.clone();
        s.snapshots.insert(sha('a'), files);
    }
    h.publish(&r, "a", &sha('a'), &sha('a'), &ctx)
        .await
        .unwrap();
    let next = {
        let mut s = state.lock().unwrap();
        s.files.insert("config/other.toml".into(), b"x=3".to_vec());
        s.advance()
    };
    let before = server.received_requests().await.unwrap().len();
    let p = h.publish(&r, "b", &next, &next, &ctx).await.unwrap();
    let loaded = h.load(&r, &p.latest, &ctx).await.unwrap();
    assert_eq!(loaded.configs["config/other.toml"], b"x=3");
    let traffic = server.received_requests().await.unwrap();
    let configs: Vec<_> = traffic[before..]
        .iter()
        .filter(|r| r.url.path().contains("/contents/config/"))
        .collect();
    assert_eq!(configs.len(), 1);
    assert!(configs[0].url.path().ends_with("/other.toml"));
}

#[tokio::test]
async fn publication_can_replace_a_malformed_pointer_with_reviewed_head() {
    let (_server, state, h) = fixture().await;
    let ctx = Context::default();
    let r = repo();
    h.publish(&r, "a", &sha('a'), &sha('a'), &ctx)
        .await
        .unwrap();
    let broken = {
        let mut s = state.lock().unwrap();
        s.files
            .insert(".hadron/latest.json".into(), b"invalid JSON".to_vec());
        s.advance()
    };
    let p = h.publish(&r, "b", &sha('a'), &broken, &ctx).await.unwrap();
    assert_eq!(
        h.open(&share::encode(1).unwrap(), &ctx)
            .await
            .unwrap()
            .publication,
        p.publication
    );
}

#[cfg(feature = "cache")]
#[tokio::test]
async fn recover_cache_refetches_corruption_but_never_accepts_bad_remote_bytes() {
    use hadron_core::cache::{Cache, CachePolicy};
    let (_server, state, h) = fixture().await;
    let ctx = Context::default();
    let r = repo();
    let published = h
        .publish(&r, "a", &sha('a'), &sha('a'), &ctx)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cache = Cache::new(directory.path()).unwrap();
    let h = h.with_cache(cache.clone());
    h.load(&r, &published.latest, &ctx).await.unwrap();
    let key = &published.publication.manifest.sha256;
    std::fs::write(directory.path().join(key.as_str()), b"bad").unwrap();
    assert!(matches!(
        h.load(&r, &published.latest, &ctx).await,
        Err(Error::Integrity)
    ));
    let h = h.with_cache_policy(CachePolicy::Recover);
    h.load(&r, &published.latest, &ctx).await.unwrap();
    cache.remove(key).unwrap();
    state
        .lock()
        .unwrap()
        .assets
        .get_mut(&published.publication.manifest.id)
        .unwrap()
        .1 = b"bad".to_vec();
    assert!(h.load(&r, &published.latest, &ctx).await.is_err());
}

#[tokio::test]
async fn workflow_admission_is_bounded_and_cancellable() {
    let (server, _state, h) = fixture().await;
    Mock::given(path("/repositories/1/contents/pack.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(250))
                .set_body_json(pack()),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let first = h.clone();
    let first =
        tokio::spawn(async move { first.prepare(&repo(), &sha('a'), &Context::default()).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path().ends_with("/pack.json"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    let ctx = Context::default();
    let r = repo();
    let commit = sha('a');
    let second = h.prepare(&r, &commit, &ctx);
    tokio::pin!(second);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err()
    );
    ctx.cancellation.cancel();
    assert!(matches!(second.await, Err(Error::Cancelled)));
    first.await.unwrap().unwrap();
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/pack.json"))
            .count(),
        1
    );
}

#[cfg(feature = "cache")]
#[tokio::test]
async fn cache_write_failure_is_optional_only_in_recover_mode() {
    use hadron_core::cache::{Cache, CachePolicy};
    let (_server, _state, h) = fixture().await;
    let ctx = Context::default();
    let r = repo();
    let p = h
        .publish(&r, "a", &sha('a'), &sha('a'), &ctx)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("cache");
    let cache = Cache::new(&root).unwrap();
    std::fs::rename(&root, dir.path().join("moved-cache")).unwrap();
    let h = h.with_cache(cache);
    assert!(matches!(
        h.load(&r, &p.latest, &ctx).await,
        Err(Error::Io(_))
    ));
    h.with_cache_policy(CachePolicy::Recover)
        .load(&r, &p.latest, &ctx)
        .await
        .unwrap();
}

#[tokio::test]
async fn pointer_metadata_has_a_bound_and_explicit_invalidation() {
    let server = MockServer::start().await;
    let pointer = Latest {
        schema_version: 1,
        publication_id: "a".into(),
        index: Asset {
            id: 1,
            name: "publication.json".into(),
            size: 1,
            sha256: Hash::of(b"x"),
            decoded_size: 1,
            decoded_sha256: Hash::of(b"x"),
        },
    };
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"test\"")
                .set_body_json(pointer),
        )
        .mount(&server)
        .await;
    let g = full_client(&server);
    let ctx = Context::default();
    for id in 1..=129 {
        let mut r = repo();
        r.id = id;
        g.latest(&r, &ctx).await.unwrap();
    }
    g.latest(&repo(), &ctx).await.unwrap(); // First entry was evicted.
    let mut r = repo();
    r.id = 129;
    g.latest(&r, &ctx).await.unwrap();
    g.clear_metadata(Some(129)).unwrap();
    g.latest(&r, &ctx).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert!(!requests[129].headers.contains_key("if-none-match"));
    assert!(requests[130].headers.contains_key("if-none-match"));
    assert!(!requests[131].headers.contains_key("if-none-match"));
    g.clear_metadata(None).unwrap();
}
