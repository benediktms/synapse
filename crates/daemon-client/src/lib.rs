//! Blocking JSON-RPC client for the synapse daemon: newline-framed requests over the
//! daemon's unix socket, one short connection per command, plus the daemon's on-disk
//! config schema and spawn-on-demand lifecycle.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use api::rpc::{
    ContextParams, EditParams, GraphParams, IdParams, ImportParams, JSONRPC_VERSION, LinkMethod,
    LinkParams, MemoryMethod, Method, MoveParams, OriginParams, ReadyResponse, Request, Response,
    SaveParams, SaveResponse, SearchParams, SyncParams, UnlinkParams, WorkspaceCreatedResponse,
    WorkspaceMethod, WorkspaceParams, WorkspaceStatus,
};
use api::{
    ContextResponse, ExportDoc, GraphDto, ImportReport, ListResponse, MemoryDto, MoveBody,
    MoveResponse, Origin, PatchMemoryBody, PutMemoryBody, SearchResponse,
};

/// The daemon's state directory: `$SYNAPSE_STATE_DIR/daemon`, or the XDG state home.
/// The daemon binary and every client must derive the same path, and a cwd-relative
/// fallback would scatter state wherever the process happened to start.
pub fn state_dir() -> Result<PathBuf, String> {
    let base = if let Some(dir) = env_path("SYNAPSE_STATE_DIR") {
        dir
    } else if let Some(dir) = env_path("XDG_STATE_HOME") {
        dir.join("synapse")
    } else if let Some(home) = env_path("HOME") {
        home.join(".local/state").join("synapse")
    } else {
        return Err(
            "cannot locate the daemon state directory: set HOME or SYNAPSE_STATE_DIR".to_string(),
        );
    };
    Ok(base.join("daemon"))
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon.sock")
}

pub fn config_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon.toml")
}

/// One org this machine replicates, with its org-scoped Turso token.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScopedOrg {
    pub name: String,
    pub token: String,
}

/// The daemon's on-disk config, written by `syn setup` and read by the daemon at boot.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DaemonConfig {
    pub scoped_orgs: Vec<ScopedOrg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
}

impl DaemonConfig {
    pub fn auto_update(&self) -> bool {
        self.auto_update.unwrap_or(true)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        toml::from_str(&raw).map_err(|e| e.to_string())
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug)]
pub enum DaemonError {
    Transport(String),
    Rpc { code: i64, message: String },
    Decode(String),
}

impl DaemonError {
    /// Mirrors the HTTP client's split: only a definitive rejection is safe to give up
    /// on. -32000 covers internal errors and timeouts; -32003 is the not-ready gate.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Decode(_) => true,
            Self::Rpc { code, .. } => matches!(code, -32000 | -32003),
        }
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "cannot reach the synapse daemon: {message}"),
            Self::Rpc { message, .. } => write!(f, "{message}"),
            Self::Decode(message) => write!(f, "unreadable daemon response: {message}"),
        }
    }
}

impl std::error::Error for DaemonError {}

pub struct DaemonClient {
    socket: PathBuf,
    timeout: Duration,
}

impl DaemonClient {
    pub fn new(socket: PathBuf, timeout: Duration) -> Self {
        Self { socket, timeout }
    }

