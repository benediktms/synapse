use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::error::Error;
use crate::fusion::rrf_scores;
use crate::memory::{Importance, Memory, MemoryId, MemoryKind, Scope, Timestamp};
use crate::ports::{Embedder, ScopeFilter, Store};
use crate::similarity::cosine_similarity;
use crate::workspace::Workspace;

// Measured on bge-small-en-v1.5: unrelated short texts score 0.30–0.58,
// paraphrases ~0.95 — 0.6 sits above the noise band.
pub const MIN_VECTOR_SIMILARITY: f32 = 0.6;
pub const RECALL_LIMIT_CAP: usize = 20;
pub const DIGEST_PINNED_CAP: usize = 10;
pub const DIGEST_RECENT_PROJECT_CAP: usize = 5;
pub const DIGEST_SHARED_USER_CAP: usize = 5;

const CANDIDATE_DEPTH: usize = 50;

#[derive(Clone, Debug)]
pub struct SaveRequest {
    pub id: MemoryId,
    pub content: String,
    pub kind: MemoryKind,
    pub scope: Scope,
    pub tags: Vec<String>,
    pub importance: Importance,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SaveOutcome {
    Created(Memory),
    Unchanged(Memory),
}

pub async fn save<S: Store, E: Embedder>(
    store: &S,
    embedder: &E,
    req: SaveRequest,
    now: Timestamp,
) -> Result<SaveOutcome, Error> {
    if let Some(existing) = store.get(&req.id).await? {
        let same_payload = existing.content == req.content
            && existing.kind == req.kind
            && existing.scope == req.scope
            && existing.tags == req.tags
            && existing.importance == req.importance;
        return if same_payload {
            Ok(SaveOutcome::Unchanged(existing))
        } else {
            Err(Error::Conflict(req.id))
        };
    }
    let embedding = embedder.embed(&req.content).await?;
    let memory = Memory {
        id: req.id,
        content: req.content,
        kind: req.kind,
        scope: req.scope,
        tags: req.tags,
        pinned: false,
        importance: req.importance,
        created_at: now.clone(),
        updated_at: now,
    };
    store.insert(&memory, &embedding).await?;
    Ok(SaveOutcome::Created(memory))
}

#[derive(Clone, Debug, Default)]
pub struct EditRequest {
    pub content: Option<String>,
    pub tags: Option<Vec<String>>,
    pub pinned: Option<bool>,
    pub importance: Option<Importance>,
}

pub async fn edit<S: Store, E: Embedder>(
    store: &S,
    embedder: &E,
    id: &MemoryId,
    req: EditRequest,
    now: Timestamp,
) -> Result<Memory, Error> {
    let current = store
        .get(id)
        .await?
        .ok_or_else(|| Error::NotFound(id.clone()))?;
    let patch = EditRequest {
        content: req.content.filter(|content| *content != current.content),
        tags: req.tags.filter(|tags| *tags != current.tags),
        pinned: req.pinned.filter(|pinned| *pinned != current.pinned),
        importance: req.importance.filter(|tier| *tier != current.importance),
    };
    if patch.content.is_none()
        && patch.tags.is_none()
        && patch.pinned.is_none()
        && patch.importance.is_none()
    {
        return Ok(current);
    }
    let embedding = match &patch.content {
        Some(content) => Some(embedder.embed(content).await?),
        None => None,
    };
    store.update(id, &patch, embedding.as_deref(), &now).await
}

#[derive(Clone, Debug, PartialEq)]
pub struct MoveOutcome {
    pub memory: Memory,
    pub from_scope: Scope,
    pub moved: bool,
}

pub async fn move_memory<S: Store>(
    source: (&Workspace, &S),
    target: (&Workspace, &S),
    id: &MemoryId,
    now: Timestamp,
) -> Result<MoveOutcome, Error> {
    let (memory, embedding) = source
        .1
        .get_with_embedding(id)
        .await?
        .ok_or_else(|| Error::NotFound(id.clone()))?;
    let from_scope = memory.scope.clone();
    if source.0 == target.0 {
        return Ok(MoveOutcome {
            memory,
            from_scope,
            moved: false,
        });
    }
    let mut moved = memory;
    if target.0.is_shared() {
        moved.scope = Scope::Workspace;
    }
    moved.updated_at = now;
    // A crash between these two leaves a recoverable duplicate; the reverse order loses
    // the memory outright.
    target.1.insert(&moved, &embedding).await?;
    let stored = target
        .1
        .get(id)
        .await?
        .ok_or_else(|| Error::NotFound(id.clone()))?;
    source.1.delete(id).await?;
    Ok(MoveOutcome {
        memory: stored,
        from_scope,
        moved: true,
    })
}

pub async fn forget<S: Store>(store: &S, id: &MemoryId) -> Result<(), Error> {
    if store.delete(id).await? {
        Ok(())
    } else {
        Err(Error::NotFound(id.clone()))
    }
}

pub async fn list_memories<S: Store>(store: &S) -> Result<Vec<Memory>, Error> {
    let mut memories = store.list().await?;
    memories.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(memories)
}

#[derive(Clone, Debug)]
pub struct RecallRequest {
    pub query: String,
    pub project: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Debug)]
