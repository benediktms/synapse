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

/// A candidate from the vector lane, carrying the cosine that selected it. The store orders these
/// by `similarity` descending, breaking ties on `id` ascending, and applies no threshold — the
/// floor is the domain's to choose.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorHit {
    pub id: MemoryId,
    pub similarity: f32,
}

/// A candidate from the keyword lane, carrying its bm25 rank. bm25 is negative and a stronger
/// match is more negative, so these arrive ascending.
#[derive(Clone, Debug, PartialEq)]
pub struct KeywordHit {
    pub id: MemoryId,
    pub rank: f64,
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
    /// Replace a memory's vector and nothing else. Separate from `update` because a re-embed
    /// is not a semantic edit: `updated_at` must survive it.
    async fn set_embedding(&self, id: &MemoryId, embedding: &[f32]) -> Result<(), Error>;
    async fn delete(&self, id: &MemoryId) -> Result<bool, Error>;
    async fn list(&self) -> Result<Vec<Memory>, Error>;
    /// The `limit` best in-scope candidates by cosine against `embedding`, unthresholded.
    async fn vector_search(
        &self,
        embedding: &[f32],
        filter: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<VectorHit>, Error>;
    async fn keyword_search(
        &self,
        query: &str,
        filter: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<KeywordHit>, Error>;
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
