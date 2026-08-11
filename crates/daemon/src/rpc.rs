use std::future::Future;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

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

#[cfg(unix)]
pub fn bind_listener(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        // Tolerate a stale socket left behind by a killed daemon; the flock gate above ensures we
        // only reach here when no live daemon holds the lock.
        let _ = std::fs::remove_file(path);
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    // The socket is the auth boundary; never leave its mode to the process umask.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(unix)]
pub async fn serve<H: RpcHost>(listener: tokio::net::UnixListener, host: H) {
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

/// A named-pipe "listener": the pipe name plus the pre-created next instance.
/// `first_pipe_instance` makes a second daemon fail at bind, mirroring a taken socket.
#[cfg(windows)]
pub struct PipeListener {
    name: String,
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
pub fn bind_listener(path: &Path) -> std::io::Result<PipeListener> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = path
        .to_str()
        .ok_or_else(|| std::io::Error::other(format!("pipe name is not UTF-8: {path:?}")))?
        .to_string();
    let next = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)?;
    Ok(PipeListener { name, next })
}

#[cfg(windows)]
pub async fn serve<H: RpcHost>(mut listener: PipeListener, host: H) {
    use tokio::net::windows::named_pipe::ServerOptions;
    loop {
        if let Err(e) = listener.next.connect().await {
            tracing::warn!("pipe connect failed: {e}");
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        let replacement = loop {
            match ServerOptions::new().create(&listener.name) {
                Ok(server) => break server,
                Err(e) => {
                    tracing::warn!("cannot create the next pipe instance: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        let stream = std::mem::replace(&mut listener.next, replacement);
        let host = host.clone();
        tokio::spawn(async move {
            let _ = handle_conn(stream, host).await;
        });
    }
}

async fn handle_conn<H: RpcHost, S>(stream: S, host: H) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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
