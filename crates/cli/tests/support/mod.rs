//! A scriptable JSON-RPC stub of the synapse daemon, small enough to let a test drop a
//! connection mid-response — something a real daemon won't do on request.
//!
//! The wire contract mirrors `daemon_client::DaemonClient::call`: one newline-terminated
//! request per connection, one newline-terminated response, then close.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// JSON-RPC error codes the CLI's outbox branches on, named so a test reads as intent
/// rather than as a number. Mirrors `DaemonError::is_retryable`/`is_invalid_request`.
pub const CONFLICT: i64 = -32002;
pub const NOT_READY: i64 = -32003;
pub const BAD_REQUEST: i64 = -32602;

/// Sent by the transport before every command, so it is answered but never recorded — a test
/// asserting that nothing reached the daemon is asking about calls the command itself made.
const TRANSPORT_LIVENESS_PROBE: &str = "ping";

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub params: String,
}

impl Recorded {
    fn param(&self, name: &str) -> Option<serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(&self.params)
            .ok()
            .and_then(|params| params.get(name).cloned())
    }

    /// The `id` param, for a test asserting which memory a call named.
    pub fn id(&self) -> String {
        self.param("id")
            .and_then(|id| id.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    /// Which store the call addressed, as the wire names it: `preference`, or a workspace.
    /// The daemon-transport replacement for a route path telling the two apart.
    pub fn origin(&self) -> String {
        match self.param("origin") {
            Some(serde_json::Value::String(name)) => name,
            Some(serde_json::Value::Object(map)) => map
                .get("workspace")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        }
    }

    pub fn is_preference(&self) -> bool {
        self.origin() == "preference"
    }

    /// True when the call named a workspace store. A call with no origin at all — `ping`,
    /// `ready` — addresses neither, so it is not evidence either way.
    pub fn addresses_workspace(&self) -> bool {
        let origin = self.origin();
        !origin.is_empty() && origin != "preference"
    }
}

#[derive(Clone, Debug)]
pub enum Behavior {
    /// Store the memory, then hang up without answering.
    DropAfterCommit,
    /// Store the memory, then answer with a result the client cannot decode.
    UndecodableSuccess,
    /// Answer with a JSON-RPC error object.
    Error(i64, String),
}

#[derive(Default)]
pub struct State {
    pub recorded: Vec<Recorded>,
    pub memories: BTreeMap<String, String>,
    pub script: VecDeque<Behavior>,
    pub search: Option<String>,
    pub context: Option<String>,
    /// Scope a moved memory carried before the move, echoed as `from_scope`.
    pub move_from_scope: Option<String>,
}

impl State {
    /// The calls that wrote a memory, the daemon-transport equivalent of an HTTP PUT.
    pub fn saves(&self) -> Vec<&Recorded> {
        self.calls("memory.save")
    }

    pub fn calls(&self, method: &str) -> Vec<&Recorded> {
        self.recorded
            .iter()
            .filter(|call| call.method == method)
            .collect()
    }
}

pub struct Stub {
    pub state: Arc<Mutex<State>>,
}

impl Stub {
    /// Bind on the socket the CLI derives from its state dir. The parent directory is
    /// created here so a test need not know the daemon's layout.
    pub fn start_at(socket: PathBuf) -> Self {
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).expect("create socket dir");
        }
        let listener = UnixListener::bind(&socket).expect("bind stub socket");
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                handle(stream, &shared);
            }
        });
        Self { state }
    }

    pub fn script(&self, behaviors: Vec<Behavior>) {
        self.state.lock().unwrap().script = behaviors.into();
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }
}

/// Holds the outbox lock the way a concurrently running `syn` would.
pub fn flock(path: &std::path::Path) -> std::fs::File {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .expect("open lock");
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(taken, 0, "lock was already held");
    file
}

