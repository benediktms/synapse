//! A scriptable HTTP/1.1 stub of the synapse server, small enough to let a test
//! drop a connection mid-response — something a real server won't do on request.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub enum Behavior {
    /// Store the memory, then hang up without answering.
    DropAfterCommit,
    /// Store the memory, then answer 200 with a body the client cannot decode.
    UndecodableSuccess,
    Status(u16, String),
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
    pub fn puts(&self) -> Vec<&Recorded> {
        self.recorded.iter().filter(|r| r.method == "PUT").collect()
    }
}

pub struct Stub {
    pub port: u16,
    pub state: Arc<Mutex<State>>,
}

impl Stub {
    pub fn start() -> Self {
        Self::start_on(0)
    }

    pub fn start_on(port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind stub");
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                handle(stream, &shared);
            }
        });
        Self { port, state }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
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

/// A port nothing is listening on, reusable later by `Stub::start_on`.
pub fn dead_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn handle(stream: TcpStream, state: &Arc<Mutex<State>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        if header.trim().is_empty() {
            break;
        }
        let lowered = header.to_ascii_lowercase();
        if let Some(value) = lowered.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&body).to_string();
    let path = target.split('?').next().unwrap_or_default().to_string();

    let mut state = state.lock().unwrap();
    state.recorded.push(Recorded {
        method: method.clone(),
        path: path.clone(),
        body: body.clone(),
    });

    let id_path = ["/memories/", "/preferences/"]
        .iter()
        .find_map(|prefix| path.strip_prefix(prefix))
        .filter(|id| id.starts_with("m_"))
        .map(str::to_string);

    let move_id = id_path
        .as_deref()
        .and_then(|path| path.strip_suffix("/move"))
        .map(str::to_string);
    if let (Some(id), "POST") = (move_id, method.as_str()) {
        let sent: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let mut response: serde_json::Value =
            serde_json::from_str(&memory_json(&id, "{}")).unwrap();
        response["moved"] = serde_json::json!(sent["from"] != sent["to"]);
        response["from"] = sent["from"].clone();
        response["to"] = sent["to"].clone();
        response["from_scope"] = serde_json::json!(
            state
                .move_from_scope
                .clone()
                .unwrap_or_else(|| "workspace".to_string())
        );
        return respond(stream, 200, &response.to_string());
    }

    if let (Some(id), "PUT") = (id_path.clone(), method.as_str()) {
        match state.script.pop_front() {
            Some(Behavior::Status(status, message)) => {
                return respond(stream, status, &format!(r#"{{"error":"{message}"}}"#));
            }
            Some(Behavior::DropAfterCommit) => {
                state.memories.insert(id, body);
                return;
            }
            Some(Behavior::UndecodableSuccess) => {
                state.memories.insert(id, body);
                return respond(stream, 200, r#"{"id":42,"unexpected":"shape"}"#);
            }
            None => {}
        }
        let existing = state.memories.insert(id.clone(), body.clone());
        let status = if existing.is_some() { 200 } else { 201 };
        return respond(stream, status, &memory_json(&id, &body));
    }

    if let Some(id) = id_path {
        return match method.as_str() {
            "DELETE" => respond(stream, 204, ""),
            "PATCH" | "GET" => respond(stream, 200, &memory_json(&id, &body)),
            _ => respond(stream, 405, r#"{"error":"stub has no route"}"#),
        };
    }

    let canned = match path.as_str() {
        "/memories/search" => state
            .search
            .clone()
            .unwrap_or_else(|| r#"{"hits":[]}"#.to_string()),
        "/context" => state
            .context
            .clone()
            .unwrap_or_else(|| r#"{"entries":[]}"#.to_string()),
        "/memories" => r#"{"memories":[]}"#.to_string(),
        "/export" => {
            let memories: Vec<serde_json::Value> = state
                .memories
                .iter()
                .map(|(id, body)| serde_json::from_str(&memory_json(id, body)).unwrap())
                .collect();
            serde_json::json!({"version": 1, "origin": {"workspace": "work"}, "memories": memories})
                .to_string()
        }
        "/workspaces" => r#"{"workspaces":["shared","work"]}"#.to_string(),
        _ => return respond(stream, 404, r#"{"error":"stub has no route"}"#),
    };
    respond(stream, 200, &canned);
}

fn memory_json(id: &str, body: &str) -> String {
    let sent: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let field = |name: &str, fallback: &str| {
        sent.get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or(fallback)
            .to_string()
    };
    serde_json::json!({
        "id": id,
        "content": field("content", "stub content"),
        "kind": field("kind", "user"),
        "scope": field("scope", "workspace"),
        "tags": sent.get("tags").filter(|t| t.is_array()).cloned().unwrap_or_else(|| serde_json::json!([])),
        "pinned": false,
        "created_at": "2026-08-02T10:00:00Z",
        "updated_at": "2026-08-02T10:00:00Z",
    })
    .to_string()
}

fn respond(mut stream: TcpStream, status: u16, body: &str) {
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}
