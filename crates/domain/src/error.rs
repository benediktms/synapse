use std::fmt;

use crate::memory::MemoryId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidWorkspaceName(String),
    ReservedWorkspaceName,
    InvalidMemoryId(String),
    InvalidKind(String),
    InvalidImportance(String),
    InvalidRelation(String),
    InvalidScope(String),
    NotFound(MemoryId),
    Conflict(MemoryId),
    /// A supersession edge would close a cycle in the supersession relation.
    Cycle(MemoryId, MemoryId),
    /// Retyping an edge between two memories was ambiguous because several typed edges coexist.
    Ambiguous(MemoryId, MemoryId),
    Store(String),
    Embed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidWorkspaceName(name) => write!(f, "invalid workspace name: {name:?}"),
            Error::ReservedWorkspaceName => write!(f, "workspace name \"shared\" is reserved"),
            Error::InvalidMemoryId(id) => write!(f, "invalid memory id: {id:?}"),
            Error::InvalidKind(kind) => write!(f, "invalid memory kind: {kind:?}"),
            Error::InvalidImportance(tier) => write!(f, "invalid importance tier: {tier:?}"),
            Error::InvalidRelation(rel) => write!(f, "invalid relation: {rel:?}"),
            Error::InvalidScope(scope) => write!(f, "invalid scope: {scope:?}"),
            Error::NotFound(id) => write!(f, "memory {id} not found"),
            Error::Conflict(id) => write!(f, "memory {id} already exists with different payload"),
            Error::Cycle(source, target) => {
                write!(f, "{source} supersedes {target} would create a cycle")
            }
            Error::Ambiguous(a, b) => write!(
                f,
                "multiple links exist between {a} and {b}; unlink the one you mean or pick a pair with a single link"
            ),
            Error::Store(msg) => write!(f, "store error: {msg}"),
            Error::Embed(msg) => write!(f, "embedding error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}