fn handle(stream: UnixStream, state: &Arc<Mutex<State>>) {
    let mut line = String::new();
    if BufReader::new(stream.try_clone().expect("clone stream"))
        .read_line(&mut line)
        .unwrap_or(0)
        == 0
    {
        return;
    }
    let request: serde_json::Value = match serde_json::from_str(&line) {
        Ok(value) => value,
        Err(_) => return,
    };
    let id = request
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let method = request
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = request
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let mut state = state.lock().unwrap();
    if method != TRANSPORT_LIVENESS_PROBE {
        state.recorded.push(Recorded {
            method: method.clone(),
            params: params.to_string(),
        });
    }

    let memory_id = params
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    if method == "memory.save" {
        match state.script.pop_front() {
            Some(Behavior::Error(code, message)) => return fail(stream, id, code, &message),
            Some(Behavior::DropAfterCommit) => {
                state.memories.insert(memory_id, params.to_string());
                return;
            }
            Some(Behavior::UndecodableSuccess) => {
                state.memories.insert(memory_id, params.to_string());
                return ok(stream, id, r#"{"created":42,"unexpected":"shape"}"#);
            }
            None => {}
        }
        let existing = state.memories.insert(memory_id.clone(), params.to_string());
        let saved = serde_json::json!({
            "created": existing.is_none(),
            "memory": memory_json(&memory_id, &params),
            "candidates": [],
        });
        return ok(stream, id, &saved.to_string());
    }

    if method == "memory.move" {
        let mut response = memory_json(&memory_id, &params);
        let from = params
            .get("from")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let to = params.get("to").cloned().unwrap_or(serde_json::Value::Null);
        let object = response.as_object_mut().expect("memory json is an object");
        object.insert("moved".into(), serde_json::json!(from != to));
        object.insert("from".into(), from);
        object.insert("to".into(), to);
        object.insert(
            "from_scope".into(),
            serde_json::json!(
                state
                    .move_from_scope
                    .clone()
                    .unwrap_or_else(|| "workspace".to_string())
            ),
        );
        object.insert("links_dropped".into(), serde_json::json!(0));
        return ok(stream, id, &response.to_string());
    }

    let result = match method.as_str() {
        "ping" => "\"pong\"".to_string(),
        "ready" => r#"{"ready":true}"#.to_string(),
        "status" | "sync" => "[]".to_string(),
        "workspace.list" => r#"{"workspaces":["shared","work"]}"#.to_string(),
        "workspace.create" => {
            let name = params
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            serde_json::json!({"workspace": name, "created": true}).to_string()
        }
        "memory.get" | "memory.edit" => memory_json(&memory_id, &params).to_string(),
        "memory.list" => r#"{"memories":[]}"#.to_string(),
        "memory.forget" | "link.create" | "link.retype" => "null".to_string(),
        "link.delete" => r#"{"removed":1}"#.to_string(),
        "search" => state
            .search
            .clone()
            .unwrap_or_else(|| r#"{"hits":[]}"#.to_string()),
        "context" => state
            .context
            .clone()
            .unwrap_or_else(|| r#"{"entries":[]}"#.to_string()),
        "import" => r#"{"imported":0,"unchanged":0}"#.to_string(),
        "export" => {
            let memories: Vec<serde_json::Value> = state
                .memories
                .iter()
                .map(|(id, params)| {
                    let params = serde_json::from_str(params).unwrap_or(serde_json::Value::Null);
                    memory_json(id, &params)
                })
                .collect();
            serde_json::json!({
                "version": 2,
                "origin": {"workspace": "work"},
                "memories": memories,
                "links": [],
            })
            .to_string()
        }
        _ => return fail(stream, id, BAD_REQUEST, "stub has no method"),
    };
    ok(stream, id, &result);
}

/// A memory as the daemon would echo it back, taking whatever the request supplied and
/// filling the rest so the CLI's decode succeeds.
fn memory_json(id: &str, params: &serde_json::Value) -> serde_json::Value {
    let field = |name: &str, fallback: &str| {
        params
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or(fallback)
            .to_string()
    };
    serde_json::json!({
        "id": id,
        "content": field("content", "stub content"),
        "title": field("title", ""),
        "kind": field("kind", "user"),
        "scope": field("scope", "workspace"),
        "tags": params.get("tags").filter(|tags| tags.is_array()).cloned().unwrap_or_else(|| serde_json::json!([])),
        "pinned": false,
        "importance": field("importance", "medium"),
        "created_at": "2026-08-02T10:00:00Z",
        "updated_at": "2026-08-02T10:00:00Z",
    })
}

fn ok(stream: UnixStream, id: u64, result: &str) {
    respond(
        stream,
        &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#),
    );
}

fn fail(stream: UnixStream, id: u64, code: i64, message: &str) {
    let error = serde_json::json!({"code": code, "message": message});
    respond(
        stream,
        &format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{error}}}"#),
    );
}

fn respond(mut stream: UnixStream, line: &str) {
    let _ = stream.write_all(line.as_bytes());
    let _ = stream.write_all(b"\n");
    let _ = stream.flush();
}

/// The socket the CLI derives from a state dir, so a test can bind the stub where `syn`
/// will look, or leave it unbound to exercise an unreachable daemon.
pub fn socket_in(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon").join("daemon.sock")
}
