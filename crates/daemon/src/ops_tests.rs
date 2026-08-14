//! Ops-layer behaviour, driven through the JSON-RPC surface the CLI actually calls.
//!
//! These cover what `crates/server/tests/api.rs` was the only home for: import and export
//! semantics, move semantics, recall isolation, the context digest, and the reach rules that
//! keep preferences and workspaces apart. The transport differs; the ops layer under it is the
//! same `api::ops` module the HTTP routes called, so the assertions are about `ops`, not about
//! either transport.

use std::collections::HashMap;
use std::sync::Arc;

use adapters_fastembed::{DIMENSION, FastEmbedder, MODEL_NAME};
use adapters_libsql::LibsqlStore;
use domain::Workspace;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::OnceCell;

use crate::DaemonApp;
use crate::rpc::dispatch;

/// One model for the whole test binary. Booting an embedder per test dominates runtime and
/// the model is immutable, so sharing it costs nothing and saves minutes.
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

struct Harness {
    app: DaemonApp,
    _dir: TempDir,
}

impl Harness {
    /// Boot a daemon over local libSQL databases — no Turso primary, no network. `shared` is
    /// always present, because the preference store is not something a session opts into.
    async fn boot(workspaces: &[&str]) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let mut stores = HashMap::new();
        let named = workspaces
            .iter()
            .map(|name| Workspace::new(name).expect("valid workspace name"));
        for ws in named.chain(std::iter::once(Workspace::shared())) {
            let db = libsql::Builder::new_local(dir.path().join(format!("{ws}.db")))
                .build()
                .await
                .expect("local db");
            let store = LibsqlStore::init(db, MODEL_NAME, DIMENSION)
                .await
                .expect("store init");
            stores.insert(ws, Arc::new(store));
        }
        let app = DaemonApp::for_tests(dir.path().to_path_buf(), embedder().await, stores);
        Self { app, _dir: dir }
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        serde_json::from_str(&dispatch(&line.to_string(), &self.app).await).expect("valid response")
    }

    /// The result of a call that must succeed.
    async fn ok(&self, method: &str, params: Value) -> Value {
        let response = self.call(method, params).await;
        assert!(
            response.get("error").is_none(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }

    /// The error code of a call that must fail.
    async fn err(&self, method: &str, params: Value) -> i64 {
        let response = self.call(method, params).await;
        response["error"]["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("{method} succeeded: {response}"))
    }

    async fn save(&self, origin: Value, n: u32, content: &str) -> Value {
        self.save_scoped(origin, n, content, "workspace").await
    }

    async fn save_scoped(&self, origin: Value, n: u32, content: &str, scope: &str) -> Value {
        self.ok(
            "memory.save",
            json!({
                "origin": origin, "id": mid(n), "content": content,
                "title": "A stated fact", "kind": "project", "scope": scope, "tags": ["alpha"],
            }),
        )
        .await
    }

    async fn pin(&self, origin: Value, n: u32) {
        self.ok(
            "memory.edit",
            json!({"origin": origin, "id": mid(n), "pinned": true}),
        )
        .await;
    }

    async fn supersede(&self, origin: Value, newer: u32, older: u32) {
        self.ok(
            "link.create",
            json!({"origin": origin, "id": mid(newer), "target": mid(older),
                   "relation": "supersession"}),
        )
        .await;
    }

    async fn edges(&self, origin: Value, n: u32) -> Vec<Value> {
        let graph = self
            .ok("link.graph", json!({"origin": origin, "id": mid(n)}))
            .await;
        graph["graph"]["edges"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    async fn export(&self, origin: Value) -> Value {
        self.ok("export", json!({"origin": origin})).await
    }

    async fn import(&self, origin: Value, mode: Option<&str>, doc: Value) -> Value {
        self.call(
            "import",
            json!({"origin": origin, "mode": mode, "doc": doc}),
        )
        .await
    }
}

const NOT_FOUND: i64 = -32001;
const CONFLICT: i64 = -32002;
const BAD_REQUEST: i64 = -32602;

fn mid(n: u32) -> String {
    format!("m_{n:022}")
}

fn ws(name: &str) -> Value {
    json!({"workspace": name})
}

fn pref() -> Value {
    json!("preference")
}

fn finds(result: &Value, n: u32) -> bool {
    result["hits"]
        .as_array()
        .is_some_and(|hits| hits.iter().any(|hit| hit["id"] == mid(n)))
}

/// A dump body, so an import test states only what it is exercising.
fn dump(origin: Value, memories: Value, links: Value) -> Value {
    json!({"version": 2, "origin": origin, "memories": memories, "links": links})
}

fn dumped_memory(n: u32, content: &str, scope: &str, kind: &str) -> Value {
    json!({
        "id": mid(n), "content": content, "title": "", "kind": kind, "scope": scope,
        "tags": [], "pinned": false, "importance": "medium",
        "created_at": "2026-07-01T00:00:00Z", "updated_at": "2026-07-01T00:00:00Z",
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn save_is_idempotent_and_conflicts_on_a_changed_payload() {
    let h = Harness::boot(&["work"]).await;

    let created = h
        .save(ws("work"), 1, "rebase feature branches onto main")
        .await;
    assert_eq!(created["created"], true, "{created}");
    assert_eq!(created["memory"]["id"], mid(1));

    let replay = h
        .save(ws("work"), 1, "rebase feature branches onto main")
        .await;
    assert_eq!(replay["created"], false, "a replay must not re-create");

    let changed = h
        .call(
            "memory.save",
            json!({"origin": ws("work"), "id": mid(1), "content": "different content entirely",
                   "title": "A stated fact", "kind": "project", "scope": "workspace",
                   "tags": ["alpha"]}),
        )
        .await;
    assert_eq!(changed["error"]["code"], CONFLICT, "{changed}");

    let unknown = h
        .err(
            "memory.save",
            json!({"origin": ws("nope"), "id": mid(2), "content": "no such workspace",
                   "title": "A stated fact", "kind": "project", "scope": "workspace",
                   "tags": []}),
        )
        .await;
    assert_eq!(unknown, NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn validation_limits_reject_clearly() {
    let h = Harness::boot(&["work"]).await;
    let body = |extra: Value| {
        let mut base = json!({"origin": ws("work"), "id": mid(1), "content": "ok",
                              "title": "A stated fact", "kind": "project",
                              "scope": "workspace", "tags": []});
        for (key, value) in extra.as_object().expect("object") {
            base[key] = value.clone();
        }
        base
    };
    let oversized = "x".repeat(api::CONTENT_MAX_BYTES + 1);
    let cases = [
        (
            "content over the byte cap",
            body(json!({"content": oversized})),
        ),
        (
            "content over the token window",
            body(json!({"content": "ab ".repeat(2000)})),
        ),
        ("empty content", body(json!({"content": ""}))),
        ("empty title", body(json!({"title": ""}))),
        ("unknown kind", body(json!({"kind": "note"}))),
        ("malformed scope", body(json!({"scope": "has space"}))),
        (
            "too many tags",
            body(json!({"tags": (0..17).map(|i| i.to_string()).collect::<Vec<_>>()})),
        ),
        ("a tag with whitespace", body(json!({"tags": ["bad tag"]}))),
        ("a malformed id", body(json!({"id": "not-an-id"}))),
        (
            "an unknown importance tier",
            body(json!({"importance": "urgent"})),
        ),
    ];
    for (case, params) in cases {
        assert_eq!(
            h.err("memory.save", params).await,
            BAD_REQUEST,
            "accepted {case}"
        );
    }

    let long_query = "q".repeat(api::QUERY_MAX_BYTES + 1);
    assert_eq!(
        h.err("search", json!({"ws": "work", "q": long_query}))
            .await,
        BAD_REQUEST,
        "accepted an over-long query"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recall_fuses_both_lanes_and_respects_workspace_isolation() {
    let h = Harness::boot(&["work", "personal"]).await;
    h.save(ws("work"), 1, "argocd deploy to staging for offers")
        .await;
    h.save(ws("work"), 2, "std::collections::HashMap usage patterns")
        .await;
    h.save(ws("personal"), 3, "personal home server deploy notes")
        .await;
    h.save(pref(), 4, "prefers oat milk in coffee").await;

    let hits = h
        .ok("search", json!({"ws": "work", "q": "deploy to staging"}))
        .await;
    assert!(finds(&hits, 1), "work deploy memory missing: {hits}");
    assert!(
        !finds(&hits, 3),
        "personal leaked into a work recall: {hits}"
    );
    assert!(!finds(&hits, 4), "an unrelated preference surfaced: {hits}");

    let keyword = h.ok("search", json!({"ws": "work", "q": "HashMap"})).await;
    assert!(
        finds(&keyword, 2),
        "the keyword leg missed a code-shaped query: {keyword}"
    );

    let grouped = h.ok("search", json!({"q": "deploy", "all": true})).await;
    let origins: Vec<String> = grouped["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|group| group["origin"].to_string())
        .collect();
    assert!(
        origins.iter().any(|o| o.contains("work"))
            && origins.iter().any(|o| o.contains("personal")),
        "{grouped}"
    );
    assert!(
        !origins.iter().any(|o| o.contains("shared")),
        "the shared database leaked into grouped output: {grouped}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recall_hit_carries_its_neighbours_and_accepts_the_scope_flag() {
    let h = Harness::boot(&["work"]).await;
    h.save(ws("work"), 1, "argocd deploy to staging for offers")
        .await;

    let hits = h
        .ok("search", json!({"ws": "work", "q": "deploy staging"}))
        .await;
    let hit = hits["hits"][0].as_object().expect("a hit");
    assert!(
        hit.contains_key("neighbors"),
        "a recall hit is missing the neighbors field: {hits}"
    );
    assert_eq!(hit["neighbors"], json!([]));

    h.ok(
        "search",
        json!({"ws": "work", "q": "deploy", "links_scope": true}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_superseded_memory_leaves_standalone_recall_but_stays_a_neighbour() {
    let h = Harness::boot(&["work"]).await;
    h.save(ws("work"), 1, "deploys go through the slack bot")
        .await;
    h.save(
        ws("work"),
        2,
        "deploys go through argocd, not the slack bot",
    )
    .await;
    h.supersede(ws("work"), 2, 1).await;

    let hits = h
        .ok("search", json!({"ws": "work", "q": "how deploys happen"}))
        .await;
    assert!(
        !finds(&hits, 1),
        "a superseded memory still recalls on its own: {hits}"
    );
    assert!(finds(&hits, 2), "the superseder must recall: {hits}");

    let neighbours = hits["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .find(|hit| hit["id"] == mid(2))
        .expect("the superseder")["neighbors"]
        .clone();
    assert!(
        neighbours
            .as_array()
            .expect("neighbors")
            .iter()
            .any(|n| n["id"] == mid(1)),
        "the superseded memory must stay reachable as a neighbour: {neighbours}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_context_digest_leads_with_the_pinned_memory() {
    let h = Harness::boot(&["work"]).await;
    h.save(ws("work"), 1, "pinned architectural decision").await;
    h.pin(ws("work"), 1).await;
    h.save_scoped(
        ws("work"),
        2,
        "offers uses the outbox pattern",
        "fresha/offers",
    )
    .await;
    h.save(ws("work"), 3, "workspace wide convention").await;
    h.save(pref(), 4, "prefers tables over prose").await;

    let digest = h
        .ok(
            "context",
            json!({"origin": ws("work"), "project": "fresha/offers"}),
        )
        .await;
    let ids: Vec<String> = digest["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["id"].as_str().expect("id").to_string())
        .collect();
    assert_eq!(ids[0], mid(1), "the pinned memory must lead: {digest}");
    let mut rest = ids[1..].to_vec();
    rest.sort();
    assert_eq!(rest, vec![mid(2), mid(3), mid(4)], "{digest}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_reembeds_and_a_delete_removes() {
    let h = Harness::boot(&["work"]).await;
    h.save(ws("work"), 1, "the cat sat on the mat").await;

    let edited = h
        .ok(
            "memory.edit",
            json!({"origin": ws("work"), "id": mid(1),
                   "content": "kubernetes ingress configuration guide"}),
        )
        .await;
    assert_eq!(edited["content"], "kubernetes ingress configuration guide");

    let hits = h
        .ok("search", json!({"ws": "work", "q": "kubernetes ingress"}))
        .await;
    assert!(finds(&hits, 1), "edited content is not searchable: {hits}");

    assert_eq!(
        h.err(
            "memory.edit",
            json!({"origin": ws("work"), "id": mid(9), "pinned": true})
        )
        .await,
        NOT_FOUND
    );

    h.ok("memory.forget", json!({"origin": ws("work"), "id": mid(1)}))
        .await;
    assert_eq!(
        h.err("memory.get", json!({"origin": ws("work"), "id": mid(1)}))
            .await,
        NOT_FOUND
    );
    let hits = h
        .ok("search", json!({"ws": "work", "q": "kubernetes ingress"}))
        .await;
    assert!(!finds(&hits, 1), "a deleted memory still recalls: {hits}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_importance_tier_survives_save_edit_and_an_older_client_replay() {
    let h = Harness::boot(&["work"]).await;

    let created = h
        .ok(
            "memory.save",
            json!({"origin": ws("work"), "id": mid(1), "content": "deploy runbook",
                   "title": "A stated fact", "kind": "project", "scope": "workspace",
                   "tags": [], "importance": "high"}),
        )
        .await;
    assert_eq!(created["memory"]["importance"], "high");

    let fetched = h
        .ok("memory.get", json!({"origin": ws("work"), "id": mid(1)}))
        .await;
    assert_eq!(fetched["importance"], "high");

    let lowered = h
        .ok(
            "memory.edit",
            json!({"origin": ws("work"), "id": mid(1), "importance": "low"}),
        )
        .await;
    assert_eq!(lowered["importance"], "low");

    let defaulted = h
        .ok(
            "memory.save",
            json!({"origin": ws("work"), "id": mid(2), "content": "plain fact",
                   "title": "A stated fact", "kind": "project", "scope": "workspace",
                   "tags": []}),
        )
        .await;
    assert_eq!(defaulted["memory"]["importance"], "medium");

    let replay = h
        .ok(
            "memory.save",
            json!({"origin": ws("work"), "id": mid(1), "content": "deploy runbook",
                   "title": "A stated fact", "kind": "project", "scope": "workspace",
                   "tags": []}),
        )
        .await;
    assert_eq!(
        replay["created"], false,
        "a replay omitting importance must not conflict against the edited tier: {replay}"
    );
    assert_eq!(
        replay["memory"]["importance"], "low",
        "a replay omitting importance must keep the stored tier: {replay}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn preferences_are_reachable_from_every_workspace_and_project() {
    let h = Harness::boot(&["work", "personal"]).await;
    let created = h
        .save(pref(), 1, "always use ripgrep for code search")
        .await;
    assert_eq!(
        created["memory"]["scope"], "workspace",
        "a preference is never project-scoped: {created}"
    );

    for name in ["work", "personal"] {
        let hits = h
            .ok(
                "search",
                json!({"ws": name, "q": "ripgrep code search", "scope": "fresha/offers"}),
            )
            .await;
        assert!(
            finds(&hits, 1),
            "the preference is missing from {name}: {hits}"
        );
        let hit = hits["hits"]
            .as_array()
            .expect("hits")
            .iter()
            .find(|hit| hit["id"] == mid(1))
            .expect("the preference");
        assert_eq!(hit["origin"], "preference", "{hits}");
    }

    let dumped = h.export(pref()).await;
    assert_eq!(dumped["origin"], "preference");
    assert!(
        !dumped.to_string().contains("shared"),
        "the backing database is named on the wire: {dumped}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shared_store_is_not_addressable_as_a_workspace() {
    let h = Harness::boot(&["work"]).await;

    for (method, params) in [
        ("memory.get", json!({"origin": ws("shared"), "id": mid(1)})),
        ("memory.list", json!({"origin": ws("shared")})),
        ("context", json!({"origin": ws("shared")})),
        ("export", json!({"origin": ws("shared")})),
        ("search", json!({"ws": "shared", "q": "anything"})),
    ] {
        assert_eq!(
            h.err(method, params).await,
            BAD_REQUEST,
            "{method} accepted the shared store by name"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_relocates_a_memory_without_changing_its_identity() {
    let h = Harness::boot(&["work", "personal"]).await;
    let created = h
        .save_scoped(
            ws("work"),
            1,
            "the home server runs nixos on a mini pc",
            "me/homelab",
        )
        .await;

    let moved = h
        .ok(
            "memory.move",
            json!({"id": mid(1), "from": ws("work"), "to": ws("personal")}),
        )
        .await;
    assert_eq!(moved["moved"], true, "{moved}");
    assert_eq!(moved["from_scope"], "me/homelab");
    assert_eq!(moved["scope"], "me/homelab", "the scope must survive");
    assert_eq!(
        moved["created_at"], created["memory"]["created_at"],
        "a move minted a new creation date"
    );
    assert_eq!(moved["tags"], json!(["alpha"]));

    assert_eq!(
        h.err("memory.get", json!({"origin": ws("work"), "id": mid(1)}))
            .await,
        NOT_FOUND,
        "the source still holds it"
    );
    let target = h
        .ok(
            "memory.get",
            json!({"origin": ws("personal"), "id": mid(1)}),
        )
        .await;
    assert_eq!(target["created_at"], created["memory"]["created_at"]);

    let query = json!({"q": "nixos home server", "scope": "me/homelab"});
    let mut from_target = query.clone();
    from_target["ws"] = json!("personal");
    assert!(
        finds(&h.ok("search", from_target).await, 1),
        "not recallable from the target"
    );
    let mut from_source = query;
    from_source["ws"] = json!("work");
    assert!(
        !finds(&h.ok("search", from_source).await, 1),
        "still recallable from the source"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_drops_the_memory_edges_and_reports_how_many() {
    let h = Harness::boot(&["work", "personal"]).await;
    h.save(ws("work"), 1, "old deploy process").await;
    h.save(ws("work"), 2, "new deploy process").await;
    h.supersede(ws("work"), 2, 1).await;
    assert_eq!(
        h.edges(ws("work"), 2).await.len(),
        1,
        "the edge must exist before the move, or the assertion below proves nothing"
    );

    let moved = h
        .ok(
            "memory.move",
            json!({"id": mid(1), "from": ws("work"), "to": ws("personal")}),
        )
        .await;
    assert_eq!(moved["moved"], true, "{moved}");
    assert_eq!(
        moved["links_dropped"], 1,
        "a dropped edge must be reported, not silent: {moved}"
    );
    assert!(
        h.edges(ws("work"), 2).await.is_empty(),
        "an edge survived with its endpoint in another store"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn moving_into_preferences_widens_the_scope_and_moving_out_narrows_the_reach() {
    let h = Harness::boot(&["work", "personal"]).await;
    h.save_scoped(
        ws("work"),
        1,
        "benedikt writes commit subjects in the imperative mood",
        "fresha/offers",
    )
    .await;

    let widened = h
        .ok(
            "memory.move",
            json!({"id": mid(1), "from": ws("work"), "to": pref()}),
        )
        .await;
    assert_eq!(widened["from_scope"], "fresha/offers");
    assert_eq!(
        widened["scope"], "workspace",
        "a project scope must be widened on the way in: {widened}"
    );

    let query = json!({"q": "imperative mood commit subjects"});
    let mut elsewhere = query.clone();
    elsewhere["ws"] = json!("personal");
    let hits = h.ok("search", elsewhere).await;
    assert!(finds(&hits, 1), "the preference did not travel: {hits}");

    let narrowed = h
        .ok(
            "memory.move",
            json!({"id": mid(1), "from": pref(), "to": ws("personal")}),
        )
        .await;
    assert_eq!(narrowed["scope"], "workspace", "{narrowed}");
    let mut from_work = query;
    from_work["ws"] = json!("work");
    assert!(
        !finds(&h.ok("search", from_work).await, 1),
        "it still reaches other workspaces after moving out"
    );
    assert_eq!(
        h.err("memory.get", json!({"origin": pref(), "id": mid(1)}))
            .await,
        NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_refuses_the_shared_store_by_name_and_reports_an_in_place_move() {
    let h = Harness::boot(&["work"]).await;
    h.save(ws("work"), 1, "a fact that stays put").await;

    for (from, to) in [(ws("shared"), ws("work")), (ws("work"), ws("shared"))] {
        assert_eq!(
            h.err("memory.move", json!({"id": mid(1), "from": from, "to": to}))
                .await,
            BAD_REQUEST,
            "the shared store was addressable by name"
        );
    }

    let in_place = h
        .ok(
            "memory.move",
            json!({"id": mid(1), "from": ws("work"), "to": ws("work")}),
        )
        .await;
    assert_eq!(in_place["moved"], false, "{in_place}");
    h.ok("memory.get", json!({"origin": ws("work"), "id": mid(1)}))
        .await;

    assert_eq!(
        h.err(
            "memory.move",
            json!({"id": mid(1), "from": ws("work"), "to": ws("nope")})
        )
        .await,
        NOT_FOUND
    );
    assert_eq!(
        h.err(
            "memory.move",
            json!({"id": mid(9), "from": ws("work"), "to": pref()})
        )
        .await,
        NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_move_finishes_and_a_colliding_id_conflicts() {
    let h = Harness::boot(&["work", "personal"]).await;
    let payload = "the espresso machine needs descaling every two months";
    h.save(ws("work"), 1, payload).await;
    h.save(ws("personal"), 1, payload).await;

    let resumed = h
        .ok(
            "memory.move",
            json!({"id": mid(1), "from": ws("work"), "to": ws("personal")}),
        )
        .await;
    assert_eq!(
        resumed["moved"], true,
        "an identical payload at the target must finish the move: {resumed}"
    );
    assert_eq!(
        h.err("memory.get", json!({"origin": ws("work"), "id": mid(1)}))
            .await,
        NOT_FOUND,
        "the source was not cleared"
    );

    h.save(ws("work"), 2, "one thing").await;
    h.save(ws("personal"), 2, "a different thing").await;
    assert_eq!(
        h.err(
            "memory.move",
            json!({"id": mid(2), "from": ws("work"), "to": ws("personal")})
        )
        .await,
        CONFLICT
    );
    let kept = h
        .ok("memory.get", json!({"origin": ws("work"), "id": mid(2)}))
        .await;
    assert_eq!(
        kept["content"], "one thing",
        "a conflicting move deleted the source"
    );
    let target = h
        .ok(
            "memory.get",
            json!({"origin": ws("personal"), "id": mid(2)}),
        )
        .await;
    assert_eq!(target["content"], "a different thing");
}

#[tokio::test(flavor = "multi_thread")]
async fn export_and_import_round_trip_including_the_graph() {
    let h = Harness::boot(&["work", "restore"]).await;
    h.save(ws("work"), 1, "first exported fact").await;
    h.save(ws("work"), 2, "second exported fact").await;
    h.pin(ws("work"), 1).await;
    h.supersede(ws("work"), 2, 1).await;

    let exported = h.export(ws("work")).await;
    assert_eq!(exported["version"], 2);
    assert_eq!(exported["memories"].as_array().expect("memories").len(), 2);
    assert_eq!(
        exported["links"].as_array().expect("links").len(),
        1,
        "the export must carry the graph: {exported}"
    );

    let report = h.import(ws("restore"), None, exported.clone()).await;
    assert_eq!(report["result"]["imported"], 2, "{report}");
    assert_eq!(report["result"]["unchanged"], 0);

    let restored = h.export(ws("restore")).await;
    assert_eq!(
        exported["memories"], restored["memories"],
        "the round trip drifted"
    );
    assert_eq!(
        exported["links"], restored["links"],
        "links must round-trip through export and import"
    );
    assert_eq!(h.edges(ws("restore"), 2).await.len(), 1);

    let again = h.import(ws("restore"), None, exported.clone()).await;
    assert_eq!(
        again["result"]["unchanged"], 2,
        "repeating the same default import must resume rather than conflict: {again}"
    );

    h.save(ws("restore"), 7, "a fact this dump never had").await;
    let polluted = h.import(ws("restore"), None, exported.clone()).await;
    assert_eq!(
        polluted["error"]["code"], CONFLICT,
        "a default import must refuse a target holding unrelated memories: {polluted}"
    );

    let merged = h
        .import(ws("restore"), Some("merge"), exported.clone())
        .await;
    assert_eq!(merged["result"]["imported"], 0, "{merged}");
    assert_eq!(merged["result"]["unchanged"], 2);

    let mut mutated = exported;
    mutated["memories"][0]["content"] = json!("mutated content");
    let conflicting = h.import(ws("restore"), Some("merge"), mutated).await;
    assert_eq!(conflicting["error"]["code"], CONFLICT, "{conflicting}");

    let hits = h
        .ok(
            "search",
            json!({"ws": "restore", "q": "second exported fact"}),
        )
        .await;
    assert!(
        finds(&hits, 2),
        "an imported memory is not recallable: {hits}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_v1_dump_imports_as_linkless() {
    let h = Harness::boot(&["old"]).await;
    let v1 = json!({
        "version": 1,
        "origin": ws("old"),
        "memories": [dumped_memory(1, "backup made before links existed", "workspace", "project")],
    });
    let report = h.import(ws("old"), None, v1).await;
    assert_eq!(report["result"]["imported"], 1, "{report}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_rejects_a_supersession_cycle_and_leaves_nothing_behind() {
    let h = Harness::boot(&["cyc"]).await;
    let doc = dump(
        ws("cyc"),
        json!([
            dumped_memory(1, "a", "workspace", "project"),
            dumped_memory(2, "b", "workspace", "project"),
        ]),
        json!([
            {"source": mid(1), "relation": "supersession", "target": mid(2), "directed": true},
            {"source": mid(2), "relation": "supersession", "target": mid(1), "directed": true},
        ]),
    );
    let rejected = h.import(ws("cyc"), None, doc).await;
    assert_eq!(
        rejected["error"]["code"], BAD_REQUEST,
        "a cyclic supersession dump must be rejected: {rejected}"
    );

    let listed = h.ok("memory.list", json!({"origin": ws("cyc")})).await;
    assert!(
        listed["memories"].as_array().expect("memories").is_empty(),
        "a rejected import left memories behind: {listed}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_default_import_conflicts_on_a_link_the_dump_lacks() {
    let h = Harness::boot(&["work"]).await;
    h.save(ws("work"), 1, "old deploy process").await;
    h.save(ws("work"), 2, "new deploy process").await;
    let linkless = h.export(ws("work")).await;
    h.supersede(ws("work"), 2, 1).await;

    let refused = h.import(ws("work"), None, linkless.clone()).await;
    assert_eq!(
        refused["error"]["code"], CONFLICT,
        "a default restore must not leave a link the dump never held: {refused}"
    );

    let merged = h.import(ws("work"), Some("merge"), linkless).await;
    assert!(
        merged.get("error").is_none(),
        "merge must keep unrelated state: {merged}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_keeps_preferences_and_workspaces_apart() {
    let h = Harness::boot(&["work"]).await;
    h.save(ws("work"), 1, "an internal work detail").await;
    h.save(pref(), 2, "prefers ripgrep").await;

    let work_dump = h.export(ws("work")).await;
    let preference_dump = h.export(pref()).await;

    for mode in [None, Some("merge")] {
        let crossed = h.import(pref(), mode, work_dump.clone()).await;
        assert_eq!(
            crossed["error"]["code"], BAD_REQUEST,
            "a workspace dump became preferences: {crossed}"
        );
        let crossed = h.import(ws("work"), mode, preference_dump.clone()).await;
        assert_eq!(
            crossed["error"]["code"], BAD_REQUEST,
            "a preference dump became workspace memories: {crossed}"
        );
    }

    let listed = h.ok("memory.list", json!({"origin": pref()})).await;
    assert_eq!(
        listed["memories"].as_array().expect("memories").len(),
        1,
        "a rejected dump still landed: {listed}"
    );

    let mut project_scoped = preference_dump;
    project_scoped["memories"][0]["scope"] = json!("fresha/offers");
    project_scoped["memories"][0]["id"] = json!(mid(3));
    let refused = h.import(pref(), Some("merge"), project_scoped).await;
    assert_eq!(
        refused["error"]["code"], BAD_REQUEST,
        "a project-scoped preference was accepted: {refused}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preference_import_refuses_links_no_caller_can_reach() {
    let h = Harness::boot(&["work"]).await;
    let doc = dump(
        pref(),
        json!([
            dumped_memory(1, "prefers ripgrep", "workspace", "user"),
            dumped_memory(2, "prefers fd", "workspace", "user"),
        ]),
        json!([{"source": mid(1), "relation": "supersession", "target": mid(2),
                "directed": true}]),
    );
    let refused = h.import(pref(), None, doc).await;
    assert_eq!(
        refused["error"]["code"], BAD_REQUEST,
        "a preference dump smuggled in unreachable edges: {refused}"
    );

    let listed = h.ok("memory.list", json!({"origin": pref()})).await;
    assert!(
        listed["memories"].as_array().expect("memories").is_empty(),
        "{listed}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_normalises_timestamps_and_refuses_impossible_ones() {
    let h = Harness::boot(&["restore"]).await;
    let with_times = |created: &str, updated: &str| {
        let mut memory = dumped_memory(1, "a restored fact", "workspace", "project");
        memory["created_at"] = json!(created);
        memory["updated_at"] = json!(updated);
        dump(ws("restore"), json!([memory]), json!([]))
    };

    for bad in [
        "2026-99-99T99:99:99Z",
        "2026-02-30T00:00:00Z",
        "2026-08-02T10:00:00+02:00",
        "2026-08-02 10:00:00Z",
    ] {
        let refused = h
            .import(
                ws("restore"),
                Some("merge"),
                with_times(bad, "2026-08-02T10:00:00Z"),
            )
            .await;
        assert_eq!(
            refused["error"]["code"], BAD_REQUEST,
            "accepted {bad}: {refused}"
        );
    }

    let accepted = h
        .import(
            ws("restore"),
            Some("merge"),
            with_times("2026-08-02T10:00:00.500Z", "2026-08-02T10:00:00.999Z"),
        )
        .await;
    assert!(accepted.get("error").is_none(), "{accepted}");

    let stored = h
        .ok("memory.get", json!({"origin": ws("restore"), "id": mid(1)}))
        .await;
    assert_eq!(stored["created_at"], "2026-08-02T10:00:00Z");
    assert_eq!(stored["updated_at"], "2026-08-02T10:00:00Z");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_can_be_created_retyped_and_removed() {
    let h = Harness::boot(&["work"]).await;
    h.save(ws("work"), 1, "old deploy process").await;
    h.save(ws("work"), 2, "new deploy process").await;

    h.supersede(ws("work"), 2, 1).await;
    let edges = h.edges(ws("work"), 1).await;
    assert_eq!(edges.len(), 1, "{edges:?}");
    assert_eq!(edges[0]["relation"], "supersession");
    assert_eq!(edges[0]["directed"], true);

    h.ok(
        "link.retype",
        json!({"origin": ws("work"), "id": mid(2), "target": mid(1),
               "relation": "support"}),
    )
    .await;
    let edges = h.edges(ws("work"), 1).await;
    assert_eq!(edges[0]["relation"], "support");
    assert_eq!(edges[0]["directed"], false);

    let removed = h
        .ok(
            "link.delete",
            json!({"origin": ws("work"), "id": mid(1), "target": mid(2)}),
        )
        .await;
    assert_eq!(removed["removed"], 1, "{removed}");
    assert!(h.edges(ws("work"), 1).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_links_graph_is_rooted_bounded_and_validated() {
    let h = Harness::boot(&["work"]).await;
    h.save(ws("work"), 1, "argocd deploy to staging").await;

    let graph = h
        .ok("link.graph", json!({"origin": ws("work"), "id": mid(1)}))
        .await;
    assert_eq!(graph["graph"]["metadata"]["root"], mid(1));
    assert_eq!(graph["graph"]["metadata"]["depth"], 2);
    assert_eq!(graph["graph"]["metadata"]["truncated"], false);
    assert!(
        graph["graph"]["edges"]
            .as_array()
            .expect("edges")
            .is_empty()
    );
    assert!(
        graph["graph"]["nodes"][&mid(1)]["label"].is_string(),
        "{graph}"
    );

    assert_eq!(
        h.err(
            "link.graph",
            json!({"origin": ws("work"), "id": mid(1), "depth": 99})
        )
        .await,
        BAD_REQUEST,
        "depth must be bounded"
    );
    assert_eq!(
        h.err("link.graph", json!({"origin": ws("work"), "id": mid(999)}))
            .await,
        NOT_FOUND
    );
}
