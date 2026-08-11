use std::future::Future;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use api::ops::{self, SearchArgs};
use api::rpc::{
    ContextParams, EditParams, ErrorObj, GraphParams, IdParams, ImportParams, JSONRPC_VERSION,
    LinkMethod, LinkParams, MemoryMethod, Method, MoveParams, OriginParams, ReadyResponse, Request,
    Response, SaveParams, SearchParams, SyncParams, UnlinkParams, UnlinkResponse,
    WorkspaceCreatedResponse, WorkspaceMethod, WorkspaceParams, WorkspaceStatus,
};
use api::{ApiError, Backend, BackendError, Origin};
use domain::Workspace;

/// What the daemon adds on top of the transport-neutral `Backend` surface: replica
/// freshness introspection and forced sync. Tests drive `dispatch` with any Backend.
pub trait RpcHost: Backend {
    fn statuses(&self) -> impl Future<Output = Vec<WorkspaceStatus>> + Send;
    /// Sync one replica, or every open replica when `only` is None.
    fn sync_replicas(
        &self,
        only: Option<&Workspace>,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;
}

struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn invalid_params(detail: impl std::fmt::Display) -> Self {
        Self {
            code: -32602,
            message: format!("invalid params: {detail}"),
        }
    }
}

impl From<ApiError> for RpcError {
    fn from(err: ApiError) -> Self {
        let (code, message) = match err {
            ApiError::BadRequest(m) => (-32602, m),
            ApiError::NotFound(m) => (-32001, m),
            ApiError::Conflict(m) => (-32002, m),
            ApiError::Unready(m) => (-32003, m),
            ApiError::Unauthorized => (-32004, "unauthorized".into()),
            ApiError::Internal(m) => (-32000, m),
        };
        Self { code, message }
    }
}

impl From<BackendError> for RpcError {
    fn from(err: BackendError) -> Self {
        ApiError::from(err).into()
    }
}

/// Mirrors the HTTP transport's DefaultBodyLimit; without a cap an endless byte stream
/// with no newline would be buffered until the daemon is OOM-killed.
const MAX_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const IMPORT_TIMEOUT: Duration = Duration::from_secs(600);

pub fn bind_listener(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        // Tolerate a stale socket left behind by a killed daemon; the flock gate above ensures we
        // only reach here when no live daemon holds the lock.
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    // The socket is the auth boundary; never leave its mode to the process umask.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn serve<H: RpcHost>(listener: UnixListener, host: H) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let host = host.clone();
        tokio::spawn(async move {
            let _ = handle_conn(stream, host).await;
        });
    }
}

async fn handle_conn<H: RpcHost>(stream: UnixStream, host: H) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream).take(MAX_REQUEST_BYTES);
    let mut line = String::new();
    // Newline-framed, per-command short connection: read one request, reply, close.
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Ok(());
    }
    let mut writer = reader.into_inner().into_inner();
    let response = if read as u64 == MAX_REQUEST_BYTES && !line.ends_with('\n') {
        fail(0, -32600, "request exceeds the 32 MiB limit")
    } else {
        dispatch(&line, &host).await
    };
    let mut bytes = response.into_bytes();
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn dispatch<H: RpcHost>(line: &str, host: &H) -> String {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => return fail(0, -32700, "invalid request"),
    };
    let Some(method) = Method::parse(&req.method) else {
        return fail(req.id, -32601, &format!("method not found: {}", req.method));
    };
    let deadline = if method == Method::Import {
        IMPORT_TIMEOUT
    } else {
        REQUEST_TIMEOUT
    };
    match tokio::time::timeout(deadline, call(method, req.params, host)).await {
        Ok(Ok(result)) => serde_json::to_string(&Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: req.id,
            result: Some(result),
            error: None,
        })
        .unwrap_or_default(),
        Ok(Err(e)) => fail(req.id, e.code, &e.message),
        Err(_) => fail(req.id, -32000, "request timed out"),
    }
}

