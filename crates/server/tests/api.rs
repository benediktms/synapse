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

async fn put_preference(router: &Router, n: u32, content: &str) -> (StatusCode, Value) {
    req(
        router,
        Method::PUT,
        &format!("/preferences/{}", mid(n)),
        Some(json!({ "content": content, "kind": "user", "tags": [] })),
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
    assert_eq!(body["workspaces"], json!([]));

    let (status, _) = req(&router, Method::PUT, "/workspaces/work", None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = req(&router, Method::PUT, "/workspaces/work", None).await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = req(&router, Method::GET, "/workspaces", None).await;
    assert_eq!(body["workspaces"], json!(["work"]));

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
    put_preference(&router, 4, "prefers oat milk in coffee").await;

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
        hits.iter()
            .all(|h| h["origin"] != json!({"workspace": "personal"})),
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
    let names: Vec<String> = groups.iter().map(|g| g["origin"].to_string()).collect();
    assert!(
        names.contains(&r#"{"workspace":"work"}"#.to_string())
            && names.contains(&r#"{"workspace":"personal"}"#.to_string()),
        "{body}"
    );
    assert!(
        !names.iter().any(|n| n.contains("shared")),
        "the shared database leaked into grouped output: {body}"
    );
    assert!(
        names.iter().filter(|n| *n == "\"preference\"").count() <= 1,
        "preferences emitted more than once: {body}"
    );
    let personal_group = groups
        .iter()
        .find(|g| g["origin"] == json!({"workspace": "personal"}))
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
async fn search_hits_carry_a_neighbors_array_and_accept_links_scope() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    put_memory(&router, "work", 1, "argocd deploy to staging for offers").await;

    // Default: hits include a (here empty) neighbors array.
    let (status, body) = req(
        &router,
        Method::GET,
        "/memories/search?ws=work&q=deploy%20staging",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hit = body["hits"][0].as_object().expect("hit object");
    assert!(
        hit.contains_key("neighbors"),
        "recall hit is missing the neighbors field: {body}"
    );
    assert_eq!(hit["neighbors"], json!([]));

    // The links_scope route param is accepted (cross-scope neighbor tightening flag).
    let (status, _) = req(
        &router,
        Method::GET,
        "/memories/search?ws=work&q=deploy&links_scope=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn links_route_returns_a_jgf_graph_and_validates_depth() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    put_memory(&router, "work", 1, "argocd deploy to staging").await;

    // A single memory with no links -> a JGF graph with just the root.
    let (status, body) = req(
        &router,
        Method::GET,
        &format!("/memories/{}/links?ws=work", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["graph"]["metadata"]["root"], mid(1));
    assert_eq!(body["graph"]["metadata"]["depth"], 2);
    assert_eq!(body["graph"]["metadata"]["truncated"], false);
    assert_eq!(body["graph"]["edges"].as_array().unwrap().len(), 0);
    assert!(body["graph"]["nodes"][&mid(1)]["label"].is_string());

    // Depth is bounded at MAX_GRAPH_DEPTH.
    let (status, _) = req(
        &router,
        Method::GET,
        &format!("/memories/{}/links?ws=work&depth=99", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown memory is a not-found.
    let (status, _) = req(
        &router,
        Method::GET,
        &format!("/memories/{}/links?ws=work", mid(999)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn link_mutation_create_retype_unlink_over_http() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    put_memory(&router, "work", 1, "old deploy process").await;
    put_memory(&router, "work", 2, "new deploy process").await;

    let links_url = |a: u32| format!("/memories/{}/links?ws=work", mid(a));
    let link_body =
        |target: u32, relation: &str| json!({ "target": mid(target), "relation": relation });

    let (status, _) = req(
        &router,
        Method::POST,
        &links_url(2),
        Some(link_body(1, "supersession")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = req(&router, Method::GET, &links_url(1), None).await;
    assert_eq!(status, StatusCode::OK);
    let edges = body["graph"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["relation"], "supersession");
    assert_eq!(edges[0]["directed"], true);

    let (status, _) = req(
        &router,
        Method::PATCH,
        &links_url(2),
        Some(link_body(1, "support")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = req(&router, Method::GET, &links_url(1), None).await;
    let edges = body["graph"]["edges"].as_array().unwrap();
    assert_eq!(edges[0]["relation"], "support");
    assert_eq!(edges[0]["directed"], false);

    let (status, _) = req(
        &router,
        Method::DELETE,
        &format!("/memories/{}/links?ws=work&target={}", mid(1), mid(2)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = req(&router, Method::GET, &links_url(1), None).await;
    assert_eq!(body["graph"]["edges"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_is_not_addressable_as_a_workspace() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;

    for uri in [
        format!("/memories/{}?ws=shared", mid(1)),
        "/memories?ws=shared".to_string(),
        "/memories/search?ws=shared&q=anything".to_string(),
        "/memories/search?ws=shared&q=anything&all=true".to_string(),
        "/context?ws=shared".to_string(),
        "/export?ws=shared".to_string(),
    ] {
        let (status, body) = req(&router, Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} was accepted");
        assert!(
            body["error"].as_str().unwrap().contains("/preferences"),
            "{uri}: {body}"
        );
    }
    let (status, _) = req(
        &router,
        Method::PUT,
        &format!("/memories/{}?ws=shared", mid(1)),
        Some(put_body("sneaking in")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn preferences_are_reachable_from_any_workspace_and_project() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    req(&router, Method::PUT, "/workspaces/personal", None).await;

    let (status, body) = put_preference(&router, 1, "always use ripgrep for code search").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["scope"], "workspace");

    let (status, replay) = put_preference(&router, 1, "always use ripgrep for code search").await;
    assert_eq!(status, StatusCode::OK, "replay must be idempotent");
    assert_eq!(replay["id"], body["id"]);
    let (status, _) = put_preference(&router, 1, "a different payload under the same id").await;
    assert_eq!(status, StatusCode::CONFLICT);

    for ws in ["work", "personal"] {
        let (status, body) = req(
            &router,
            Method::GET,
            &format!("/memories/search?ws={ws}&q=ripgrep%20code%20search&scope=fresha/offers"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let hits = body["hits"].as_array().unwrap();
        let found: Vec<&Value> = hits.iter().filter(|h| h["id"] == mid(1)).collect();
        assert_eq!(found.len(), 1, "preference missing from {ws}: {body}");
        assert_eq!(found[0]["origin"], "preference", "{body}");
    }

    let (status, body) = req(
        &router,
        Method::GET,
        &format!("/preferences/{}", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], mid(1));

    let (status, body) = req(
        &router,
        Method::PATCH,
        &format!("/preferences/{}", mid(1)),
        Some(json!({ "pinned": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pinned"], true);

    let (status, body) = req(&router, Method::GET, "/preferences", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["memories"].as_array().unwrap().len(), 1);

    let (status, dump) = req(&router, Method::GET, "/preferences/export", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dump["origin"], "preference");
    assert!(!dump.to_string().contains("shared"), "{dump}");

    let (status, _) = req(
        &router,
        Method::DELETE,
        &format!("/preferences/{}", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, report) = req(
        &router,
        Method::POST,
        "/preferences/import?mode=merge",
        Some(dump),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["imported"], 1);
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
    put_preference(&router, 4, "prefers tables over prose").await;

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
    assert_eq!(ids("preferences"), vec![mid(4)]);
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

async fn move_memory(router: &Router, n: u32, from: Value, to: Value) -> (StatusCode, Value) {
    req(
        router,
        Method::POST,
        &format!("/memories/{}/move", mid(n)),
        Some(json!({ "from": from, "to": to })),
    )
    .await
}

async fn put_scoped(
    router: &Router,
    ws: &str,
    n: u32,
    content: &str,
    scope: &str,
) -> (StatusCode, Value) {
    req(
        router,
        Method::PUT,
        &format!("/memories/{}?ws={ws}", mid(n)),
        Some(json!({ "content": content, "kind": "project", "scope": scope, "tags": ["alpha"] })),
    )
    .await
}

fn finds(body: &Value, n: u32) -> bool {
    body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .any(|hit| hit["id"] == mid(n))
}

#[tokio::test(flavor = "multi_thread")]
async fn move_relocates_a_memory_without_changing_its_identity() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    req(&router, Method::PUT, "/workspaces/personal", None).await;
    let (_, created) = put_scoped(
        &router,
        "work",
        1,
        "the home server runs nixos on a mini pc",
        "me/homelab",
    )
    .await;

    let (status, body) = move_memory(
        &router,
        1,
        json!({ "workspace": "work" }),
        json!({ "workspace": "personal" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["moved"], true);
    assert_eq!(body["from_scope"], "me/homelab");
    assert_eq!(body["scope"], "me/homelab");
    assert_eq!(body["created_at"], created["created_at"]);
    assert_ne!(body["updated_at"], Value::Null);
    assert_eq!(body["tags"], json!(["alpha"]));

    let (status, _) = req(
        &router,
        Method::GET,
        &format!("/memories/{}?ws=work", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "source still holds it");
    let (status, moved) = req(
        &router,
        Method::GET,
        &format!("/memories/{}?ws=personal", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(moved["created_at"], created["created_at"]);

    let query = "/memories/search?q=nixos%20home%20server&scope=me/homelab";
    let (_, personal) = req(&router, Method::GET, &format!("{query}&ws=personal"), None).await;
    assert!(
        finds(&personal, 1),
        "not recallable from target: {personal}"
    );
    let (_, work) = req(&router, Method::GET, &format!("{query}&ws=work"), None).await;
    assert!(!finds(&work, 1), "still recallable from source: {work}");
}

#[tokio::test(flavor = "multi_thread")]
async fn move_into_preferences_widens_scope_and_moving_out_narrows_reach() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    req(&router, Method::PUT, "/workspaces/personal", None).await;
    put_scoped(
        &router,
        "work",
        1,
        "benedikt writes commit subjects in the imperative mood",
        "fresha/offers",
    )
    .await;

    let (status, body) = move_memory(
        &router,
        1,
        json!({ "workspace": "work" }),
        json!("preference"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["from_scope"], "fresha/offers");
    assert_eq!(body["scope"], "workspace", "project scope must be widened");

    let query = "/memories/search?q=imperative%20mood%20commit%20subjects";
    let (_, elsewhere) = req(&router, Method::GET, &format!("{query}&ws=personal"), None).await;
    assert!(
        finds(&elsewhere, 1),
        "preference did not travel: {elsewhere}"
    );
    assert_eq!(
        elsewhere["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|hit| hit["id"] == mid(1))
            .unwrap()["origin"],
        "preference"
    );

    let (status, body) = move_memory(
        &router,
        1,
        json!("preference"),
        json!({ "workspace": "personal" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scope"], "workspace", "workspace scope is kept");
    let (_, work) = req(&router, Method::GET, &format!("{query}&ws=work"), None).await;
    assert!(!finds(&work, 1), "still reaching other workspaces: {work}");
    let (status, _) = req(
        &router,
        Method::GET,
        &format!("/preferences/{}", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_move_finishes_and_a_colliding_id_conflicts() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    req(&router, Method::PUT, "/workspaces/personal", None).await;

    let payload = "the espresso machine needs descaling every two months";
    put_scoped(&router, "work", 1, payload, "workspace").await;
    put_scoped(&router, "personal", 1, payload, "workspace").await;
    let (status, body) = move_memory(
        &router,
        1,
        json!({ "workspace": "work" }),
        json!({ "workspace": "personal" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["moved"], true);
    let (status, _) = req(
        &router,
        Method::GET,
        &format!("/memories/{}?ws=work", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "source was not cleared");

    put_scoped(&router, "work", 2, "one thing", "workspace").await;
    put_scoped(&router, "personal", 2, "a different thing", "workspace").await;
    let (status, body) = move_memory(
        &router,
        2,
        json!({ "workspace": "work" }),
        json!({ "workspace": "personal" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, kept) = req(
        &router,
        Method::GET,
        &format!("/memories/{}?ws=work", mid(2)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a conflict deleted the source");
    assert_eq!(kept["content"], "one thing");
    let (_, target) = req(
        &router,
        Method::GET,
        &format!("/memories/{}?ws=personal", mid(2)),
        None,
    )
    .await;
    assert_eq!(target["content"], "a different thing");
}

#[tokio::test(flavor = "multi_thread")]
async fn move_rejects_shared_by_name_and_reports_nothing_moved_in_place() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    put_memory(&router, "work", 1, "a fact that stays put").await;

    for (from, to) in [
        (
            json!({ "workspace": "shared" }),
            json!({ "workspace": "work" }),
        ),
        (
            json!({ "workspace": "work" }),
            json!({ "workspace": "shared" }),
        ),
    ] {
        let (status, body) = move_memory(&router, 1, from, to).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("/preferences"),
            "{body}"
        );
    }

    let (status, body) = move_memory(
        &router,
        1,
        json!({ "workspace": "work" }),
        json!({ "workspace": "work" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["moved"], false);
    let (status, _) = req(
        &router,
        Method::GET,
        &format!("/memories/{}?ws=work", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a self-move deleted the memory");

    let (status, _) = move_memory(
        &router,
        1,
        json!({ "workspace": "work" }),
        json!({ "workspace": "nope" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = move_memory(
        &router,
        9,
        json!({ "workspace": "work" }),
        json!("preference"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn importance_tier_surfaces_on_save_edit_and_get() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;

    let (status, body) = req(
        &router,
        Method::PUT,
        &format!("/memories/{}?ws=work", mid(1)),
        Some(json!({ "content": "deploy runbook", "kind": "project",
                     "scope": "workspace", "tags": [], "importance": "high" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["importance"], "high");

    let (status, body) = req(
        &router,
        Method::GET,
        &format!("/memories/{}?ws=work", mid(1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["importance"], "high");

    let (status, body) = req(
        &router,
        Method::PATCH,
        &format!("/memories/{}?ws=work", mid(1)),
        Some(json!({ "importance": "low" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["importance"], "low");

    let (status, body) = req(
        &router,
        Method::PUT,
        &format!("/memories/{}?ws=work", mid(2)),
        Some(json!({ "content": "plain fact", "kind": "project",
                     "scope": "workspace", "tags": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["importance"], "medium");

    // An idempotent retry that omits importance (an older client) must not 409 against the
    // edited rank — it keeps the stored tier and reports OK, not Conflict.
    let (status, body) = req(
        &router,
        Method::PUT,
        &format!("/memories/{}?ws=work", mid(1)),
        Some(json!({ "content": "deploy runbook", "kind": "project",
                     "scope": "workspace", "tags": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["importance"], "low");

    let (status, _) = req(
        &router,
        Method::PATCH,
        &format!("/memories/{}?ws=work", mid(1)),
        Some(json!({ "importance": "urgent" })),
    )
    .await;
    assert!(
        status.is_client_error(),
        "invalid tier must be rejected, got {status}"
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

    let (status, report) = req(
        &router,
        Method::POST,
        "/import?ws=restore",
        Some(export.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "repeating the same default import must resume, not 409: {report}"
    );
    assert_eq!(report["unchanged"], 2);

    put_memory(&router, "restore", 7, "a fact this dump never had").await;
    let (status, _) = req(
        &router,
        Method::POST,
        "/import?ws=restore",
        Some(export.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "default import must refuse a target holding unrelated memories"
    );
    req(
        &router,
        Method::DELETE,
        &format!("/memories/{}?ws=restore", mid(7)),
        None,
    )
    .await;

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

#[tokio::test(flavor = "multi_thread")]
async fn import_keeps_preferences_and_workspaces_apart() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/work", None).await;
    put_memory(&router, "work", 1, "an internal work detail").await;
    put_preference(&router, 2, "prefers ripgrep").await;

    let (_, work_dump) = req(&router, Method::GET, "/export?ws=work", None).await;
    let (_, preference_dump) = req(&router, Method::GET, "/preferences/export", None).await;

    for mode in ["", "?mode=merge"] {
        let (status, body) = req(
            &router,
            Method::POST,
            &format!("/preferences/import{mode}"),
            Some(work_dump.clone()),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a workspace dump became preferences: {body}"
        );

        let (status, body) = req(
            &router,
            Method::POST,
            &format!("/import?ws=work{}", mode.replace('?', "&")),
            Some(preference_dump.clone()),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a preference dump became workspace memories: {body}"
        );
    }

    let (_, listed) = req(&router, Method::GET, "/preferences", None).await;
    assert_eq!(
        listed["memories"].as_array().unwrap().len(),
        1,
        "the rejected dump still landed: {listed}"
    );

    let mut project_scoped = preference_dump.clone();
    project_scoped["memories"][0]["scope"] = json!("fresha/offers");
    project_scoped["memories"][0]["id"] = json!(mid(3));
    let (status, body) = req(
        &router,
        Method::POST,
        "/preferences/import?mode=merge",
        Some(project_scoped),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a project-scoped preference was accepted: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn import_normalizes_timestamps_and_rejects_impossible_ones() {
    let dir = TempDir::new().unwrap();
    let (router, _) = boot(dir.path()).await;
    req(&router, Method::PUT, "/workspaces/restore", None).await;

    let dump = |created: &str, updated: &str| {
        json!({
            "version": 1,
            "origin": { "workspace": "restore" },
            "memories": [{
                "id": mid(1),
                "content": "a restored fact",
                "kind": "project",
                "scope": "workspace",
                "tags": [],
                "pinned": false,
                "created_at": created,
                "updated_at": updated,
            }],
        })
    };

    for bad in [
        "2026-99-99T99:99:99Z",
        "2026-02-30T00:00:00Z",
        "2026-08-02T10:00:00+02:00",
        "2026-08-02 10:00:00Z",
    ] {
        let (status, body) = req(
            &router,
            Method::POST,
            "/import?ws=restore&mode=merge",
            Some(dump(bad, "2026-08-02T10:00:00Z")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {bad}: {body}");
    }

    let (status, body) = req(
        &router,
        Method::POST,
        "/import?ws=restore&mode=merge",
        Some(dump("2026-08-02T10:00:00.500Z", "2026-08-02T10:00:00.999Z")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, stored) = req(
        &router,
        Method::GET,
        &format!("/memories/{}?ws=restore", mid(1)),
        None,
    )
    .await;
    assert_eq!(stored["created_at"], "2026-08-02T10:00:00Z");
    assert_eq!(stored["updated_at"], "2026-08-02T10:00:00Z");
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
        importance: domain::Importance::DEFAULT,
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
    assert_unrevealing_readiness(&body);

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
    assert_eq!(body["workspaces"], json!(["half"]));

    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("Bad.db"), b"").unwrap();
    let (router, _) = boot(dir.path()).await;
    let (status, body) = send(&router, Method::GET, "/health", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_unrevealing_readiness(&body);

    let (status, body) = req(&router, Method::GET, "/workspaces", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body["error"].as_str().unwrap().contains("Bad.db"),
        "authenticated callers still need the detail: {body}"
    );
}

fn assert_unrevealing_readiness(body: &Value) {
    assert_eq!(body["status"], "unready");
    let reason = body["reason"].as_str().expect("reason");
    for secret in ["shared", ".db", "/", "model", "mismatch"] {
        assert!(
            !reason.contains(secret),
            "public readiness reason leaks {secret:?}: {reason}"
        );
    }
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