    /// Connect, send one request line, read one response line, close.
    fn call<R: DeserializeOwned>(&self, method: Method, params: Value) -> Result<R, DaemonError> {
        let stream = UnixStream::connect(&self.socket)
            .map_err(|e| DaemonError::Transport(format!("{} ({e})", self.socket.display())))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|()| stream.set_write_timeout(Some(self.timeout)))
            .map_err(|e| DaemonError::Transport(e.to_string()))?;
        let request = Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: 1,
            method: method.as_str().to_string(),
            params,
        };
        let mut line =
            serde_json::to_string(&request).map_err(|e| DaemonError::Decode(e.to_string()))?;
        line.push('\n');
        let mut writer = stream
            .try_clone()
            .map_err(|e| DaemonError::Transport(e.to_string()))?;
        writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|e| DaemonError::Transport(e.to_string()))?;
        let mut reply = String::new();
        BufReader::new(stream)
            .read_line(&mut reply)
            .map_err(|e| DaemonError::Transport(e.to_string()))?;
        let response: Response =
            serde_json::from_str(&reply).map_err(|e| DaemonError::Decode(e.to_string()))?;
        if let Some(error) = response.error {
            return Err(DaemonError::Rpc {
                code: error.code,
                message: error.message,
            });
        }
        let result = response.result.unwrap_or(Value::Null);
        serde_json::from_value(result).map_err(|e| DaemonError::Decode(e.to_string()))
    }

    fn params<P: Serialize>(params: &P) -> Result<Value, DaemonError> {
        serde_json::to_value(params).map_err(|e| DaemonError::Decode(e.to_string()))
    }

    pub fn ping(&self) -> Result<(), DaemonError> {
        let _: String = self.call(Method::Ping, Value::Null)?;
        Ok(())
    }

    pub fn ready(&self) -> Result<ReadyResponse, DaemonError> {
        self.call(Method::Ready, Value::Null)
    }

    pub fn status(&self) -> Result<Vec<WorkspaceStatus>, DaemonError> {
        self.call(Method::Status, Value::Null)
    }

    /// Forces a sync and returns the post-sync per-replica status: an unreachable
    /// primary fails open, so `online` is the field that says whether anything moved.
    pub fn sync(&self, origin: Option<Origin>) -> Result<Vec<WorkspaceStatus>, DaemonError> {
        self.call(Method::Sync, Self::params(&SyncParams { origin })?)
    }

    pub fn create_workspace(&self, name: &str) -> Result<WorkspaceCreatedResponse, DaemonError> {
        self.call(
            Method::Workspace(WorkspaceMethod::Create),
            Self::params(&WorkspaceParams {
                name: name.to_string(),
            })?,
        )
    }

    pub fn workspaces(&self) -> Result<Vec<String>, DaemonError> {
        let body: api::WorkspacesResponse =
            self.call(Method::Workspace(WorkspaceMethod::List), Value::Null)?;
        Ok(body.workspaces)
    }

    pub fn save(
        &self,
        origin: Origin,
        id: &str,
        body: PutMemoryBody,
    ) -> Result<SaveResponse, DaemonError> {
        self.call(
            Method::Memory(MemoryMethod::Save),
            Self::params(&SaveParams {
                origin,
                id: id.to_string(),
                body,
            })?,
        )
    }

    pub fn edit(
        &self,
        origin: Origin,
        id: &str,
        body: PatchMemoryBody,
    ) -> Result<MemoryDto, DaemonError> {
        self.call(
            Method::Memory(MemoryMethod::Edit),
            Self::params(&EditParams {
                origin,
                id: id.to_string(),
                body,
            })?,
        )
    }

    pub fn forget(&self, origin: Origin, id: &str) -> Result<(), DaemonError> {
        let _: Value = self.call(
            Method::Memory(MemoryMethod::Forget),
            Self::params(&IdParams {
                origin,
                id: id.to_string(),
            })?,
        )?;
        Ok(())
    }

    pub fn move_memory(&self, id: &str, body: MoveBody) -> Result<MoveResponse, DaemonError> {
        self.call(
            Method::Memory(MemoryMethod::Move),
            Self::params(&MoveParams {
                id: id.to_string(),
                body,
            })?,
        )
    }

    pub fn get(&self, origin: Origin, id: &str) -> Result<MemoryDto, DaemonError> {
        self.call(
            Method::Memory(MemoryMethod::Get),
            Self::params(&IdParams {
                origin,
                id: id.to_string(),
            })?,
        )
    }

    pub fn list(&self, origin: Origin) -> Result<Vec<MemoryDto>, DaemonError> {
        let body: ListResponse = self.call(
            Method::Memory(MemoryMethod::List),
            Self::params(&OriginParams { origin })?,
        )?;
        Ok(body.memories)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn search(
        &self,
        workspace: &str,
        query: &str,
        scope: Option<&str>,
        limit: usize,
        all_workspaces: bool,
        links_in_scope: bool,
    ) -> Result<SearchResponse, DaemonError> {
        self.call(
            Method::Search,
            Self::params(&SearchParams {
                ws: (!all_workspaces).then(|| workspace.to_string()),
                q: query.to_string(),
                scope: scope.map(str::to_string),
                limit: Some(limit),
                all: all_workspaces,
                links_scope: links_in_scope,
            })?,
        )
    }

    pub fn context(
        &self,
        workspace: &str,
        project: Option<&str>,
    ) -> Result<ContextResponse, DaemonError> {
        self.call(
            Method::Context,
            Self::params(&ContextParams {
                origin: Origin::Workspace(workspace.to_string()),
                project: project.map(str::to_string),
            })?,
        )
    }

    pub fn links(&self, workspace: &str, id: &str, depth: usize) -> Result<GraphDto, DaemonError> {
        self.call(
            Method::Link(LinkMethod::Graph),
            Self::params(&GraphParams {
                origin: Origin::Workspace(workspace.to_string()),
                id: id.to_string(),
                depth: Some(depth),
            })?,
        )
    }

    pub fn link(
        &self,
        workspace: &str,
        source: &str,
        target: &str,
        relation: &str,
    ) -> Result<(), DaemonError> {
        let _: Value = self.call(
            Method::Link(LinkMethod::Create),
            Self::params(&LinkParams {
                origin: Origin::Workspace(workspace.to_string()),
                id: source.to_string(),
                target: target.to_string(),
                relation: relation.to_string(),
            })?,
        )?;
        Ok(())
    }

    pub fn retype_link(
        &self,
        workspace: &str,
        a: &str,
        b: &str,
        relation: &str,
    ) -> Result<(), DaemonError> {
        let _: Value = self.call(
            Method::Link(LinkMethod::Retype),
            Self::params(&LinkParams {
                origin: Origin::Workspace(workspace.to_string()),
                id: a.to_string(),
                target: b.to_string(),
                relation: relation.to_string(),
            })?,
        )?;
        Ok(())
    }

    pub fn unlink(&self, workspace: &str, a: &str, b: &str) -> Result<(), DaemonError> {
        let _: Value = self.call(
            Method::Link(LinkMethod::Delete),
            Self::params(&UnlinkParams {
                origin: Origin::Workspace(workspace.to_string()),
                id: a.to_string(),
                target: b.to_string(),
            })?,
        )?;
        Ok(())
    }

    pub fn export(&self, origin: Origin) -> Result<ExportDoc, DaemonError> {
        self.call(Method::Export, Self::params(&OriginParams { origin })?)
    }

    pub fn import(
        &self,
        origin: Origin,
        merge: bool,
        doc: ExportDoc,
    ) -> Result<ImportReport, DaemonError> {
        self.call(
            Method::Import,
            Self::params(&ImportParams {
                origin,
                mode: merge.then(|| "merge".to_string()),
                doc,
            })?,
        )
    }
}