async fn call<H: RpcHost>(method: Method, params: Value, host: &H) -> Result<Value, RpcError> {
    match method {
        Method::Ping => return Ok(Value::from("pong")),
        Method::Ready => {
            return encode(&match host.ready() {
                Ok(()) => ReadyResponse {
                    ready: true,
                    problems: None,
                },
                Err(problems) => ReadyResponse {
                    ready: false,
                    problems: Some(problems),
                },
            });
        }
        Method::Status => return encode(&host.statuses().await),
        _ => {}
    }
    if let Err(reason) = host.ready() {
        return Err(ApiError::Unready(reason).into());
    }
    match method {
        Method::Ping | Method::Ready | Method::Status => unreachable!("handled above"),
        Method::Sync => {
            let p: SyncParams = parse(params)?;
            let ws = p.origin.as_ref().map(ops::workspace_of).transpose()?;
            host.sync_replicas(ws.as_ref()).await?;
            // A sync fails open when the primary is unreachable, so the verdict is the
            // post-sync per-replica status, not a bare success flag.
            encode(&host.statuses().await)
        }
        Method::Workspace(WorkspaceMethod::Create) => {
            let p: WorkspaceParams = parse(params)?;
            let (created, dto) = ops::create_workspace(host, &p.name).await?;
            encode(&WorkspaceCreatedResponse {
                workspace: dto.workspace,
                created,
            })
        }
        Method::Workspace(WorkspaceMethod::List) => encode(&ops::workspaces(host).await?),
        Method::Memory(MemoryMethod::Save) => {
            let p: SaveParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            let mut body = p.body;
            if matches!(p.origin, Origin::Preference) {
                // Preferences apply everywhere; mirror the HTTP route, which never
                // accepts a scope for them.
                body.scope = domain::Scope::Workspace.as_str().to_string();
            }
            encode(&ops::save(host, &ws, &p.id, body).await?)
        }
        Method::Memory(MemoryMethod::Edit) => {
            let p: EditParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            encode(&ops::edit(host, &ws, &p.id, p.body).await?)
        }
        Method::Memory(MemoryMethod::Forget) => {
            let p: IdParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            ops::forget(host, &ws, &p.id).await?;
            Ok(Value::Null)
        }
        Method::Memory(MemoryMethod::Move) => {
            let p: MoveParams = parse(params)?;
            encode(&ops::move_memory(host, &p.id, p.body).await?)
        }
        Method::Memory(MemoryMethod::Get) => {
            let p: IdParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            encode(&ops::fetch(host, &ws, &p.id).await?)
        }
        Method::Memory(MemoryMethod::List) => {
            let p: OriginParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            encode(&ops::list(host, &ws).await?)
        }
        Method::Search => {
            let p: SearchParams = parse(params)?;
            let args = SearchArgs {
                ws: p.ws,
                q: p.q,
                scope: p.scope,
                limit: p.limit,
                all: p.all,
                links_scope: p.links_scope,
            };
            encode(&ops::search(host, args).await?)
        }
        Method::Context => {
            let p: ContextParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            encode(&ops::context(host, &ws, p.project.as_deref()).await?)
        }
        Method::Link(LinkMethod::Graph) => {
            let p: GraphParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            encode(&ops::links_graph(host, &ws, &p.id, p.depth).await?)
        }
        Method::Link(LinkMethod::Create) => {
            let p: LinkParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            ops::create_link(host, &ws, &p.id, &p.target, &p.relation).await?;
            Ok(Value::Null)
        }
        Method::Link(LinkMethod::Retype) => {
            let p: LinkParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            ops::retype_link(host, &ws, &p.id, &p.target, &p.relation).await?;
            Ok(Value::Null)
        }
        Method::Link(LinkMethod::Delete) => {
            let p: UnlinkParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            let removed = ops::delete_link(host, &ws, &p.id, &p.target).await?;
            encode(&UnlinkResponse { removed })
        }
        Method::Export => {
            let p: OriginParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            encode(&ops::export(host, &ws).await?)
        }
        Method::Import => {
            let p: ImportParams = parse(params)?;
            let ws = ops::workspace_of(&p.origin)?;
            encode(&ops::import(host, &ws, p.mode.as_deref(), p.doc).await?)
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(RpcError::invalid_params)
}

fn encode<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(internal)
}

fn internal(err: impl std::fmt::Display) -> RpcError {
    RpcError {
        code: -32000,
        message: err.to_string(),
    }
}

fn fail(id: u64, code: i64, message: &str) -> String {
    serde_json::to_string(&Response {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        result: None,
        error: Some(ErrorObj {
            code,
            message: message.to_string(),
        }),
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use adapters_fastembed::{DIMENSION, FastEmbedder, MODEL_NAME};
    use adapters_libsql::LibsqlStore;
    use domain::Workspace;
    use serde_json::{Value, json};

    use super::dispatch;
    use crate::DaemonApp;

    // The daemon test binary must link only the libsql engine: a dev-dependency on the
    // sqlite-backed server crate pulls libsqlite3-sys in as well, and the two bundled
    // sqlite3.c archives collide at link time on Linux.
    async fn boot(dir: &std::path::Path) -> DaemonApp {
        let embedder =
            tokio::task::spawn_blocking(|| Arc::new(FastEmbedder::new().expect("model init")))
                .await
                .expect("join");
        let mut stores = HashMap::new();
        for ws in [Workspace::new("work").unwrap(), Workspace::shared()] {
            let db = libsql::Builder::new_local(dir.join(format!("{ws}.db")))
                .build()
                .await
                .expect("local db");
            let store = LibsqlStore::init(db, MODEL_NAME, DIMENSION)
                .await
                .expect("store init");
            stores.insert(ws, Arc::new(store));
        }
        DaemonApp::for_tests(dir.to_path_buf(), embedder, stores)
    }

    async fn call(app: &DaemonApp, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        serde_json::from_str(&dispatch(&line.to_string(), app).await).expect("valid response")
    }

    const ID: &str = "m_0000000000000000000001";

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_covers_the_memory_surface() {
        let dir = tempfile::tempdir().unwrap();
        let app = boot(dir.path()).await;

        let resp = call(&app, "ping", Value::Null).await;
        assert_eq!(resp["result"], "pong");

        let resp = call(&app, "workspace.list", Value::Null).await;
        assert_eq!(resp["result"]["workspaces"], json!(["work"]));

        let resp = call(&app, "workspace.create", json!({"name": "other"})).await;
        assert_eq!(
            resp["error"]["code"], -32001,
            "no scoped orgs configured, so provisioning must fail: {resp}"
        );

        let params = json!({
            "origin": {"workspace": "work"},
            "id": ID,
            "content": "deploy staging offers",
            "title": "Offers deploy to staging",
            "kind": "reference",
            "scope": "workspace",
        });
        let resp = call(&app, "memory.save", params).await;
        assert_eq!(resp["result"]["created"], true, "{resp}");
        assert_eq!(resp["result"]["memory"]["content"], "deploy staging offers");

        let resp = call(
            &app,
            "memory.get",
            json!({"origin": {"workspace": "work"}, "id": ID}),
        )
        .await;
        assert_eq!(resp["result"]["content"], "deploy staging offers");

        let resp = call(&app, "search", json!({"ws": "work", "q": "deploy staging"})).await;
        assert_eq!(resp["result"]["hits"][0]["id"], ID, "{resp}");

        let resp = call(&app, "export", json!({"origin": {"workspace": "work"}})).await;
        assert_eq!(resp["result"]["memories"][0]["id"], ID);

        let resp = call(
            &app,
            "memory.forget",
            json!({"origin": {"workspace": "work"}, "id": ID}),
        )
        .await;
        assert!(resp["result"].is_null(), "{resp}");
        let resp = call(
            &app,
            "memory.get",
            json!({"origin": {"workspace": "work"}, "id": ID}),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32001, "{resp}");

        let resp = call(&app, "memory.save", json!({"origin": "preference", "id": ID, "content": "prefers rebase", "title": "Prefers rebase", "kind": "feedback", "scope": "workspace"})).await;
        assert_eq!(resp["result"]["created"], true, "{resp}");
        let resp = call(&app, "memory.list", json!({"origin": "preference"})).await;
        assert_eq!(resp["result"]["memories"][0]["content"], "prefers rebase");

        let resp = call(&app, "nope", Value::Null).await;
        assert_eq!(resp["error"]["code"], -32601);
        let resp = call(&app, "memory.get", json!({"origin": {"workspace": "work"}})).await;
        assert_eq!(resp["error"]["code"], -32602, "{resp}");
        let resp = dispatch("not json", &app).await;
        assert!(resp.contains("-32700"), "{resp}");
    }

    /// A dry run of the title migration over real data, driven through the RPC surface the CLI
    /// actually calls, with the real embedding model rather than a stub.
    ///
    /// It binds only the databases named in `SYNAPSE_DRYRUN_DBS` — never the live ones. This is
    /// deliberate: `DaemonApp::boot` adopts every database in the org whose name parses as a
    /// workspace, so a second daemon pointed at this org would migrate the live primaries too.
    /// Branch the real databases first (`turso db create <name>-dryrun --from-db <name>`) and
    /// delete the branches afterwards, or the next daemon boot adopts them as workspaces.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Turso Cloud credentials, branch databases, and network"]
    async fn a_dry_run_of_the_title_migration_over_real_data() {
        let Some(token) = std::env::var("SYNAPSE_TURSO_TEST_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        else {
            eprintln!("skipping: SYNAPSE_TURSO_TEST_TOKEN is not set");
            return;
        };
        let org =
            std::env::var("SYNAPSE_TURSO_TEST_ORG").unwrap_or_else(|_| "benediktms".to_string());
        let names: Vec<String> = std::env::var("SYNAPSE_DRYRUN_DBS")
            .expect("SYNAPSE_DRYRUN_DBS must name the branch databases")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let platform = adapters_libsql::TursoPlatform::new();
        let group = platform.ensure_group(&org, &token).await.unwrap();
        let db_token = platform.mint_db_token(&org, &token, &group).await.unwrap();
        let (dbs, _) = platform.list_databases(&org, &token).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let embedder =
            tokio::task::spawn_blocking(|| Arc::new(FastEmbedder::new().expect("model init")))
                .await
                .expect("join");
        let mut stores = HashMap::new();
        for name in &names {
            let db = dbs
                .iter()
                .find(|db| &db.name == name)
                .unwrap_or_else(|| panic!("no database named {name} in {org}"));
            // The open that migrates: these branches predate the title column.
            let store = LibsqlStore::open(
                dir.path().join(format!("{name}.db")),
                db.url.clone(),
                db_token.clone(),
                MODEL_NAME,
                DIMENSION,
            )
            .await
            .unwrap_or_else(|e| panic!("open {name}: {e}"));
            stores.insert(Workspace::new(name).unwrap(), Arc::new(store));
        }
        let app = DaemonApp::for_tests(dir.path().to_path_buf(), embedder, stores);
        let ws = names.first().expect("at least one branch database");

        // The drill writes one memory of its own. Clearing it up front keeps a re-run against
        // the same branches honest, whatever an earlier run left behind.
        const DRILL_ID: &str = "m_0000000000000000009999";
        call(
            &app,
            "memory.forget",
            json!({"origin": {"workspace": ws}, "id": DRILL_ID}),
        )
        .await;

        // Every migrated row still reads back, with an empty title and a usable short form.
        for name in &names {
            let resp = call(&app, "memory.list", json!({"origin": {"workspace": name}})).await;
            let memories = resp["result"]["memories"].as_array().unwrap();
            assert!(!memories.is_empty(), "{name} came back empty: {resp}");
            for memory in memories {
                assert_eq!(memory["title"], "", "{name} row gained a title");
                let short = domain::short_form(
                    memory["title"].as_str().unwrap(),
                    memory["content"].as_str().unwrap(),
                );
                assert!(!short.is_empty(), "empty short form for {}", memory["id"]);
                assert!(
                    short.chars().count() <= domain::TITLE_MAX_CHARS,
                    "{short:?} exceeds the cap"
                );
            }
            eprintln!("{name}: {} memories migrated and readable", memories.len());
        }

        // A title whose words appear nowhere in the body: the vector lane is the only thing
        // that can find it, so this is what proves the title reached the embedding.
        let params = json!({
            "origin": {"workspace": ws},
            "id": DRILL_ID,
            "content": "The nightly job writes its report to the bucket at 03:00 UTC.",
            "title": "Marmalade quokka telemetry",
            "kind": "reference",
            "scope": "workspace",
        });
        let resp = call(&app, "memory.save", params).await;
        assert_eq!(resp["result"]["created"], true, "{resp}");

        let resp = call(&app, "search", json!({"ws": ws, "q": "marmalade quokka"})).await;
        let hits = resp["result"]["hits"].as_array().unwrap();
        assert_eq!(
            hits.first().map(|h| &h["id"]),
            Some(&json!(DRILL_ID)),
            "a title-only phrase did not recall its memory: {resp}"
        );
        eprintln!("recall by a title-only phrase: hit at rank 1 among real data");

        // A title-only edit must re-embed, or the new title is unrecallable.
        let resp = call(
            &app,
            "memory.edit",
            json!({"origin": {"workspace": ws}, "id": DRILL_ID, "title": "Rhubarb axolotl ledger"}),
        )
        .await;
        assert_eq!(resp["result"]["title"], "Rhubarb axolotl ledger", "{resp}");
        let resp = call(&app, "search", json!({"ws": ws, "q": "rhubarb axolotl"})).await;
        assert_eq!(
            resp["result"]["hits"][0]["id"], DRILL_ID,
            "an edited title did not re-embed: {resp}"
        );
        eprintln!("recall by an edited title: hit at rank 1");

        // What the session-start digest would print for real memories.
        let resp = call(&app, "context", json!({"origin": {"workspace": ws}})).await;
        let entries = resp["result"]["recent_project"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let pinned = resp["result"]["pinned"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for entry in pinned.iter().chain(&entries).take(5) {
            eprintln!(
                "digest: - [{}] {}",
                entry["id"].as_str().unwrap(),
                domain::short_form(
                    entry["title"].as_str().unwrap_or(""),
                    entry["content"].as_str().unwrap()
                )
            );
        }

        call(
            &app,
            "memory.forget",
            json!({"origin": {"workspace": ws}, "id": DRILL_ID}),
        )
        .await;
    }
}
