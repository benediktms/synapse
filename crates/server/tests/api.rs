use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use adapters_fastembed::{DIMENSION, FastEmbedder, MODEL_NAME};
use adapters_sqlite::SqliteStore;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use domain::{Embedder, Error, Memory, MemoryId, MemoryKind, Scope, Store, Timestamp};
use serde_json::{Value, json};
use server::{App, REEMBED_TARGET_FILE};
use tempfile::TempDir;
use tokio::sync::OnceCell;
use tower::ServiceExt;

const TOKEN: &str = "test-token";

static EMBEDDER: OnceCell<Arc<FastEmbedder>> = OnceCell::const_new();

async fn embedder() -> Arc<FastEmbedder> {
    EMBEDDER
        .get_or_init(|| async {
            tokio::task::spawn_blocking(|| Arc::new(FastEmbedder::new().expect("model init")))
                .await
                .expect("join")
        })
        .await
        .clone()
}

async fn boot(dir: &Path) -> (Router, App) {
    let app = App::boot(dir.to_path_buf(), embedder().await)
        .await
        .expect("boot");
    (api::router(app.clone(), TOKEN), app)
}

async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, value)
}

async fn req(
    router: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    send(router, method, uri, body, Some(TOKEN)).await
}

fn mid(n: u32) -> String {
    format!("m_{n:022}")
}

fn put_body(content: &str) -> Value {
    json!({ "content": content, "kind": "project", "scope": "workspace", "tags": [] })
}

