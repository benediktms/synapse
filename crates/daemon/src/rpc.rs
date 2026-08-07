use std::future::Future;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use api::ops::{self, SearchArgs};
use api::{
    ApiError, Backend, BackendError, ExportDoc, MoveBody, Origin, PatchMemoryBody, PutMemoryBody,
};
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

#[derive(Serialize)]
pub struct WorkspaceStatus {
    pub name: String,
    pub online: bool,
    pub last_synced_at: u64,
    pub pending_outbox: usize,
}

#[derive(Serialize)]
struct ReadyResponse {
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    problems: Option<String>,
}

#[derive(Serialize)]
struct SyncResponse {
    synced: bool,
}

#[derive(Serialize)]
struct WorkspaceCreatedResponse {
    workspace: String,
    created: bool,
}

#[derive(Serialize)]
struct UnlinkResponse {
    removed: usize,
}

#[derive(Deserialize)]
struct Request {
    id: u64,
    method: String,
    #[serde(default)]
    params: Value,
}

/// The wire method, parsed from its `namespace.verb` string form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Ping,
    Ready,
    Status,
    Sync,
    Search,
    Context,
    Export,
    Import,
    Workspace(WorkspaceMethod),
    Memory(MemoryMethod),
    Link(LinkMethod),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceMethod {
    Create,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoryMethod {
    Save,
    Edit,
    Forget,
    Move,
    Get,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkMethod {
    Graph,
    Create,
    Retype,
    Delete,
}

impl Method {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "ping" => Self::Ping,
            "ready" => Self::Ready,
            "status" => Self::Status,
            "sync" => Self::Sync,
            "search" => Self::Search,
            "context" => Self::Context,
            "export" => Self::Export,
            "import" => Self::Import,
            _ => match raw.split_once('.')? {
                ("workspace", "create") => Self::Workspace(WorkspaceMethod::Create),
                ("workspace", "list") => Self::Workspace(WorkspaceMethod::List),
                ("memory", "save") => Self::Memory(MemoryMethod::Save),
                ("memory", "edit") => Self::Memory(MemoryMethod::Edit),
                ("memory", "forget") => Self::Memory(MemoryMethod::Forget),
                ("memory", "move") => Self::Memory(MemoryMethod::Move),
                ("memory", "get") => Self::Memory(MemoryMethod::Get),
                ("memory", "list") => Self::Memory(MemoryMethod::List),
                ("link", "graph") => Self::Link(LinkMethod::Graph),
                ("link", "create") => Self::Link(LinkMethod::Create),
                ("link", "retype") => Self::Link(LinkMethod::Retype),
                ("link", "delete") => Self::Link(LinkMethod::Delete),
                _ => return None,
            },
        })
    }
}

#[derive(Serialize)]
struct Success {
    jsonrpc: &'static str,
    id: u64,
    result: Value,
}

#[derive(Serialize)]
struct Failure {
    jsonrpc: &'static str,
    id: u64,
    error: ErrorObj,
}

#[derive(Serialize)]
struct ErrorObj {
    code: i64,
    message: String,
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

pub fn bind_listener(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() {
        // Tolerate a stale socket left behind by a killed daemon; the flock gate above ensures we
        // only reach here when no live daemon holds the lock.
        let _ = std::fs::remove_file(path);
    }
    UnixListener::bind(path)
}

pub async fn serve<H: RpcHost>(listener: UnixListener, host: H) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let host = host.clone();
        tokio::spawn(async move {
            let _ = handle_conn(stream, host).await;
        });
    }
}

async fn handle_conn<H: RpcHost>(stream: UnixStream, host: H) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // Newline-framed, per-command short connection: read one request, reply, close.
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let mut writer = reader.into_inner();
    let response = dispatch(&line, &host).await;
    let mut bytes = response.into_bytes();
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Deserialize)]
struct OriginParams {
    origin: Origin,
}

#[derive(Deserialize)]
struct IdParams {
    origin: Origin,
    id: String,
}

#[derive(Deserialize)]
struct SaveParams {
    origin: Origin,
    id: String,
    #[serde(flatten)]
    body: PutMemoryBody,
}

#[derive(Deserialize)]
struct EditParams {
    origin: Origin,
    id: String,
    #[serde(flatten)]
    body: PatchMemoryBody,
}

#[derive(Deserialize)]
struct MoveParams {
    id: String,
    #[serde(flatten)]
    body: MoveBody,
}

#[derive(Deserialize)]
struct SearchParams {
    ws: Option<String>,
    q: String,
    scope: Option<String>,
    limit: Option<usize>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    links_scope: bool,
}

#[derive(Deserialize)]
struct ContextParams {
    origin: Origin,
    project: Option<String>,
}

#[derive(Deserialize)]
struct GraphParams {
    origin: Origin,
    id: String,
    depth: Option<usize>,
}

#[derive(Deserialize)]
struct LinkParams {
    origin: Origin,
    id: String,
    target: String,
    relation: String,
}

#[derive(Deserialize)]
struct UnlinkParams {
    origin: Origin,
    id: String,
    target: String,
}

#[derive(Deserialize)]
struct ImportParams {
    origin: Origin,
    mode: Option<String>,
    doc: ExportDoc,
}

#[derive(Deserialize)]
struct WorkspaceParams {
    name: String,
}

#[derive(Deserialize)]
struct SyncParams {
    #[serde(default)]
    origin: Option<Origin>,
}

pub async fn dispatch<H: RpcHost>(line: &str, host: &H) -> String {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => return fail(0, -32700, "invalid request"),
    };
    let Some(method) = Method::parse(&req.method) else {
        return fail(req.id, -32601, &format!("method not found: {}", req.method));
    };
    match call(method, req.params, host).await {
        Ok(result) => serde_json::to_string(&Success {
            jsonrpc: "2.0",
            id: req.id,
            result,
        })
        .unwrap_or_default(),
        Err(e) => fail(req.id, e.code, &e.message),
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
            encode(&SyncResponse { synced: true })
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
    serde_json::to_string(&Failure {
        jsonrpc: "2.0",
        id,
        error: ErrorObj {
            code,
            message: message.to_string(),
        },
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

        let resp = call(&app, "memory.save", json!({"origin": "preference", "id": ID, "content": "prefers rebase", "kind": "feedback", "scope": "workspace"})).await;
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
}
