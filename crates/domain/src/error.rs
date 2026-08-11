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
    /// A new memory arrived without a title. Restoring a dump is exempt: memories written
    /// before titles existed keep deriving one, and a backup has to stay importable.
    MissingTitle(MemoryId),
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
            Error::MissingTitle(id) => write!(
                f,
                "memory {id} needs a title: one line that states the fact, written by whoever \
                 knows it rather than cut from the content"
            ),
            Error::Cycle(source, target) => {
                write!(f, "{source} supersedes {target} would create a cycle")
            }
            Error::Ambiguous(a, b) => write!(
                f,
                "multiple links exist between {a} and {b}; unlink the pair and create the one link you want"
            ),
            Error::Store(msg) => write!(f, "store error: {msg}"),
            Error::Embed(msg) => write!(f, "embedding error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}