pub struct RecallHit {
    pub workspace: Workspace,
    pub memory: Memory,
    pub score: f64,
}

#[derive(Clone, Debug)]
pub struct WorkspaceHits {
    pub workspace: Workspace,
    pub hits: Vec<RecallHit>,
}

pub async fn recall<S: Store, E: Embedder>(
    embedder: &E,
    active: (&Workspace, &S),
    shared: Option<(&Workspace, &S)>,
    req: &RecallRequest,
) -> Result<Vec<RecallHit>, Error> {
    let query_vec = embedder.embed(&req.query).await?;
    let filter = ScopeFilter {
        project: req.project.clone(),
    };
    let mut sides = vec![active];
    if let Some(side) = shared {
        sides.push(side);
    }
    hybrid_search(
        &sides,
        &query_vec,
        &req.query,
        &filter,
        req.limit.clamp(1, RECALL_LIMIT_CAP),
    )
    .await
}

pub async fn recall_grouped<S: Store, E: Embedder>(
    embedder: &E,
    workspaces: &[(Workspace, &S)],
    req: &RecallRequest,
) -> Result<Vec<WorkspaceHits>, Error> {
    let query_vec = embedder.embed(&req.query).await?;
    let filter = ScopeFilter {
        project: req.project.clone(),
    };
    let limit = req.limit.clamp(1, RECALL_LIMIT_CAP);
    let mut ordered: Vec<&(Workspace, &S)> = workspaces.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    let mut groups = Vec::new();
    for (workspace, store) in ordered {
        let hits = hybrid_search(
            &[(workspace, *store)],
            &query_vec,
            &req.query,
            &filter,
            limit,
        )
        .await?;
        if !hits.is_empty() {
            groups.push(WorkspaceHits {
                workspace: workspace.clone(),
                hits,
            });
        }
    }
    Ok(groups)
}

async fn hybrid_search<S: Store>(
    sides: &[(&Workspace, &S)],
    query_vec: &[f32],
    query: &str,
    filter: &ScopeFilter,
    limit: usize,
) -> Result<Vec<RecallHit>, Error> {
    let mut origin: HashMap<MemoryId, usize> = HashMap::new();
    let mut vector_pool: Vec<(MemoryId, f32)> = Vec::new();
    for (side_idx, (_, store)) in sides.iter().enumerate() {
        for (id, embedding) in store.embeddings(filter).await? {
            let score = cosine_similarity(query_vec, &embedding);
            if score >= MIN_VECTOR_SIMILARITY {
                origin.entry(id.clone()).or_insert(side_idx);
                vector_pool.push((id, score));
            }
        }
    }
    vector_pool.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    vector_pool.truncate(CANDIDATE_DEPTH);

    let mut lists: Vec<Vec<MemoryId>> = vec![vector_pool.into_iter().map(|(id, _)| id).collect()];
    for (side_idx, (_, store)) in sides.iter().enumerate() {
        let ids = store.keyword_search(query, filter, CANDIDATE_DEPTH).await?;
        for id in &ids {
            origin.entry(id.clone()).or_insert(side_idx);
        }
        lists.push(ids);
    }

    let mut hits = Vec::new();
    for (id, score) in rrf_scores(&lists) {
        let (workspace, store) = sides[origin[&id]];
        if let Some(memory) = store.get(&id).await? {
            hits.push(RecallHit {
                workspace: workspace.clone(),
                memory,
                score,
            });
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.memory.updated_at.cmp(&a.memory.updated_at))
            .then_with(|| a.memory.id.cmp(&b.memory.id))
    });
    hits.truncate(limit);
    Ok(hits)
}

