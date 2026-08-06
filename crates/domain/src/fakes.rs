use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::Error;
use crate::links::Link;
use crate::memory::{Memory, MemoryId, Timestamp};
use crate::ports::{Embedder, ScopeFilter, Store};
use crate::usecases::EditRequest;

#[derive(Default)]
pub struct FakeStore {
    rows: Mutex<HashMap<MemoryId, (Memory, Vec<f32>)>>,
    links: Mutex<Vec<Link>>,
}

impl FakeStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed(&self, memory: Memory, embedding: Vec<f32>) {
        self.rows
            .lock()
            .unwrap()
            .insert(memory.id.clone(), (memory, embedding));
    }

    pub fn len(&self) -> usize {
        self.rows.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn embedding_of(&self, id: &MemoryId) -> Option<Vec<f32>> {
        self.rows
            .lock()
            .unwrap()
            .get(id)
            .map(|(_, embedding)| embedding.clone())
    }
}

impl Store for FakeStore {
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>, Error> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .get(id)
            .map(|(memory, _)| memory.clone()))
    }

    async fn get_with_embedding(&self, id: &MemoryId) -> Result<Option<(Memory, Vec<f32>)>, Error> {
        Ok(self.rows.lock().unwrap().get(id).cloned())
    }

    async fn insert(&self, memory: &Memory, embedding: &[f32]) -> Result<(), Error> {
        let mut rows = self.rows.lock().unwrap();
        if let Some((existing, _)) = rows.get(&memory.id) {
            let same_payload = existing.content == memory.content
                && existing.kind == memory.kind
                && existing.scope == memory.scope
                && existing.tags == memory.tags
                && existing.pinned == memory.pinned
                && existing.importance == memory.importance;
            return if same_payload {
                Ok(())
            } else {
                Err(Error::Conflict(memory.id.clone()))
            };
        }
        rows.insert(memory.id.clone(), (memory.clone(), embedding.to_vec()));
        Ok(())
    }

    async fn update(
        &self,
        id: &MemoryId,
        patch: &EditRequest,
        embedding: Option<&[f32]>,
        now: &Timestamp,
    ) -> Result<Memory, Error> {
        let mut rows = self.rows.lock().unwrap();
        let Some((memory, stored)) = rows.get_mut(id) else {
            return Err(Error::NotFound(id.clone()));
        };
        if let Some(content) = &patch.content {
            memory.content = content.clone();
        }
        if let Some(tags) = &patch.tags {
            memory.tags = tags.clone();
        }
        if let Some(pinned) = patch.pinned {
            memory.pinned = pinned;
        }
        if let Some(importance) = patch.importance {
            memory.importance = importance;
        }
        memory.updated_at = now.clone();
        if let Some(embedding) = embedding {
            *stored = embedding.to_vec();
        }
        Ok(memory.clone())
    }

    async fn delete(&self, id: &MemoryId) -> Result<bool, Error> {
        Ok(self.rows.lock().unwrap().remove(id).is_some())
    }

    async fn list(&self) -> Result<Vec<Memory>, Error> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .values()
            .map(|(memory, _)| memory.clone())
            .collect())
    }

    async fn embeddings(&self, filter: &ScopeFilter) -> Result<Vec<(MemoryId, Vec<f32>)>, Error> {
        let mut out: Vec<(MemoryId, Vec<f32>)> = self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter(|(memory, _)| filter.matches(&memory.scope))
            .map(|(memory, embedding)| (memory.id.clone(), embedding.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn keyword_search(
        &self,
        query: &str,
        filter: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryId>, Error> {
        let query_tokens = tokenize(query);
        let mut scored: Vec<(usize, Memory)> = self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter(|(memory, _)| filter.matches(&memory.scope))
            .map(|(memory, _)| {
                let content_tokens = tokenize(&memory.content);
                let overlap = query_tokens
                    .iter()
                    .filter(|token| content_tokens.contains(*token))
                    .count();
                (overlap, memory.clone())
            })
            .filter(|(overlap, _)| *overlap > 0)
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.updated_at.cmp(&a.1.updated_at))
                .then_with(|| a.1.id.cmp(&b.1.id))
        });
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(_, memory)| memory.id).collect())
    }

    async fn insert_link(&self, link: &Link) -> Result<(), Error> {
        let link = link.clone().canonical();
        if !self.rows.lock().unwrap().contains_key(&link.source) {
            return Err(Error::NotFound(link.source.clone()));
        }
        if !self.rows.lock().unwrap().contains_key(&link.target) {
            return Err(Error::NotFound(link.target.clone()));
        }
        let mut links = self.links.lock().unwrap();
        if !links.contains(&link) {
            links.push(link);
        }
        Ok(())
    }

    async fn delete_links_between(&self, a: &MemoryId, b: &MemoryId) -> Result<usize, Error> {
        let mut links = self.links.lock().unwrap();
        let before = links.len();
        links.retain(|link| {
            !(link.source == *a && link.target == *b || link.source == *b && link.target == *a)
        });
        Ok(before - links.len())
    }

    async fn links_of(&self, id: &MemoryId) -> Result<Vec<Link>, Error> {
        let mut out: Vec<Link> = self
            .links
            .lock()
            .unwrap()
            .iter()
            .filter(|link| link.source == *id || link.target == *id)
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Default)]
pub struct FakeEmbedder {
    map: HashMap<String, Vec<f32>>,
}

impl FakeEmbedder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, text: &str, embedding: Vec<f32>) -> Self {
        self.map.insert(text.to_string(), embedding);
        self
    }
}

impl Embedder for FakeEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, Error> {
        self.map
            .get(text)
            .cloned()
            .ok_or_else(|| Error::Embed(format!("no fake embedding registered for {text:?}")))
    }
}