const SPAWN_POLL: Duration = Duration::from_millis(200);
const SPAWN_DEADLINE: Duration = Duration::from_secs(30);

pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon.log")
}

/// Probe the socket; when no daemon answers, spawn `synd` detached with its stderr
/// appended to `<state>/daemon.log`, then poll until it answers. A child that exits
/// during the poll fails immediately with the log tail instead of running out the clock.
pub fn ensure_running(client: &DaemonClient, state_dir: &Path) -> Result<(), String> {
    if client.ping().is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("cannot create {}: {e}", state_dir.display()))?;
    let log_path = log_path(state_dir);
    let log = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
    let program = synd_program();
    let mut child = std::process::Command::new(&program)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", program.display()))?;
    let started = Instant::now();
    while started.elapsed() < SPAWN_DEADLINE {
        if client.ping().is_ok() {
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                return Err(format!(
                    "{} exited at startup ({status}): {}",
                    program.display(),
                    log_tail(&log_path)
                ));
            }
            // Exit 0 means another daemon already holds the single-instance lock;
            // keep polling its socket.
            _ => {}
        }
        std::thread::sleep(SPAWN_POLL);
    }
    Err(format!(
        "the daemon did not answer within {}s of being started; see {}",
        SPAWN_DEADLINE.as_secs(),
        log_path.display()
    ))
}

fn log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return format!("no log at {}", path.display());
    };
    let tail: Vec<&str> = text.lines().rev().take(5).collect();
    let mut lines: Vec<&str> = tail.into_iter().rev().collect();
    if lines.is_empty() {
        lines.push("(log is empty)");
    }
    lines.join("; ")
}

/// Prefer the synd next to the running syn binary, so an installed pair stays in step.
fn synd_program() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("synd")))
        .filter(|candidate| candidate.exists())
        .unwrap_or_else(|| PathBuf::from("synd"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn serve_once(socket: &Path, reply: &'static str) -> std::thread::JoinHandle<String> {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            stream.write_all(reply.as_bytes()).unwrap();
            line
        })
    }

    #[test]
    fn call_frames_one_request_line_and_reads_one_reply() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = serve_once(
            &socket,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"pong\"}\n",
        );
        let client = DaemonClient::new(socket, Duration::from_secs(2));
        client.ping().unwrap();
        let request_line = server.join().unwrap();
        let request: Request = serde_json::from_str(&request_line).unwrap();
        assert_eq!(request.method, "ping");
        assert!(request_line.ends_with('\n'));
    }

    #[test]
    fn rpc_errors_surface_code_and_message() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let _server = serve_once(
            &socket,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32001,\"message\":\"memory m_x not found\"}}\n",
        );
        let client = DaemonClient::new(socket, Duration::from_secs(2));
        match client.get(Origin::Preference, "m_x") {
            Err(DaemonError::Rpc { code, message }) => {
                assert_eq!(code, -32001);
                assert_eq!(message, "memory m_x not found");
            }
            other => panic!("expected an rpc error, got {other:?}"),
        }
    }

    #[test]
    fn daemon_config_roundtrips_through_toml() {
        let config = DaemonConfig {
            scoped_orgs: vec![ScopedOrg {
                name: "benediktms".into(),
                token: "tok".into(),
            }],
            auto_update: None,
        };
        let text = config.to_toml().unwrap();
        assert!(text.contains("[[scoped_orgs]]"), "{text}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.toml");
        std::fs::write(&path, &text).unwrap();
        let loaded = DaemonConfig::load(&path).unwrap();
        assert_eq!(loaded.scoped_orgs[0].name, "benediktms");
    }
}