async fn put_memory(router: &Router, ws: &str, n: u32, content: &str) -> (StatusCode, Value) {
    req(
        router,
        Method::PUT,
        &format!("/memories/{}?ws={ws}", mid(n)),
        Some(put_body(content)),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn health_workspaces_and_name_validation() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;

    let (status, body) = send(&router, Method::GET, "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");

    let (status, body) = req(&router, Method::GET, "/workspaces", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["workspaces"], json!(["shared"]));

    let (status, _) = req(&router, Method::PUT, "/workspaces/work", None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = req(&router, Method::PUT, "/workspaces/work", None).await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = req(&router, Method::GET, "/workspaces", None).await;
    assert_eq!(body["workspaces"], json!(["shared", "work"]));

    for bad in ["Work", "wo_rk", "shared", &"a".repeat(33)] {
        let (status, _) = req(&router, Method::PUT, &format!("/workspaces/{bad}"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {bad:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_is_enforced_except_health() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;

    let (status, _) = send(&router, Method::GET, "/workspaces", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(&router, Method::GET, "/workspaces", None, Some("wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(&router, Method::GET, "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn put_memory_is_idempotent_and_conflicts_on_mismatch() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;

    let (status, body) = put_memory(&router, "work", 1, "rebase feature branches onto main").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["id"], mid(1));

    let (status, body) = put_memory(&router, "work", 1, "rebase feature branches onto main").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], mid(1));

    let (status, _) = put_memory(&router, "work", 1, "different content entirely").await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = put_memory(&router, "nope", 2, "no such workspace").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn validation_limits_reject_clearly() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;

    let uri = format!("/memories/{}?ws=work", mid(1));
    let cases = [
        put_body(&"x".repeat(8 * 1024 + 1)),
        put_body(&"ab ".repeat(2000)),
        put_body(""),
        json!({ "content": "ok", "kind": "note", "scope": "workspace", "tags": [] }),
        json!({ "content": "ok", "kind": "project", "scope": "has space", "tags": [] }),
        json!({ "content": "ok", "kind": "project", "scope": "workspace",
                "tags": (0..17).map(|i| i.to_string()).collect::<Vec<_>>() }),
        json!({ "content": "ok", "kind": "project", "scope": "workspace", "tags": ["bad tag"] }),
    ];
    for (i, body) in cases.into_iter().enumerate() {
        let (status, response) = req(&router, Method::PUT, &uri, Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "case {i}: {response}");
    }

    let (status, _) = req(
        &router,
        Method::PUT,
        "/memories/not-an-id?ws=work",
        Some(put_body("ok")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let long_query = "q".repeat(1025);
    for uri in [
        format!("/memories/search?ws=work&q={long_query}"),
        "/memories/search?ws=work&q=ok&limit=0".to_string(),
        "/memories/search?ws=work&q=ok&limit=21".to_string(),
        "/memories/search?ws=work".to_string(),
        "/memories/search?q=ok".to_string(),
    ] {
        let (status, response) = req(&router, Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "uri {uri}: {response}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn search_fuses_and_respects_workspace_isolation() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    req(&router, Method::PUT, "/workspaces/personal", None).await;

    put_memory(&router, "work", 1, "argocd deploy to staging for offers").await;
    put_memory(
        &router,
        "work",
        2,
        "std::collections::HashMap usage patterns",
    )
    .await;
    put_memory(&router, "personal", 3, "personal home server deploy notes").await;
    put_memory(&router, "shared", 4, "prefers oat milk in coffee").await;

    let (status, body) = req(
        &router,
        Method::GET,
        "/memories/search?ws=work&q=deploy%20to%20staging",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().expect("hits");
    assert!(
        hits.iter().any(|h| h["id"] == mid(1)),
        "work deploy memory missing: {body}"
    );
    assert!(
        hits.iter().all(|h| h["workspace"] != "personal"),
        "personal leaked into work search: {body}"
    );
    assert!(
        hits.iter().all(|h| h["id"] != mid(4)),
        "unrelated shared memory surfaced: {body}"
    );

    let (status, body) = req(
        &router,
        Method::GET,
        "/memories/search?ws=work&q=HashMap",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == mid(2)),
        "keyword leg missed code-shaped query: {body}"
    );

    let (status, body) = req(
        &router,
        Method::GET,
        "/memories/search?q=deploy&all=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let groups = body["groups"].as_array().expect("groups");
    let names: Vec<&str> = groups
        .iter()
        .map(|g| g["workspace"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"work") && names.contains(&"personal"),
        "{body}"
    );
    assert!(
        names.iter().filter(|n| **n == "shared").count() <= 1,
        "shared emitted more than once: {body}"
    );
    let personal_group = groups
        .iter()
        .find(|g| g["workspace"] == "personal")
        .unwrap();
    assert!(
        personal_group["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["id"] == mid(3)),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_on_shared_workspace_queries_once() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    put_memory(&router, "shared", 1, "always use ripgrep for code search").await;

    let (status, body) = req(
        &router,
        Method::GET,
        "/memories/search?ws=shared&q=ripgrep%20code%20search",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().unwrap();
    assert_eq!(
        hits.iter().filter(|h| h["id"] == mid(1)).count(),
        1,
        "shared hit duplicated or missing: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn context_digest_sections() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;

    put_memory(&router, "work", 1, "pinned architectural decision").await;
    req(
        &router,
        Method::PATCH,
        &format!("/memories/{}?ws=work", mid(1)),
        Some(json!({ "pinned": true })),
    )
    .await;
    req(
        &router,
        Method::PUT,
        &format!("/memories/{}?ws=work", mid(2)),
        Some(
            json!({ "content": "offers uses the outbox pattern", "kind": "project",
                     "scope": "fresha/offers", "tags": [] }),
        ),
    )
    .await;
    put_memory(&router, "work", 3, "workspace wide convention").await;
    req(
        &router,
        Method::PUT,
        &format!("/memories/{}?ws=shared", mid(4)),
        Some(
            json!({ "content": "prefers tables over prose", "kind": "user",
                     "scope": "workspace", "tags": [] }),
        ),
    )
    .await;

    let (status, body) = req(
        &router,
        Method::GET,
        "/context?ws=work&project=fresha/offers",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids = |section: &str| -> Vec<String> {
        body[section]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids("pinned"), vec![mid(1)]);
    assert_eq!(ids("recent_project"), vec![mid(2)]);
    assert_eq!(ids("shared_user"), vec![mid(4)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn patch_reembeds_and_delete_removes() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    put_memory(&router, "work", 1, "the cat sat on the mat").await;

    let (status, body) = req(
        &router,
        Method::PATCH,
        &format!("/memories/{}?ws=work", mid(1)),
        Some(json!({ "content": "kubernetes ingress configuration guide" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"], "kubernetes ingress configuration guide");

    let (_, body) = req(
        &router,
        Method::GET,
        "/memories/search?ws=work&q=kubernetes%20ingress",
        None,
    )
    .await;
    assert!(
        body["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == mid(1)),
        "edited content not searchable: {body}"
    );

    let (status, _) = req(
        &router,
        Method::PATCH,
        &format!("/memories/{}?ws=work", mid(9)),
        Some(json!({ "pinned": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = req(
        &router,
        Method::DELETE,
        &format!("/memories/{}?ws=work", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = req(
        &router,
        Method::GET,
        &format!("/memories/{}?ws=work", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, body) = req(
        &router,
        Method::GET,
        "/memories/search?ws=work&q=kubernetes%20ingress",
        None,
    )
    .await;
    assert!(
        body["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["id"] != mid(1)),
        "deleted memory still searchable: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn export_import_round_trip_and_merge_idempotency() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    req(&router, Method::PUT, "/workspaces/restore", None).await;
    put_memory(&router, "work", 1, "first exported fact").await;
    put_memory(&router, "work", 2, "second exported fact").await;
    req(
        &router,
        Method::PATCH,
        &format!("/memories/{}?ws=work", mid(1)),
        Some(json!({ "pinned": true })),
    )
    .await;

    let (status, export) = req(&router, Method::GET, "/export?ws=work", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(export["version"], 1);
    assert_eq!(export["memories"].as_array().unwrap().len(), 2);

    let (status, report) = req(
        &router,
        Method::POST,
        "/import?ws=restore",
        Some(export.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["imported"], 2);
    assert_eq!(report["unchanged"], 0);

    let (_, restored) = req(&router, Method::GET, "/export?ws=restore", None).await;
    assert_eq!(
        export["memories"], restored["memories"],
        "round trip drifted"
    );

    let (status, _) = req(
        &router,
        Method::POST,
        "/import?ws=restore",
        Some(export.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, report) = req(
        &router,
        Method::POST,
        "/import?ws=restore&mode=merge",
        Some(export.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["imported"], 0);
    assert_eq!(report["unchanged"], 2);

    let mut conflicting = export.clone();
    conflicting["memories"][0]["content"] = json!("mutated content");
    let (status, _) = req(
        &router,
        Method::POST,
        "/import?ws=restore&mode=merge",
        Some(conflicting),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (_, searched) = req(
        &router,
        Method::GET,
        "/memories/search?ws=restore&q=second%20exported%20fact",
        None,
    )
    .await;
    assert!(
        searched["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == mid(2)),
        "imported memory not recallable: {searched}"
    );
}

struct ScriptedEmbedder {
    vector: Vec<f32>,
    fail_after: Option<usize>,
    calls: AtomicUsize,
}

impl ScriptedEmbedder {
    fn ok(vector: Vec<f32>) -> Self {
        Self {
            vector,
            fail_after: None,
            calls: AtomicUsize::new(0),
        }
    }

    fn failing_after(vector: Vec<f32>, fail_after: usize) -> Self {
        Self {
            vector,
            fail_after: Some(fail_after),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Embedder for ScriptedEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, Error> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_after.is_some_and(|limit| call >= limit) {
            return Err(Error::Embed("scripted failure".into()));
        }
        Ok(self.vector.clone())
    }
}

fn memory(n: u32, content: &str) -> Memory {
    Memory {
        id: MemoryId::parse(&mid(n)).unwrap(),
        content: content.to_string(),
        kind: MemoryKind::Project,
        scope: Scope::Workspace,
        tags: Vec::new(),
        pinned: false,
        created_at: Timestamp::new("2026-08-01T00:00:00Z"),
        updated_at: Timestamp::new("2026-08-01T00:00:00Z"),
    }
}

async fn seed_old_model_db(path: &Path, model: &str, dim: usize, rows: &[(u32, &str)]) {
    let store = SqliteStore::open(path, model, dim).await.unwrap();
    for (n, content) in rows {
        store
            .insert(&memory(*n, content), &vec![0.5; dim])
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reembed_kill_resume_and_refuse_ready() {
    let dir = TempDir::new().unwrap();
    seed_old_model_db(
        &dir.path().join("aa.db"),
        "model-a",
        2,
        &[(1, "alpha fact")],
    )
    .await;
    seed_old_model_db(&dir.path().join("bb.db"), "model-a", 2, &[(2, "beta fact")]).await;

    let killer = ScriptedEmbedder::failing_after(vec![0.1, 0.2, 0.3], 1);
    let err = server::reembed(dir.path(), "model-b", 3, &killer)
        .await
        .expect_err("should die mid-run");
    assert!(err.contains("bb"), "unexpected error: {err}");
    assert!(dir.path().join(REEMBED_TARGET_FILE).exists());

    let aa = SqliteStore::open_maintenance(&dir.path().join("aa.db"))
        .await
        .unwrap();
    assert_eq!(
        aa.embedding_meta().await.unwrap(),
        ("model-b".to_string(), 3)
    );
    let bb = SqliteStore::open_maintenance(&dir.path().join("bb.db"))
        .await
        .unwrap();
    assert_eq!(
        bb.embedding_meta().await.unwrap(),
        ("model-a".to_string(), 2)
    );

    let err = server::reembed(
        dir.path(),
        "model-c",
        4,
        &ScriptedEmbedder::ok(vec![0.0; 4]),
    )
    .await
    .expect_err("different target must be refused mid-run");
    assert!(err.contains("in progress"), "unexpected error: {err}");

    let (router, _) = boot(dir.path()).await;
    let (status, body) = send(&router, Method::GET, "/health", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "unready");
    let (status, _) = req(&router, Method::GET, "/workspaces", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let report = server::reembed(
        dir.path(),
        "model-b",
        3,
        &ScriptedEmbedder::ok(vec![0.1, 0.2, 0.3]),
    )
    .await
    .expect("resume");
    assert_eq!(report.converted, 2, "bb.db plus the boot-created shared.db");
    assert_eq!(report.skipped, 1);
    assert!(!dir.path().join(REEMBED_TARGET_FILE).exists());
    let bb = SqliteStore::open_maintenance(&dir.path().join("bb.db"))
        .await
        .unwrap();
    assert_eq!(
        bb.embedding_meta().await.unwrap(),
        ("model-b".to_string(), 3)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reembed_to_runtime_model_restores_readiness_and_recall() {
    let dir = TempDir::new().unwrap();
    seed_old_model_db(
        &dir.path().join("work.db"),
        "old-model",
        2,
        &[(1, "argocd deploy to staging for offers")],
    )
    .await;

    let (router, _) = boot(dir.path()).await;
    let (status, body) = send(&router, Method::GET, "/health", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body["reason"].as_str().unwrap().contains("mismatch"),
        "{body}"
    );

    let real = embedder().await;
    let report = server::reembed(dir.path(), MODEL_NAME, DIMENSION, &*real)
        .await
        .expect("reembed to runtime model");
    assert_eq!(report.converted, 1);

    let (router, _) = boot(dir.path()).await;
    let (status, _) = send(&router, Method::GET, "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = req(
        &router,
        Method::GET,
        "/memories/search?ws=work&q=deploy%20to%20staging",
        None,
    )
    .await;
    assert!(
        body["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == mid(1)),
        "recall incoherent after reembed: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_recovers_from_crash_and_rejects_invalid_files() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("half.db"), b"").unwrap();
    let (router, _) = boot(dir.path()).await;
    let (status, body) = send(&router, Method::GET, "/health", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = req(&router, Method::GET, "/workspaces", None).await;
    assert_eq!(body["workspaces"], json!(["half", "shared"]));

    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("Bad.db"), b"").unwrap();
    let (router, _) = boot(dir.path()).await;
    let (status, body) = send(&router, Method::GET, "/health", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body["reason"].as_str().unwrap().contains("Bad.db"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fts_rebuild_keeps_search_working() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    put_memory(&router, "work", 1, "unleash feature flags rollout").await;

    let rebuilt = server::fts_rebuild(dir.path()).await.expect("rebuild");
    assert_eq!(rebuilt, 2);

    let (_, body) = req(
        &router,
        Method::GET,
        "/memories/search?ws=work&q=unleash%20rollout",
        None,
    )
    .await;
    assert!(
        body["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == mid(1)),
        "search broken after fts rebuild: {body}"
    );
}
