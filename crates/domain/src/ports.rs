use crate::error::Error;
use crate::links::Link;
use crate::memory::{Memory, MemoryId, Scope, Timestamp};
use crate::usecases::EditRequest;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeFilter {
    pub project: Option<String>,
}

impl ScopeFilter {
    pub fn matches(&self, scope: &Scope) -> bool {
        match scope {
            Scope::Workspace => true,
            Scope::Project(slug) => self.project.as_deref() == Some(slug),
        }
    }
}

pub trait Store {
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>, Error>;
    async fn get_with_embedding(&self, id: &MemoryId) -> Result<Option<(Memory, Vec<f32>)>, Error>;
    async fn insert(&self, memory: &Memory, embedding: &[f32]) -> Result<(), Error>;
    async fn update(
        &self,
        id: &MemoryId,
        patch: &EditRequest,
        embedding: Option<&[f32]>,
        now: &Timestamp,
    ) -> Result<Memory, Error>;
    async fn delete(&self, id: &MemoryId) -> Result<bool, Error>;
    async fn list(&self) -> Result<Vec<Memory>, Error>;
    async fn embeddings(&self, filter: &ScopeFilter) -> Result<Vec<(MemoryId, Vec<f32>)>, Error>;
    async fn keyword_search(
        &self,
        query: &str,
        filter: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryId>, Error>;
    /// Insert a typed edge. Idempotent: symmetric links are canonicalized so (a,b) and (b,a)
    /// collapse; inserting the same edge twice is a no-op. Existing memories for both endpoints
    /// must exist.
    async fn insert_link(&self, link: &Link) -> Result<(), Error>;
    /// Remove every edge between the two memories, whatever its relation. Returns the number
    /// removed (0 if none).
    async fn delete_links_between(&self, a: &MemoryId, b: &MemoryId) -> Result<usize, Error>;
    /// Every edge touching `id`. For symmetric relations the endpoints are canonicalised;
    /// direction is preserved only for directed edges, where `source` is the memory at the
    /// superseding end.
    async fn links_of(&self, id: &MemoryId) -> Result<Vec<Link>, Error>;
    /// Every edge in the store, deduplicated, for export.
    async fn links_all(&self) -> Result<Vec<Link>, Error>;
}

pub trait Embedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, Error>;
}
