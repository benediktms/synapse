use crate::error::Error;
use crate::memory::{Memory, MemoryId, Scope};

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
    async fn insert(&self, memory: &Memory, embedding: &[f32]) -> Result<(), Error>;
    async fn update(&self, memory: &Memory, embedding: Option<&[f32]>) -> Result<(), Error>;
    async fn delete(&self, id: &MemoryId) -> Result<bool, Error>;
    async fn list(&self) -> Result<Vec<Memory>, Error>;
    async fn embeddings(&self, filter: &ScopeFilter) -> Result<Vec<(MemoryId, Vec<f32>)>, Error>;
    async fn keyword_search(
        &self,
        query: &str,
        filter: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryId>, Error>;
}

pub trait Embedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, Error>;
}