#[derive(Clone, Debug)]
pub struct DigestEntry {
    pub workspace: Workspace,
    pub memory: Memory,
}

#[derive(Clone, Debug)]
pub struct ContextDigest {
    pub pinned: Vec<DigestEntry>,
    pub recent_project: Vec<DigestEntry>,
    pub preferences: Vec<DigestEntry>,
}

pub async fn context_digest<S: Store>(
    active: (&Workspace, &S),
    shared: Option<(&Workspace, &S)>,
    project: Option<&str>,
) -> Result<ContextDigest, Error> {
    let mut pool = Vec::new();
    for memory in active.1.list().await? {
        pool.push(DigestEntry {
            workspace: active.0.clone(),
            memory,
        });
    }
    if let Some((workspace, store)) = shared {
        for memory in store.list().await? {
            pool.push(DigestEntry {
                workspace: workspace.clone(),
                memory,
            });
        }
    }
    pool.sort_by(|a, b| {
        b.memory
            .importance
            .cmp(&a.memory.importance)
            .then_with(|| b.memory.updated_at.cmp(&a.memory.updated_at))
            .then_with(|| a.memory.id.cmp(&b.memory.id))
    });

    let mut taken: HashSet<MemoryId> = HashSet::new();
    let pinned: Vec<DigestEntry> = pool
        .iter()
        .filter(|entry| entry.memory.pinned)
        .take(DIGEST_PINNED_CAP)
        .cloned()
        .collect();
    taken.extend(pinned.iter().map(|entry| entry.memory.id.clone()));

    let target_scope = match project {
        Some(slug) => Scope::Project(slug.to_string()),
        None => Scope::Workspace,
    };
    let recent_project: Vec<DigestEntry> = pool
        .iter()
        .filter(|entry| {
            !entry.memory.pinned
                && entry.memory.scope == target_scope
                && !taken.contains(&entry.memory.id)
        })
        .take(DIGEST_RECENT_PROJECT_CAP)
        .cloned()
        .collect();
    taken.extend(recent_project.iter().map(|entry| entry.memory.id.clone()));

    let shared_workspace = shared
        .map(|(workspace, _)| workspace.clone())
        .or_else(|| active.0.is_shared().then(|| active.0.clone()));
    let preferences: Vec<DigestEntry> = match shared_workspace {
        Some(workspace) => pool
            .iter()
            .filter(|entry| {
                entry.workspace == workspace
                    && entry.memory.kind == MemoryKind::User
                    && !taken.contains(&entry.memory.id)
            })
            .take(DIGEST_SHARED_USER_CAP)
            .cloned()
            .collect(),
        None => Vec::new(),
    };

    Ok(ContextDigest {
        pinned,
        recent_project,
        preferences,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;
    use crate::fakes::{FakeEmbedder, FakeStore};

    fn ts(minute: u32) -> Timestamp {
        Timestamp::new(format!("2026-01-01T00:{minute:02}:00Z"))
    }

    fn mid(n: u32) -> MemoryId {
        MemoryId::parse(&format!("m_{n:022}")).unwrap()
    }

    fn mem(n: u32, content: &str, kind: MemoryKind, scope: Scope, pinned: bool) -> Memory {
        Memory {
            id: mid(n),
            content: content.to_string(),
            kind,
            scope,
            tags: Vec::new(),
            pinned,
            importance: Importance::DEFAULT,
            created_at: ts(n % 60),
            updated_at: ts(n % 60),
        }
    }

    fn save_req(n: u32, content: &str) -> SaveRequest {
        SaveRequest {
            id: mid(n),
            content: content.to_string(),
            kind: MemoryKind::Project,
            scope: Scope::Workspace,
            tags: Vec::new(),
            importance: Importance::DEFAULT,
        }
    }

    fn hit_ids(hits: &[RecallHit]) -> Vec<MemoryId> {
        hits.iter().map(|hit| hit.memory.id.clone()).collect()
    }

    fn entry_ids(entries: &[DigestEntry]) -> Vec<MemoryId> {
        entries
            .iter()
            .map(|entry| entry.memory.id.clone())
            .collect()
    }

    #[test]
    fn save_creates_memory_with_embedding() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new().with("fact", vec![1.0, 0.0]);
        let outcome = block_on(save(&store, &embedder, save_req(1, "fact"), ts(1))).unwrap();
        let SaveOutcome::Created(memory) = outcome else {
            panic!("expected Created");
        };
        assert_eq!(memory.id, mid(1));
        assert!(!memory.pinned);
        assert_eq!(store.embedding_of(&mid(1)), Some(vec![1.0, 0.0]));
    }

    #[test]
    fn save_with_same_payload_is_idempotent() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new().with("fact", vec![1.0, 0.0]);
        block_on(save(&store, &embedder, save_req(1, "fact"), ts(1))).unwrap();
        let outcome = block_on(save(&store, &embedder, save_req(1, "fact"), ts(2))).unwrap();
        assert!(matches!(outcome, SaveOutcome::Unchanged(_)));
        assert_eq!(store.len(), 1);
        let stored = block_on(store.get(&mid(1))).unwrap().unwrap();
        assert_eq!(stored.created_at, ts(1));
    }

    #[test]
    fn save_with_different_payload_conflicts() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new()
            .with("fact", vec![1.0, 0.0])
            .with("other", vec![0.0, 1.0]);
        block_on(save(&store, &embedder, save_req(1, "fact"), ts(1))).unwrap();
        let err = block_on(save(&store, &embedder, save_req(1, "other"), ts(2))).unwrap_err();
        assert_eq!(err, Error::Conflict(mid(1)));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn edit_content_re_embeds_and_bumps_updated_at() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new().with("new fact", vec![0.0, 1.0]);
        store.seed(
            mem(1, "old fact", MemoryKind::Project, Scope::Workspace, false),
            vec![1.0, 0.0],
        );
        let edited = block_on(edit(
            &store,
            &embedder,
            &mid(1),
            EditRequest {
                content: Some("new fact".to_string()),
                ..EditRequest::default()
            },
            ts(30),
        ))
        .unwrap();
        assert_eq!(edited.content, "new fact");
        assert_eq!(edited.updated_at, ts(30));
        assert_eq!(store.embedding_of(&mid(1)), Some(vec![0.0, 1.0]));
    }

    #[test]
    fn edit_pin_keeps_embedding() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new();
        store.seed(
            mem(1, "fact", MemoryKind::Project, Scope::Workspace, false),
            vec![1.0, 0.0],
        );
        let edited = block_on(edit(
            &store,
            &embedder,
            &mid(1),
            EditRequest {
                pinned: Some(true),
                ..EditRequest::default()
            },
            ts(30),
        ))
        .unwrap();
        assert!(edited.pinned);
        assert_eq!(store.embedding_of(&mid(1)), Some(vec![1.0, 0.0]));
    }

    #[test]
    fn noop_edit_leaves_memory_untouched() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new();
        store.seed(
            mem(1, "fact", MemoryKind::Project, Scope::Workspace, false),
            vec![1.0, 0.0],
        );
        let edited = block_on(edit(
            &store,
            &embedder,
            &mid(1),
            EditRequest::default(),
            ts(30),
        ))
        .unwrap();
        assert_eq!(edited.updated_at, ts(1));
    }

    #[test]
    fn edit_missing_memory_is_not_found() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new();
        let err = block_on(edit(
            &store,
            &embedder,
            &mid(9),
            EditRequest::default(),
            ts(1),
        ))
        .unwrap_err();
        assert_eq!(err, Error::NotFound(mid(9)));
    }

    #[test]
    fn forget_deletes_and_reports_missing() {
        let store = FakeStore::new();
        store.seed(
            mem(1, "fact", MemoryKind::Project, Scope::Workspace, false),
            vec![1.0, 0.0],
        );
        block_on(forget(&store, &mid(1))).unwrap();
        assert!(store.is_empty());
        assert_eq!(
            block_on(forget(&store, &mid(1))).unwrap_err(),
            Error::NotFound(mid(1))
        );
    }

    fn workspaces() -> (Workspace, Workspace) {
        (Workspace::new("work").unwrap(), Workspace::shared())
    }

    #[test]
    fn move_carries_the_memory_and_its_vector_and_empties_the_source() {
        let work = Workspace::new("work").unwrap();
        let personal = Workspace::new("personal").unwrap();
        let source = FakeStore::new();
        let target = FakeStore::new();
        source.seed(
            mem(
                1,
                "fact",
                MemoryKind::Project,
                Scope::Project("me/side-project".to_string()),
                true,
            ),
            vec![0.25, 0.75],
        );
        let outcome = block_on(move_memory(
            (&work, &source),
            (&personal, &target),
            &mid(1),
            ts(30),
        ))
        .unwrap();

        assert!(outcome.moved);
        assert_eq!(outcome.from_scope, Scope::Project("me/side-project".into()));
        assert_eq!(
            outcome.memory.scope,
            Scope::Project("me/side-project".into())
        );
        assert_eq!(outcome.memory.created_at, ts(1));
        assert_eq!(outcome.memory.updated_at, ts(30));
        assert!(outcome.memory.pinned);
        assert!(source.is_empty());
        assert_eq!(target.embedding_of(&mid(1)), Some(vec![0.25, 0.75]));
    }

    #[test]
    fn moving_into_preferences_widens_a_project_scope() {
        let (work, shared) = workspaces();
        let source = FakeStore::new();
        let target = FakeStore::new();
        source.seed(
            mem(
                1,
                "prefers oat milk",
                MemoryKind::User,
                Scope::Project("fresha/offers".to_string()),
                false,
            ),
            vec![1.0, 0.0],
        );
        let outcome = block_on(move_memory(
            (&work, &source),
            (&shared, &target),
            &mid(1),
            ts(30),
        ))
        .unwrap();
        assert_eq!(outcome.from_scope, Scope::Project("fresha/offers".into()));
        assert_eq!(outcome.memory.scope, Scope::Workspace);
    }

    #[test]
    fn moving_out_of_preferences_keeps_workspace_scope() {
        let (work, shared) = workspaces();
        let source = FakeStore::new();
        let target = FakeStore::new();
        source.seed(
            mem(1, "a preference", MemoryKind::User, Scope::Workspace, false),
            vec![1.0, 0.0],
        );
        let outcome = block_on(move_memory(
            (&shared, &source),
            (&work, &target),
            &mid(1),
            ts(30),
        ))
        .unwrap();
        assert!(outcome.moved);
        assert_eq!(outcome.memory.scope, Scope::Workspace);
        assert!(source.is_empty());
    }

    #[test]
    fn moving_where_it_already_lives_changes_nothing() {
        let work = Workspace::new("work").unwrap();
        let store = FakeStore::new();
        store.seed(
            mem(1, "fact", MemoryKind::Project, Scope::Workspace, false),
            vec![1.0, 0.0],
        );
        let outcome = block_on(move_memory(
            (&work, &store),
            (&work, &store),
            &mid(1),
            ts(30),
        ))
        .unwrap();
        assert!(!outcome.moved);
        assert_eq!(outcome.memory.updated_at, ts(1));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn a_retried_move_finishes_instead_of_conflicting() {
        let work = Workspace::new("work").unwrap();
        let personal = Workspace::new("personal").unwrap();
        let source = FakeStore::new();
        let target = FakeStore::new();
        let memory = mem(1, "fact", MemoryKind::Project, Scope::Workspace, false);
        source.seed(memory.clone(), vec![1.0, 0.0]);
        let mut already = memory;
        already.updated_at = ts(20);
        target.seed(already, vec![1.0, 0.0]);

        let outcome = block_on(move_memory(
            (&work, &source),
            (&personal, &target),
            &mid(1),
            ts(30),
        ))
        .unwrap();
        assert!(outcome.moved);
        assert_eq!(outcome.memory.updated_at, ts(20), "reports the stored row");
        assert!(source.is_empty());
        assert_eq!(target.len(), 1);
    }

    #[test]
    fn a_different_memory_under_the_same_id_conflicts_and_keeps_the_source() {
        let work = Workspace::new("work").unwrap();
        let personal = Workspace::new("personal").unwrap();
        let source = FakeStore::new();
        let target = FakeStore::new();
        source.seed(
            mem(1, "fact", MemoryKind::Project, Scope::Workspace, false),
            vec![1.0, 0.0],
        );
        target.seed(
            mem(
                1,
                "something else",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.0, 1.0],
        );
        let err = block_on(move_memory(
            (&work, &source),
            (&personal, &target),
            &mid(1),
            ts(30),
        ))
        .unwrap_err();
        assert_eq!(err, Error::Conflict(mid(1)));
        assert_eq!(source.len(), 1);
        assert_eq!(
            block_on(target.get(&mid(1))).unwrap().unwrap().content,
            "something else"
        );
    }

    #[test]
    fn moving_a_memory_the_source_does_not_hold_is_not_found() {
        let (work, shared) = workspaces();
        let source = FakeStore::new();
        let target = FakeStore::new();
        let err = block_on(move_memory(
            (&work, &source),
            (&shared, &target),
            &mid(9),
            ts(30),
        ))
        .unwrap_err();
        assert_eq!(err, Error::NotFound(mid(9)));
        assert!(target.is_empty());
    }

    #[test]
    fn list_memories_orders_by_updated_at_desc_then_id() {
        let store = FakeStore::new();
        let mut a = mem(1, "a", MemoryKind::Project, Scope::Workspace, false);
        a.updated_at = ts(5);
        let mut b = mem(2, "b", MemoryKind::Project, Scope::Workspace, false);
        b.updated_at = ts(9);
        let mut c = mem(3, "c", MemoryKind::Project, Scope::Workspace, false);
        c.updated_at = ts(5);
        store.seed(a, vec![1.0]);
        store.seed(b, vec![1.0]);
        store.seed(c, vec![1.0]);
        let listed = block_on(list_memories(&store)).unwrap();
        let ids: Vec<MemoryId> = listed.into_iter().map(|memory| memory.id).collect();
        assert_eq!(ids, vec![mid(2), mid(1), mid(3)]);
    }

    #[test]
    fn weak_shared_neighbor_does_not_displace_strong_active_hit() {
        let work_ws = Workspace::new("work").unwrap();
        let shared_ws = Workspace::shared();
        let work = FakeStore::new();
        let shared = FakeStore::new();
        work.seed(
            mem(
                1,
                "argocd deploy staging offers",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0, 0.0],
        );
        work.seed(
            mem(
                2,
                "deploy pipeline retro notes",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.8, 0.6],
        );
        work.seed(
            mem(
                3,
                "offers service oncall runbook",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.7, 0.7141],
        );
        shared.seed(
            mem(
                4,
                "prefers oat milk in coffee",
                MemoryKind::User,
                Scope::Workspace,
                false,
            ),
            vec![0.62, 0.7846],
        );
        let embedder = FakeEmbedder::new().with("deploy staging", vec![1.0, 0.0]);
        let hits = block_on(recall(
            &embedder,
            (&work_ws, &work),
            Some((&shared_ws, &shared)),
            &RecallRequest {
                query: "deploy staging".to_string(),
                project: None,
                limit: 10,
            },
        ))
        .unwrap();
        assert_eq!(hit_ids(&hits), vec![mid(1), mid(2), mid(3), mid(4)]);
    }

    #[test]
    fn below_threshold_shared_neighbor_is_dropped_from_vector_leg() {
        let work_ws = Workspace::new("work").unwrap();
        let shared_ws = Workspace::shared();
        let work = FakeStore::new();
        let shared = FakeStore::new();
        work.seed(
            mem(
                1,
                "argocd deploy staging offers",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0, 0.0],
        );
        shared.seed(
            mem(
                4,
                "prefers oat milk in coffee",
                MemoryKind::User,
                Scope::Workspace,
                false,
            ),
            vec![0.1, 0.995],
        );
        let embedder = FakeEmbedder::new().with("deploy staging", vec![1.0, 0.0]);
        let hits = block_on(recall(
            &embedder,
            (&work_ws, &work),
            Some((&shared_ws, &shared)),
            &RecallRequest {
                query: "deploy staging".to_string(),
                project: None,
                limit: 10,
            },
        ))
        .unwrap();
        assert_eq!(hit_ids(&hits), vec![mid(1)]);
    }

    #[test]
    fn relevant_shared_preference_surfaces() {
        let work_ws = Workspace::new("work").unwrap();
        let shared_ws = Workspace::shared();
        let work = FakeStore::new();
        let shared = FakeStore::new();
        work.seed(
            mem(
                2,
                "argocd deploy staging offers",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0, 0.0],
        );
        work.seed(
            mem(
                1,
                "deploy pipeline retro notes",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.8, 0.6],
        );
        shared.seed(
            mem(
                5,
                "Benedikt prefers Datadog dashboard for deploy verification",
                MemoryKind::User,
                Scope::Workspace,
                false,
            ),
            vec![0.95, 0.312],
        );
        let embedder = FakeEmbedder::new().with("deploy verification", vec![1.0, 0.0]);
        let hits = block_on(recall(
            &embedder,
            (&work_ws, &work),
            Some((&shared_ws, &shared)),
            &RecallRequest {
                query: "deploy verification".to_string(),
                project: None,
                limit: 10,
            },
        ))
        .unwrap();
        let ids = hit_ids(&hits);
        assert!(
            ids.iter().position(|id| *id == mid(5)).unwrap() < 2,
            "shared preference not in top 2: {ids:?}"
        );
        assert_eq!(hits[0].memory.id, mid(2));
        let shared_hit = hits.iter().find(|hit| hit.memory.id == mid(5)).unwrap();
        assert!(shared_hit.workspace.is_shared());
    }

    #[test]
    fn recall_respects_project_scope_filter() {
        let work_ws = Workspace::new("work").unwrap();
        let work = FakeStore::new();
        work.seed(
            mem(
                1,
                "offers deploy runbook",
                MemoryKind::Project,
                Scope::Project("fresha/offers".to_string()),
                false,
            ),
            vec![1.0, 0.0],
        );
        work.seed(
            mem(
                2,
                "billing deploy runbook",
                MemoryKind::Project,
                Scope::Project("fresha/billing".to_string()),
                false,
            ),
            vec![1.0, 0.0],
        );
        let embedder = FakeEmbedder::new().with("deploy runbook", vec![1.0, 0.0]);
        let hits = block_on(recall(
            &embedder,
            (&work_ws, &work),
            None,
            &RecallRequest {
                query: "deploy runbook".to_string(),
                project: Some("fresha/offers".to_string()),
                limit: 10,
            },
        ))
        .unwrap();
        assert_eq!(hit_ids(&hits), vec![mid(1)]);
    }

    #[test]
    fn recall_grouped_returns_sorted_groups_with_shared_once() {
        let work_ws = Workspace::new("work").unwrap();
        let personal_ws = Workspace::new("personal").unwrap();
        let shared_ws = Workspace::shared();
        let work = FakeStore::new();
        let personal = FakeStore::new();
        let shared = FakeStore::new();
        work.seed(
            mem(
                1,
                "deploy notes work",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0, 0.0],
        );
        personal.seed(
            mem(
                2,
                "deploy home server",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.9, 0.436],
        );
        shared.seed(
            mem(
                3,
                "prefers deploy checklists",
                MemoryKind::User,
                Scope::Workspace,
                false,
            ),
            vec![0.8, 0.6],
        );
        let embedder = FakeEmbedder::new().with("deploy", vec![1.0, 0.0]);
        let groups = block_on(recall_grouped(
            &embedder,
            &[
                (work_ws.clone(), &work),
                (shared_ws.clone(), &shared),
                (personal_ws.clone(), &personal),
            ],
            &RecallRequest {
                query: "deploy".to_string(),
                project: None,
                limit: 10,
            },
        ))
        .unwrap();
        let names: Vec<&str> = groups
            .iter()
            .map(|group| group.workspace.as_str())
            .collect();
        assert_eq!(names, vec!["personal", "shared", "work"]);
        assert_eq!(hit_ids(&groups[0].hits), vec![mid(2)]);
        assert_eq!(hit_ids(&groups[1].hits), vec![mid(3)]);
        assert_eq!(hit_ids(&groups[2].hits), vec![mid(1)]);
    }

    #[test]
    fn digest_selects_pinned_recent_project_and_preferences() {
        let work_ws = Workspace::new("work").unwrap();
        let shared_ws = Workspace::shared();
        let work = FakeStore::new();
        let shared = FakeStore::new();
        for n in 1..=11 {
            work.seed(
                mem(
                    n,
                    "pinned fact",
                    MemoryKind::Project,
                    Scope::Workspace,
                    true,
                ),
                vec![1.0],
            );
        }
        for n in 20..=26 {
            work.seed(
                mem(
                    n,
                    "project fact",
                    MemoryKind::Project,
                    Scope::Project("fresha/offers".to_string()),
                    false,
                ),
                vec![1.0],
            );
        }
        work.seed(
            mem(
                40,
                "workspace-wide fact",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0],
        );
        for n in 30..=35 {
            shared.seed(
                mem(
                    n,
                    "user preference",
                    MemoryKind::User,
                    Scope::Workspace,
                    false,
                ),
                vec![1.0],
            );
        }
        shared.seed(
            mem(
                36,
                "pinned user preference",
                MemoryKind::User,
                Scope::Workspace,
                true,
            ),
            vec![1.0],
        );
        shared.seed(
            mem(
                50,
                "shared feedback",
                MemoryKind::Feedback,
                Scope::Workspace,
                false,
            ),
            vec![1.0],
        );

        let digest = block_on(context_digest(
            (&work_ws, &work),
            Some((&shared_ws, &shared)),
            Some("fresha/offers"),
        ))
        .unwrap();

        assert_eq!(
            entry_ids(&digest.pinned),
            vec![
                mid(36),
                mid(11),
                mid(10),
                mid(9),
                mid(8),
                mid(7),
                mid(6),
                mid(5),
                mid(4),
                mid(3)
            ]
        );
        assert_eq!(
            entry_ids(&digest.recent_project),
            vec![mid(26), mid(25), mid(24), mid(23), mid(22)]
        );
        assert_eq!(
            entry_ids(&digest.preferences),
            vec![mid(35), mid(34), mid(33), mid(32), mid(31)]
        );
    }

    #[test]
    fn digest_ranks_importance_over_recency() {
        let work_ws = Workspace::new("work").unwrap();
        let work = FakeStore::new();

        let mut high = mem(
            1,
            "high",
            MemoryKind::Project,
            Scope::Project("fresha/offers".into()),
            false,
        );
        high.importance = Importance::High;
        let mut medium = mem(
            2,
            "medium",
            MemoryKind::Project,
            Scope::Project("fresha/offers".into()),
            false,
        );
        medium.importance = Importance::Medium;
        let mut low = mem(
            3,
            "low",
            MemoryKind::Project,
            Scope::Project("fresha/offers".into()),
            false,
        );
        low.importance = Importance::Low;
        work.seed(high, vec![1.0]);
        work.seed(medium, vec![1.0]);
        work.seed(low, vec![1.0]);

        // Newer pinned memory is lower importance, so importance must beat recency inside the section.
        let mut pinned_high = mem(
            5,
            "pinned high",
            MemoryKind::Project,
            Scope::Workspace,
            true,
        );
        pinned_high.importance = Importance::High;
        let mut pinned_low = mem(6, "pinned low", MemoryKind::Project, Scope::Workspace, true);
        pinned_low.importance = Importance::Low;
        work.seed(pinned_high, vec![1.0]);
        work.seed(pinned_low, vec![1.0]);

        let digest = block_on(context_digest(
            (&work_ws, &work),
            None,
            Some("fresha/offers"),
        ))
        .unwrap();

        assert_eq!(
            entry_ids(&digest.recent_project),
            vec![mid(1), mid(2), mid(3)]
        );
        assert_eq!(entry_ids(&digest.pinned), vec![mid(5), mid(6)]);
    }

    #[test]
    fn digest_without_project_uses_workspace_scope_recents() {
        let work_ws = Workspace::new("work").unwrap();
        let work = FakeStore::new();
        work.seed(
            mem(
                1,
                "workspace fact",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0],
        );
        work.seed(
            mem(
                2,
                "project fact",
                MemoryKind::Project,
                Scope::Project("fresha/offers".to_string()),
                false,
            ),
            vec![1.0],
        );
        let digest = block_on(context_digest((&work_ws, &work), None, None)).unwrap();
        assert_eq!(entry_ids(&digest.recent_project), vec![mid(1)]);
        assert!(digest.preferences.is_empty());
    }

    #[test]
    fn digest_on_shared_workspace_fills_user_section_from_active() {
        let shared_ws = Workspace::shared();
        let shared = FakeStore::new();
        for n in 1..=6 {
            shared.seed(
                mem(
                    n,
                    "user preference",
                    MemoryKind::User,
                    Scope::Workspace,
                    false,
                ),
                vec![1.0],
            );
        }
        let digest = block_on(context_digest((&shared_ws, &shared), None, None)).unwrap();
        assert_eq!(
            entry_ids(&digest.recent_project),
            vec![mid(6), mid(5), mid(4), mid(3), mid(2)]
        );
        assert_eq!(entry_ids(&digest.preferences), vec![mid(1)]);
    }
}
