//! Wire types for the daemon's JSON-RPC transport, shared by the daemon's dispatch and
//! the CLI's client so the two cannot drift.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dto::{
    ExportDoc, LinkCandidateDto, MemoryDto, MoveBody, Origin, PatchMemoryBody, PutMemoryBody,
};

pub const JSONRPC_VERSION: &str = "2.0";

/// The wire method, parsed from its `namespace.verb` string form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Ping,
    Ready,
    Status,
    Shutdown,
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
pub enum WorkspaceMethod {
    Create,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryMethod {
    Save,
    Edit,
    Forget,
    Move,
    Get,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkMethod {
    Graph,
    Create,
    Retype,
    Delete,
}

impl Method {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "ping" => Self::Ping,
            "ready" => Self::Ready,
            "status" => Self::Status,
            "shutdown" => Self::Shutdown,
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

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Ready => "ready",
            Self::Status => "status",
            Self::Shutdown => "shutdown",
            Self::Sync => "sync",
            Self::Search => "search",
            Self::Context => "context",
            Self::Export => "export",
            Self::Import => "import",
            Self::Workspace(WorkspaceMethod::Create) => "workspace.create",
            Self::Workspace(WorkspaceMethod::List) => "workspace.list",
            Self::Memory(MemoryMethod::Save) => "memory.save",
            Self::Memory(MemoryMethod::Edit) => "memory.edit",
            Self::Memory(MemoryMethod::Forget) => "memory.forget",
            Self::Memory(MemoryMethod::Move) => "memory.move",
            Self::Memory(MemoryMethod::Get) => "memory.get",
            Self::Memory(MemoryMethod::List) => "memory.list",
            Self::Link(LinkMethod::Graph) => "link.graph",
            Self::Link(LinkMethod::Create) => "link.create",
            Self::Link(LinkMethod::Retype) => "link.retype",
            Self::Link(LinkMethod::Delete) => "link.delete",
        }
    }
}

fn version() -> String {
    JSONRPC_VERSION.to_string()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    #[serde(default = "version")]
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    #[serde(default = "version")]
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObj>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorObj {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OriginParams {
    pub origin: Origin,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IdParams {
    pub origin: Origin,
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveParams {
    pub origin: Origin,
    pub id: String,
    #[serde(flatten)]
    pub body: PutMemoryBody,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EditParams {
    pub origin: Origin,
    pub id: String,
    #[serde(flatten)]
    pub body: PatchMemoryBody,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MoveParams {
    pub id: String,
    #[serde(flatten)]
    pub body: MoveBody,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws: Option<String>,
    pub q: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub links_scope: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ContextParams {
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GraphParams {
    pub origin: Origin,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LinkParams {
    pub origin: Origin,
    pub id: String,
    pub target: String,
    pub relation: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UnlinkParams {
    pub origin: Origin,
    pub id: String,
    pub target: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportParams {
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub doc: ExportDoc,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceParams {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadyResponse {
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problems: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceCreatedResponse {
    pub workspace: String,
    pub created: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UnlinkResponse {
    pub removed: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveResponse {
    pub created: bool,
    pub memory: MemoryDto,
    /// Memories the store already held that closely resemble this one. Nothing is linked.
    #[serde(default)]
    pub candidates: Vec<LinkCandidateDto>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    pub name: String,
    pub online: bool,
    pub last_synced_at: u64,
    /// What the last failed sync said (None after a successful sync), so an auth
    /// failure is distinguishable from a network outage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_roundtrips_through_its_wire_name() {
        let all = [
            Method::Ping,
            Method::Ready,
            Method::Status,
            Method::Shutdown,
            Method::Sync,
            Method::Search,
            Method::Context,
            Method::Export,
            Method::Import,
            Method::Workspace(WorkspaceMethod::Create),
            Method::Workspace(WorkspaceMethod::List),
            Method::Memory(MemoryMethod::Save),
            Method::Memory(MemoryMethod::Edit),
            Method::Memory(MemoryMethod::Forget),
            Method::Memory(MemoryMethod::Move),
            Method::Memory(MemoryMethod::Get),
            Method::Memory(MemoryMethod::List),
            Method::Link(LinkMethod::Graph),
            Method::Link(LinkMethod::Create),
            Method::Link(LinkMethod::Retype),
            Method::Link(LinkMethod::Delete),
        ];
        for method in all {
            assert_eq!(Method::parse(method.as_str()), Some(method));
        }
        assert_eq!(Method::parse("nope"), None);
    }
}
