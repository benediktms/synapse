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
    Status(u16, String),
}

#[derive(Default)]
pub struct State {
    pub recorded: Vec<Recorded>,
    pub memories: BTreeMap<String, String>,
    pub script: VecDeque<Behavior>,
    pub search: Option<String>,
    pub context: Option<String>,
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

    if method == "PUT" && path.starts_with("/memories/") {
        let id = path.trim_start_matches("/memories/").to_string();
        match state.script.pop_front() {
            Some(Behavior::Status(status, message)) => {
                return respond(stream, status, &format!(r#"{{"error":"{message}"}}"#));
            }
            Some(Behavior::DropAfterCommit) => {
                state.memories.insert(id, body);
                return;
            }
            None => {}
        }
        let existing = state.memories.insert(id.clone(), body.clone());
        let status = if existing.is_some() { 200 } else { 201 };
        return respond(stream, status, &memory_json(&id, &body));
    }

    if path.starts_with("/memories/m_") {
        return match method.as_str() {
            "DELETE" => respond(stream, 204, ""),
            "PATCH" | "GET" => respond(stream, 200, &memory_json(&path[10..], &body)),
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
            .unwrap_or_else(|| r#"{"pinned":[],"recent_project":[],"shared_user":[]}"#.to_string()),
        "/memories" => r#"{"memories":[]}"#.to_string(),
        "/workspaces" => r#"{"workspaces":["shared","work"]}"#.to_string(),
        _ => return respond(stream, 404, r#"{"error":"stub has no route"}"#),
    };
    respond(stream, 200, &canned);
}

fn memory_json(id: &str, body: &str) -> String {
    let sent: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    serde_json::json!({
        "id": id,
        "content": sent["content"],
        "kind": sent["kind"],
        "scope": sent["scope"],
        "tags": sent.get("tags").cloned().unwrap_or_else(|| serde_json::json!([])),
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
