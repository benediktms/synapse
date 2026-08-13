use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::Error;
use crate::fusion::rrf_scores;
use crate::links::{Link, Relation};
use crate::memory::{Importance, Memory, MemoryId, MemoryKind, Scope, Timestamp};
use crate::ports::{Embedder, ScopeFilter, Store};
use crate::similarity::cosine_similarity;
use crate::workspace::Workspace;

pub const MIN_VECTOR_SIMILARITY: f32 = 0.65;
pub const LINK_CANDIDATE_SIMILARITY: f32 = 0.8;
pub const LINK_CANDIDATE_CAP: usize = 3;
pub const RECALL_LIMIT_CAP: usize = 20;
pub const RECALL_NEIGHBOUR_CAP: usize = 5;
pub const DIGEST_ENTRY_BUDGET: usize = 100;

const CANDIDATE_DEPTH: usize = 50;

#[derive(Clone, Debug)]
pub struct SaveRequest {
    pub id: MemoryId,
    pub content: String,
    /// Some: the caller sets an explicit title (conflict if it differs); None: keep the stored
    /// title on an existing idempotent save, and derive the short form on a true create.
    pub title: Option<String>,
    pub kind: MemoryKind,
    pub scope: Scope,
    pub tags: Vec<String>,
    /// Some: the caller pins an explicit tier (conflict if it differs); None: keep the stored
    /// tier on an existing idempotent save, default to medium on a true create.
    pub importance: Option<Importance>,
}

/// A memory the store already holds that closely resembles one just written. Cosine finds it;
/// only the writer can say whether the relation is a supersession, a contradiction or nothing,
/// so nothing is linked on its behalf.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkCandidate {
    pub id: MemoryId,
    pub title: String,
    pub similarity: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SaveOutcome {
    Created(Memory, Vec<LinkCandidate>),
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
            && req.title.as_ref().is_none_or(|t| existing.title == *t)
            && existing.kind == req.kind
            && existing.scope == req.scope
            && existing.tags == req.tags
            && req
                .importance
                .is_none_or(|tier| existing.importance == tier);
        return if same_payload {
            Ok(SaveOutcome::Unchanged(existing))
        } else {
            Err(Error::Conflict(req.id))
        };
    }
    // Creating is where the rule bites: a new memory must carry a title. Re-saving an existing
    // one may omit it (the stored title stands), and `restore` never comes through here, so a
    // dump written before titles existed still imports.
    let title = req.title.unwrap_or_default();
    if title.is_empty() {
        return Err(Error::MissingTitle(req.id));
    }
    let embedding = embedder
        .embed(&crate::memory::embed_text(&title, &req.content))
        .await?;
    let memory = Memory {
        id: req.id,
        content: req.content,
        title,
        kind: req.kind,
        scope: req.scope,
        tags: req.tags,
        pinned: false,
        importance: req.importance.unwrap_or(Importance::DEFAULT),
        created_at: now.clone(),
        updated_at: now,
    };
    let candidates = link_candidates(store, &memory, &embedding).await?;
    store.insert(&memory, &embedding).await?;
    Ok(SaveOutcome::Created(memory, candidates))
}

/// The memories already in reach of `memory` that most resemble it, strongest first. Run before
/// the write, so the new memory cannot match itself.
async fn link_candidates<S: Store>(
    store: &S,
    memory: &Memory,
    embedding: &[f32],
) -> Result<Vec<LinkCandidate>, Error> {
    let filter = ScopeFilter {
        project: match &memory.scope {
            Scope::Project(slug) => Some(slug.clone()),
            Scope::Workspace => None,
        },
    };
    let mut scored: Vec<(MemoryId, f32)> = store
        .embeddings(&filter)
        .await?
        .into_iter()
        .map(|(id, other)| {
            let similarity = cosine_similarity(embedding, &other);
            (id, similarity)
        })
        .filter(|(id, similarity)| *similarity >= LINK_CANDIDATE_SIMILARITY && *id != memory.id)
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(LINK_CANDIDATE_CAP);

    let mut candidates = Vec::new();
    for (id, similarity) in scored {
        let Some(existing) = store.get(&id).await? else {
            continue;
        };
        candidates.push(LinkCandidate {
            id,
            title: crate::memory::short_form(&existing.title, &existing.content),
            similarity,
        });
    }
    Ok(candidates)
}

#[derive(Clone, Debug, Default)]
pub struct EditRequest {
    pub content: Option<String>,
    /// Replace the short level-of-detail form; an empty string clears it back to derived.
    pub title: Option<String>,
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
        title: req.title.filter(|title| *title != current.title),
        tags: req.tags.filter(|tags| *tags != current.tags),
        pinned: req.pinned.filter(|pinned| *pinned != current.pinned),
        importance: req.importance.filter(|tier| *tier != current.importance),
    };
    if patch.content.is_none()
        && patch.title.is_none()
        && patch.tags.is_none()
        && patch.pinned.is_none()
        && patch.importance.is_none()
    {
        return Ok(current);
    }
    // A title edit re-embeds too: the title is part of what a memory is embedded from, so
    // leaving the old vector in place would make the new title unrecallable.
    let embedding = if patch.content.is_some() || patch.title.is_some() {
        let content = patch.content.as_deref().unwrap_or(&current.content);
        let title = patch.title.as_deref().unwrap_or(&current.title);
        Some(
            embedder
                .embed(&crate::memory::embed_text(title, content))
                .await?,
        )
    } else {
        None
    };
    store.update(id, &patch, embedding.as_deref(), &now).await
}

/// Re-embed every memory in the store with `embedder`, returning how many vectors were written.
/// A memory deleted while the walk is in flight is skipped, not an error.
///
/// The walk is not atomic: a crash leaves some vectors on the new model and some on the old, and
/// the caller's resume marker is what makes the store unreadable until a later run finishes it.
pub async fn reembed<S: Store, E: Embedder>(store: &S, embedder: &E) -> Result<usize, Error> {
    let mut written = 0;
    for memory in store.list().await? {
        let embedding = embedder
            .embed(&crate::memory::embed_text(&memory.title, &memory.content))
            .await?;
        match store.set_embedding(&memory.id, &embedding).await {
            Ok(()) => written += 1,
            Err(Error::NotFound(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(written)
}

#[derive(Clone, Debug, PartialEq)]
pub struct MoveOutcome {
    pub memory: Memory,
    pub from_scope: Scope,
    pub moved: bool,
    pub links_dropped: usize,
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
            links_dropped: 0,
        });
    }
    // ponytail: a link cannot span two stores, so a move drops the memory's edges and reports the
    // count; qualifying link endpoints with a workspace removes the loss (#9).
    let links_dropped = source.1.links_of(id).await?.len();
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
        links_dropped,
    })
}

pub async fn forget<S: Store>(store: &S, id: &MemoryId) -> Result<(), Error> {
    if store.delete(id).await? {
        Ok(())
    } else {
        Err(Error::NotFound(id.clone()))
    }
}

/// Create a typed edge between two memories. Non-supersession relations are canonicalized as
/// symmetric pairs (deduped); `supersede` is directed and cycle-guarded.
pub async fn link<S: Store>(
    store: &S,
    source: &MemoryId,
    target: &MemoryId,
    relation: Relation,
) -> Result<(), Error> {
    if relation == Relation::Supersession {
        within_cycle_guard(store, source, target).await?;
    }
    store
        .insert_link(&Link {
            source: source.clone(),
            target: target.clone(),
            relation,
        })
        .await
}

/// Reject an incoming edge set whose merged supersession graph holds a cycle — checked before
/// anything is written, so a bad dump cannot leave half an import behind. The stored graph is
/// acyclic by construction, so every cycle runs through at least one incoming edge.
pub fn check_import_acyclic(existing: &[Link], incoming: &[Link]) -> Result<(), Error> {
    let mut adjacency: HashMap<MemoryId, Vec<MemoryId>> = HashMap::new();
    for link in existing.iter().chain(incoming) {
        if link.relation == Relation::Supersession {
            adjacency
                .entry(link.source.clone())
                .or_default()
                .push(link.target.clone());
        }
    }
    for link in incoming {
        if link.relation != Relation::Supersession {
            continue;
        }
        if link.source == link.target || reaches(&adjacency, &link.target, &link.source) {
            return Err(Error::Cycle(link.source.clone(), link.target.clone()));
        }
    }
    Ok(())
}

fn reaches(adjacency: &HashMap<MemoryId, Vec<MemoryId>>, from: &MemoryId, to: &MemoryId) -> bool {
    let mut stack = vec![from.clone()];
    let mut seen: HashSet<MemoryId> = HashSet::new();
    while let Some(id) = stack.pop() {
        if id == *to {
            return true;
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(next) = adjacency.get(&id) {
            stack.extend(next.iter().cloned());
        }
    }
    false
}

/// Remove every edge between two memories, whatever its relation. Returns the number removed.
pub async fn unlink<S: Store>(store: &S, a: &MemoryId, b: &MemoryId) -> Result<usize, Error> {
    store.delete_links_between(a, b).await
}

/// Re-anchor an existing edge between `a` and `b` onto `relation`. If a supersession edge is
/// removed or created this is a stateful transition: the derived pin/suppression recomputes
/// from the live graph on the next read (no stored undo).
pub async fn retype_link<S: Store>(
    store: &S,
    a: &MemoryId,
    b: &MemoryId,
    relation: Relation,
) -> Result<(), Error> {
    let links = store.links_of(a).await?;
    let matched: Vec<Link> = links
        .into_iter()
        .filter(|link| link.source == *b || link.target == *b)
        .collect();
    if matched.is_empty() {
        return Err(Error::NotFound(a.clone()));
    }
    // Retyping names only the new type, not which existing edge to change. If several typed edges
    // coexist between the pair, deleting them all would silently drop the siblings — reject rather
    // than guess.
    if matched.len() > 1 {
        return Err(Error::Ambiguous(a.clone(), b.clone()));
    }
    if relation == Relation::Supersession && supersession_would_cycle(store, a, b).await? {
        return Err(Error::Cycle(a.clone(), b.clone()));
    }
    store.delete_links_between(a, b).await?;
    store
        .insert_link(&Link {
            source: a.clone(),
            target: b.clone(),
            relation,
        })
        .await
}

/// A memory is superseded if it is the target of at least one incoming supersession edge.
pub async fn is_superseded<S: Store>(store: &S, id: &MemoryId) -> Result<bool, Error> {
    Ok(store
        .links_of(id)
        .await?
        .iter()
        .any(|link| link.relation == Relation::Supersession && link.target == *id))
}

/// The memories that supersede `id` — the sources of incoming supersession edges.
pub async fn superseders_of<S: Store>(store: &S, id: &MemoryId) -> Result<Vec<MemoryId>, Error> {
    let mut out: Vec<MemoryId> = store
        .links_of(id)
        .await?
        .into_iter()
        .filter(|link| link.relation == Relation::Supersession && link.target == *id)
        .map(|link| link.source)
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// Derived pin status. A memory is effectively pinned if it is itself pinned, or it supersedes —
/// directly or down a chain of supersessions — a memory that is pinned (pin inherits upward).
/// Derived — nothing is written; breaking the supersession edge reverses it automatically.
pub async fn effective_pinned<S: Store>(store: &S, memory: &Memory) -> Result<bool, Error> {
    if memory.pinned {
        return Ok(true);
    }
    let mut stack = vec![memory.id.clone()];
    let mut seen: HashSet<MemoryId> = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        for link in store.links_of(&id).await? {
            if link.relation != Relation::Supersession || link.source != id {
                continue;
            }
            let Some(target) = store.get(&link.target).await? else {
                continue;
            };
            if target.pinned {
                return Ok(true);
            }
            stack.push(link.target.clone());
        }
    }
    Ok(false)
}

async fn within_cycle_guard<S: Store>(
    store: &S,
    source: &MemoryId,
    target: &MemoryId,
) -> Result<(), Error> {
    if supersession_would_cycle(store, source, target).await? {
        return Err(Error::Cycle(source.clone(), target.clone()));
    }
    Ok(())
}

/// True if adding `source supersedes target` would form a cycle — i.e. `target` already
/// transitively reaches `source` by following supersession edges source→target.
async fn supersession_would_cycle<S: Store>(
    store: &S,
    source: &MemoryId,
    target: &MemoryId,
) -> Result<bool, Error> {
    if source == target {
        return Ok(true);
    }
    let mut stack = vec![target.clone()];
    let mut seen: HashSet<MemoryId> = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        for link in store.links_of(&id).await? {
            if link.relation == Relation::Supersession && link.source == id {
                if link.target == *source {
                    return Ok(true);
                }
                stack.push(link.target.clone());
            }
        }
    }
    Ok(false)
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

pub const MAX_GRAPH_DEPTH: usize = 10;
/// Node cap for a single subgraph dump: a depth cap alone does not bound work when a hub memory
/// links to thousands of others, so cap the collected nodes (and by extension edges) to keep one
/// `syn links` call response-bounded. When hit, the dump reports `truncated`.
pub const GRAPH_NODE_BUDGET: usize = 500;
pub const GRAPH_EDGE_BUDGET: usize = 1000;

/// A node of a bounded subgraph dump, for `syn links`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    pub memory: Memory,
    /// True when this node sits on the depth frontier and we did not expand its adjacency — the
    /// dump cuts here and the real graph extends at least one more hop below it.
    pub truncated: bool,
}

/// An edge of the subgraph dump. `directed` is true only for supersession.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEdge {
    pub source: MemoryId,
    pub target: MemoryId,
    pub relation: Relation,
    pub directed: bool,
}

/// The result of a bounded breadth-first subgraph dump rooted at a memory.
#[derive(Clone, Debug)]
pub struct GraphSubgraph {
    pub root: MemoryId,
    pub depth: usize,
    /// True when the dump is a depth-N window and the real graph extends deeper somewhere.
    pub truncated: bool,
    pub nodes: Vec<GraphNode>,
    /// Every edge within the visited set, including back-edges that close cycles.
    pub edges: Vec<GraphEdge>,
}

/// Build a bounded subgraph around `root`: visit each node once but emit every edge (cycle-safe),
/// expand at most `depth` hops, and mark frontier nodes whose real adjacency exceeds the window.
pub async fn graph_subgraph<S: Store>(
    store: &S,
    root: &MemoryId,
    depth: usize,
) -> Result<GraphSubgraph, Error> {
    let depth = depth.min(MAX_GRAPH_DEPTH);

    // FIFO BFS so each node gets its shortest-path level; a node reached first along a long path
    // and later along a short one keeps the short level (no early DFS mis-assignment).
    let mut levels: HashMap<MemoryId, usize> = HashMap::new();
    let mut queue: VecDeque<MemoryId> = VecDeque::new();
    levels.insert(root.clone(), 0);
    queue.push_back(root.clone());
    let mut budget_exceeded = false;
    while let Some(id) = queue.pop_front() {
        let level = levels[&id];
        if level >= depth {
            continue;
        }
        for link in store.links_of(&id).await? {
            for other in [link.source.clone(), link.target.clone()] {
                if !levels.contains_key(&other) {
                    if levels.len() >= GRAPH_NODE_BUDGET {
                        budget_exceeded = true;
                        queue.clear();
                        break;
                    }
                    levels.insert(other.clone(), level + 1);
                    queue.push_back(other);
                }
            }
            if budget_exceeded {
                break;
            }
        }
    }
    let discovered: HashSet<MemoryId> = levels.keys().cloned().collect();

    // Enumerate every edge whose endpoints are both within the window — including edges that only
    // surface from a frontier node's side (e.g. an A–B edge at depth 1) — so the dump is complete
    // at the level it claims. Edges into unvisited territory are cut by the window.
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut seen_edges: HashSet<(MemoryId, MemoryId, Relation)> = HashSet::new();
    for id in &discovered {
        if edges.len() >= GRAPH_EDGE_BUDGET {
            budget_exceeded = true;
            break;
        }
        for link in store.links_of(id).await? {
            if edges.len() >= GRAPH_EDGE_BUDGET {
                budget_exceeded = true;
                break;
            }
            if !discovered.contains(&link.source) || !discovered.contains(&link.target) {
                continue;
            }
            let key = (link.source.clone(), link.target.clone(), link.relation);
            if seen_edges.insert(key) {
                edges.push(GraphEdge {
                    source: link.source.clone(),
                    target: link.target.clone(),
                    relation: link.relation,
                    directed: link.relation.is_directed(),
                });
            }
        }
    }

    // A node on the frontier (shortest-path level == depth) is truncated iff it has a real
    // neighbour beyond the window — one we did not expand. This stays accurate at MAX_GRAPH_DEPTH,
    // which is exactly where the client cannot ask for a deeper follow-up.
    let mut nodes = Vec::new();
    for (id, level) in &levels {
        let links = store.links_of(id).await?;
        let truncated = level == &depth
            && links.iter().any(|link| {
                let other = if &link.source == id {
                    &link.target
                } else {
                    &link.source
                };
                !discovered.contains(other)
            });
        nodes.push(GraphNode {
            memory: store
                .get(id)
                .await?
                .ok_or_else(|| Error::NotFound(id.clone()))?,
            truncated,
        });
    }
    nodes.sort_by(|a, b| a.memory.id.cmp(&b.memory.id));
    edges.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then_with(|| a.target.cmp(&b.target))
            .then_with(|| a.relation.cmp(&b.relation))
    });

    Ok(GraphSubgraph {
        root: root.clone(),
        depth,
        truncated: budget_exceeded || nodes.iter().any(|n| n.truncated),
        nodes,
        edges,
    })
}

#[derive(Clone, Debug, Default)]
pub struct RecallRequest {
    pub query: String,
    pub project: Option<String>,
    pub limit: usize,
    /// When true, only surface linked neighbors whose scope matches the recall's scope filter.
    /// Default false: cross-scope links surface too (Q17).
    pub links_in_scope: bool,
}

/// A first-hop linked neighbor of a recalled memory, for recall surfacing. Ids and a phrase only —
/// the agent traverses explicitly from here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecallLink {
    pub id: MemoryId,
    /// The display phrase from the recalled memory's perspective, e.g. "relates to" or
    /// "is superseded by".
    pub phrase: String,
    /// The neighbor's scope, so a cross-scope link can be identified.
    pub scope: Scope,
}

#[derive(Clone, Debug)]
pub struct RecallHit {
    pub workspace: Workspace,
    pub memory: Memory,
    pub score: f64,
    pub links: Vec<RecallLink>,
    /// The memory has first-hop neighbours beyond the `RECALL_NEIGHBOUR_CAP` shown; `syn links`
    /// walks the rest.
    pub links_truncated: bool,
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
        req.links_in_scope,
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
            req.links_in_scope,
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
    links_in_scope: bool,
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
            // A superseded memory is suppressed from standalone recall (it stays reachable as a
            // neighbour of its superseder via build_recall_links below).
            if is_superseded(store, &id).await? {
                continue;
            }
            let (links, links_truncated) =
                build_recall_links(store, &memory, filter, links_in_scope).await?;
            hits.push(RecallHit {
                workspace: workspace.clone(),
                memory,
                score,
                links,
                links_truncated,
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

/// The first-hop linked neighbors of `memory`, for recall surfacing. For each edge touching the
/// memory, emits the other endpoint with a display phrase relative to `memory`. When
/// `links_in_scope` is set, edges whose neighbor's scope doesn't match `filter` are dropped
/// (cross-scope links otherwise surface, per Q17). At most `RECALL_NEIGHBOUR_CAP` neighbours are
/// hydrated, by relation priority and then lowest id, so a hub memory cannot turn one recall into
/// an unbounded fan-out; the flag says the rest exist. Ranking by anything about the neighbour
/// itself would mean loading every one of them before the cut.
async fn build_recall_links<S: Store>(
    store: &S,
    memory: &Memory,
    filter: &ScopeFilter,
    links_in_scope: bool,
) -> Result<(Vec<RecallLink>, bool), Error> {
    let mut links = store.links_of(&memory.id).await?;
    links.sort_by(|a, b| {
        let left = if a.source == memory.id {
            &a.target
        } else {
            &a.source
        };
        let right = if b.source == memory.id {
            &b.target
        } else {
            &b.source
        };
        a.relation
            .priority()
            .cmp(&b.relation.priority())
            .then(left.cmp(right))
    });
    let mut out = Vec::new();
    let mut truncated = false;
    for link in links {
        if out.len() == RECALL_NEIGHBOUR_CAP {
            truncated = true;
            break;
        }
        let (neighbor_id, directed, this_is_source) = if link.source == memory.id {
            (link.target.clone(), true, true)
        } else {
            (link.source.clone(), false, false)
        };
        let Some(neighbor) = store.get(&neighbor_id).await? else {
            continue;
        };
        if links_in_scope && !filter.matches(&neighbor.scope) {
            continue;
        }
        out.push(RecallLink {
            id: neighbor_id,
            phrase: link
                .relation
                .phrase_from(directed, this_is_source)
                .to_string(),
            scope: neighbor.scope,
        });
    }
    Ok((out, truncated))
}

#[derive(Clone, Debug)]
pub struct DigestEntry {
    pub workspace: Workspace,
    pub memory: Memory,
}

/// One ordered list: every effectively-pinned memory first, then the highest-importance
/// remainder up to `DIGEST_ENTRY_BUDGET`. The digest prints one line per entry, so the
/// budget is an entry count, not a section count.
#[derive(Clone, Debug)]
pub struct ContextDigest {
    pub entries: Vec<DigestEntry>,
}

pub async fn context_digest<S: Store>(
    active: (&Workspace, &S),
    shared: Option<(&Workspace, &S)>,
    project: Option<&str>,
) -> Result<ContextDigest, Error> {
    let mut pool = Vec::new();
    let mut effectively_pinned: HashSet<MemoryId> = HashSet::new();
    for memory in active.1.list().await? {
        // A superseded memory is suppressed from the digest entirely (it stays reachable via
        // its superseder as a neighbour); effective_pinned below reads the store, not the pool.
        if is_superseded(active.1, &memory.id).await? {
            continue;
        }
        if effective_pinned(active.1, &memory).await? {
            effectively_pinned.insert(memory.id.clone());
        }
        pool.push(DigestEntry {
            workspace: active.0.clone(),
            memory,
        });
    }
    if let Some((workspace, store)) = shared {
        for memory in store.list().await? {
            if is_superseded(store, &memory.id).await? {
                continue;
            }
            if effective_pinned(store, &memory).await? {
                effectively_pinned.insert(memory.id.clone());
            }
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

    let mut entries: Vec<DigestEntry> = pool
        .iter()
        .filter(|entry| effectively_pinned.contains(&entry.memory.id))
        .cloned()
        .collect();

    let target_scope = match project {
        Some(slug) => Scope::Project(slug.to_string()),
        None => Scope::Workspace,
    };
    let remaining = DIGEST_ENTRY_BUDGET.saturating_sub(entries.len());
    entries.extend(
        pool.iter()
            .filter(|entry| {
                !effectively_pinned.contains(&entry.memory.id)
                    && (entry.memory.scope == Scope::Workspace
                        || entry.memory.scope == target_scope)
            })
            .take(remaining)
            .cloned(),
    );

    Ok(ContextDigest { entries })
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
            title: String::new(),
            kind,
            scope,
            tags: Vec::new(),
            pinned,
            importance: Importance::DEFAULT,
            created_at: ts(n % 60),
            updated_at: ts(n % 60),
        }
    }

    fn seed_pair(store: &FakeStore, a: u32, b: u32) {
        store.seed(
            mem(
                a,
                &format!("fact {a}"),
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![a as f32, 0.0],
        );
        store.seed(
            mem(
                b,
                &format!("fact {b}"),
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.0, b as f32],
        );
    }

    fn seed_triplet(store: &FakeStore) {
        seed_pair(store, 1, 2);
        store.seed(
            mem(3, "fact 3", MemoryKind::Project, Scope::Workspace, false),
            vec![0.0, 3.0],
        );
    }

    fn super_links(store: &FakeStore, id: &MemoryId) -> Result<Vec<Link>, Error> {
        block_on(store.links_of(id))
    }

    fn get_mem(store: &FakeStore, id: &MemoryId) -> Memory {
        block_on(store.get(id)).unwrap().unwrap()
    }

    const TEST_TITLE: &str = "A stated fact";

    /// What `save` embeds for a `save_req` carrying this content.
    fn saved_text(content: &str) -> String {
        crate::memory::embed_text(TEST_TITLE, content)
    }

    fn save_req(n: u32, content: &str) -> SaveRequest {
        SaveRequest {
            id: mid(n),
            content: content.to_string(),
            title: Some(TEST_TITLE.to_string()),
            kind: MemoryKind::Project,
            scope: Scope::Workspace,
            tags: Vec::new(),
            importance: Some(Importance::DEFAULT),
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
        let embedder = FakeEmbedder::new().with(&saved_text("fact"), vec![1.0, 0.0]);
        let outcome = block_on(save(&store, &embedder, save_req(1, "fact"), ts(1))).unwrap();
        let SaveOutcome::Created(memory, candidates) = outcome else {
            panic!("expected Created");
        };
        assert!(candidates.is_empty(), "an empty store has no candidates");
        assert_eq!(memory.id, mid(1));
        assert!(!memory.pinned);
        assert_eq!(store.embedding_of(&mid(1)), Some(vec![1.0, 0.0]));
    }

    #[test]
    fn save_reports_the_nearest_existing_memories_without_linking_them() {
        let store = FakeStore::new();
        store.seed(
            mem(
                1,
                "a near twin",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.99, 0.1411],
        );
        store.seed(
            mem(
                2,
                "also close",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.9, 0.4359],
        );
        store.seed(
            mem(3, "unrelated", MemoryKind::Project, Scope::Workspace, false),
            vec![0.2, 0.9798],
        );
        let embedder = FakeEmbedder::new().with(&saved_text("fact"), vec![1.0, 0.0]);
        let outcome = block_on(save(&store, &embedder, save_req(4, "fact"), ts(1))).unwrap();
        let SaveOutcome::Created(_, candidates) = outcome else {
            panic!("expected Created");
        };

        assert_eq!(
            candidates.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            vec![mid(1), mid(2)],
            "only memories above the candidate bar, strongest first"
        );
        assert!(candidates[0].similarity > candidates[1].similarity);
        assert!(
            block_on(store.links_of(&mid(4))).unwrap().is_empty(),
            "surfacing a candidate must not create an edge"
        );
    }

    #[test]
    fn save_candidates_stay_inside_the_new_memorys_reach() {
        let store = FakeStore::new();
        store.seed(
            mem(
                1,
                "a near twin in another project",
                MemoryKind::Project,
                Scope::Project("other/repo".into()),
                false,
            ),
            vec![1.0, 0.0],
        );
        let embedder = FakeEmbedder::new().with(&saved_text("fact"), vec![1.0, 0.0]);
        let outcome = block_on(save(&store, &embedder, save_req(2, "fact"), ts(1))).unwrap();
        let SaveOutcome::Created(_, candidates) = outcome else {
            panic!("expected Created");
        };
        assert!(
            candidates.is_empty(),
            "a workspace-scoped save cannot reach another project's memory"
        );
    }

    #[test]
    fn save_with_same_payload_is_idempotent() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new().with(&saved_text("fact"), vec![1.0, 0.0]);
        block_on(save(&store, &embedder, save_req(1, "fact"), ts(1))).unwrap();
        let outcome = block_on(save(&store, &embedder, save_req(1, "fact"), ts(2))).unwrap();
        assert!(matches!(outcome, SaveOutcome::Unchanged(_)));
        assert_eq!(store.len(), 1);
        let stored = block_on(store.get(&mid(1))).unwrap().unwrap();
        assert_eq!(stored.created_at, ts(1));
    }

    /// A title is what recall shows, so it has to be what recall can match. `FakeEmbedder`
    /// errors on any text it has no entry for, which makes the exact embedded string the
    /// assertion — and the edit half proves a title-only change does not leave a stale vector.
    #[test]
    fn a_title_is_embedded_with_the_content_and_re_embedded_when_it_changes() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new()
            .with("ArgoCD owns deploys\nfact", vec![1.0, 0.0])
            .with("Deploys are ArgoCD's\nfact", vec![0.0, 1.0]);
        let req = SaveRequest {
            title: Some("ArgoCD owns deploys".to_string()),
            ..save_req(1, "fact")
        };
        block_on(save(&store, &embedder, req, ts(1))).unwrap();
        assert_eq!(store.embedding_of(&mid(1)), Some(vec![1.0, 0.0]));

        let patch = EditRequest {
            title: Some("Deploys are ArgoCD's".to_string()),
            ..EditRequest::default()
        };
        let edited = block_on(edit(&store, &embedder, &mid(1), patch, ts(2))).unwrap();
        assert_eq!(edited.title, "Deploys are ArgoCD's");
        assert_eq!(
            edited.content, "fact",
            "a title edit leaves the content alone"
        );
        assert_eq!(store.embedding_of(&mid(1)), Some(vec![0.0, 1.0]));
    }

    /// An omitted title must not turn a replayed save into a conflict — an outbox entry
    /// queued before titles existed carries none, and the memory it re-sends may have gained
    /// one since.
    #[test]
    fn a_save_that_omits_the_title_leaves_a_stored_one_alone() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new()
            .with("Titled\nfact", vec![1.0, 0.0])
            .with("fact", vec![0.5, 0.5]);
        let req = SaveRequest {
            title: Some("Titled".to_string()),
            ..save_req(1, "fact")
        };
        block_on(save(&store, &embedder, req, ts(1))).unwrap();
        let replay = SaveRequest {
            title: None,
            ..save_req(1, "fact")
        };
        let outcome = block_on(save(&store, &embedder, replay, ts(2))).unwrap();
        assert!(matches!(outcome, SaveOutcome::Unchanged(_)), "{outcome:?}");
        assert_eq!(
            block_on(store.get(&mid(1))).unwrap().unwrap().title,
            "Titled"
        );
    }

    #[test]
    fn save_with_different_payload_conflicts() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new()
            .with(&saved_text("fact"), vec![1.0, 0.0])
            .with(&saved_text("other"), vec![0.0, 1.0]);
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
    fn reembed_rewrites_every_vector_and_leaves_updated_at_alone() {
        let store = FakeStore::new();
        seed_pair(&store, 1, 2);
        let before = get_mem(&store, &mid(1)).updated_at;
        let embedder = FakeEmbedder::new()
            .with("fact 1", vec![9.0, 1.0])
            .with("fact 2", vec![9.0, 2.0]);

        let written = block_on(reembed(&store, &embedder)).unwrap();

        assert_eq!(written, 2);
        assert_eq!(store.embedding_of(&mid(1)), Some(vec![9.0, 1.0]));
        assert_eq!(store.embedding_of(&mid(2)), Some(vec![9.0, 2.0]));
        assert_eq!(get_mem(&store, &mid(1)).updated_at, before);
    }

    #[test]
    fn reembed_covers_a_titled_memory_by_its_embed_text() {
        let store = FakeStore::new();
        let mut memory = mem(1, "fact", MemoryKind::Project, Scope::Workspace, false);
        memory.title = TEST_TITLE.to_string();
        store.seed(memory, vec![1.0, 0.0]);
        let embedder = FakeEmbedder::new().with(&saved_text("fact"), vec![4.0, 2.0]);

        assert_eq!(block_on(reembed(&store, &embedder)).unwrap(), 1);
        assert_eq!(store.embedding_of(&mid(1)), Some(vec![4.0, 2.0]));
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
            vec![0.68, 0.7332],
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
                links_in_scope: false,
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
                links_in_scope: false,
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
                links_in_scope: false,
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
                links_in_scope: false,
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
                links_in_scope: false,
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
    fn digest_puts_every_pinned_first_then_fills_the_budget() {
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
        work.seed(
            mem(
                45,
                "another project's fact",
                MemoryKind::Project,
                Scope::Project("fresha/other".to_string()),
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
            entry_ids(&digest.entries),
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
                mid(3),
                mid(2),
                mid(1),
                mid(50),
                mid(40),
                mid(35),
                mid(34),
                mid(33),
                mid(32),
                mid(31),
                mid(30),
                mid(26),
                mid(25),
                mid(24),
                mid(23),
                mid(22),
                mid(21),
                mid(20),
            ]
        );
    }

    #[test]
    fn digest_budget_caps_the_unpinned_tail_but_never_the_pinned_head() {
        let ws = Workspace::new("work").unwrap();
        let store = FakeStore::new();
        for n in 1..=DIGEST_ENTRY_BUDGET as u32 + 20 {
            store.seed(
                mem(n, "fact", MemoryKind::Project, Scope::Workspace, false),
                vec![1.0],
            );
        }
        let digest = block_on(context_digest((&ws, &store), None, None)).unwrap();
        assert_eq!(digest.entries.len(), DIGEST_ENTRY_BUDGET);

        let pinned = FakeStore::new();
        for n in 1..=DIGEST_ENTRY_BUDGET as u32 + 20 {
            pinned.seed(
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
        let digest = block_on(context_digest((&ws, &pinned), None, None)).unwrap();
        assert_eq!(digest.entries.len(), DIGEST_ENTRY_BUDGET + 20);
    }

    #[test]
    fn digest_superseder_inherits_a_pinned_targets_pin() {
        let ws = Workspace::new("work").unwrap();
        let store = FakeStore::new();
        // 2 is pinned; 1 supersedes it.
        store.seed(
            mem(
                1,
                "superseder",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0],
        );
        store.seed(
            mem(
                2,
                "pinned superseded",
                MemoryKind::Project,
                Scope::Workspace,
                true,
            ),
            vec![1.0],
        );
        block_on(link(&store, &mid(1), &mid(2), Relation::Supersession)).unwrap();

        let digest = block_on(context_digest((&ws, &store), None, None)).unwrap();
        // 1 is not itself pinned but supersedes pinned 2, so it inherits the pin slot.
        assert_eq!(entry_ids(&digest.entries), vec![mid(1)]);
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
            entry_ids(&digest.entries),
            vec![mid(5), mid(6), mid(1), mid(2), mid(3)]
        );
    }

    #[test]
    fn digest_without_project_excludes_project_scoped_memories() {
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
        assert_eq!(entry_ids(&digest.entries), vec![mid(1)]);
    }

    #[test]
    fn digest_on_shared_workspace_takes_every_preference() {
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
            entry_ids(&digest.entries),
            vec![mid(6), mid(5), mid(4), mid(3), mid(2), mid(1)]
        );
    }

    #[test]
    fn link_bidirectional_canonicalizes() {
        let store = FakeStore::new();
        seed_pair(&store, 1, 2);
        block_on(link(&store, &mid(1), &mid(2), Relation::Support)).unwrap();
        // reversed insertion collapses to the same canonical edge
        block_on(link(&store, &mid(2), &mid(1), Relation::Support)).unwrap();
        let from_1 = super_links(&store, &mid(1)).unwrap();
        assert_eq!(from_1.len(), 1);
        assert_eq!(from_1[0].relation, Relation::Support);
    }

    #[test]
    fn supersede_rejects_direct_and_transitive_cycles() {
        let store = FakeStore::new();
        seed_triplet(&store);
        // 1 supersedes 2 -> direct reverse 2->1 is a cycle
        block_on(link(&store, &mid(1), &mid(2), Relation::Supersession)).unwrap();
        assert_eq!(
            block_on(link(&store, &mid(2), &mid(1), Relation::Supersession)).unwrap_err(),
            Error::Cycle(mid(2), mid(1))
        );
        // 2 supersedes 3 -> 3 supersedes 1 closes the loop 1->2->3->1
        block_on(link(&store, &mid(2), &mid(3), Relation::Supersession)).unwrap();
        assert_eq!(
            block_on(link(&store, &mid(3), &mid(1), Relation::Supersession)).unwrap_err(),
            Error::Cycle(mid(3), mid(1))
        );
    }

    #[test]
    fn supersede_rejects_self_and_accepts_idempotent_duplicate() {
        let store = FakeStore::new();
        seed_pair(&store, 1, 2);
        assert_eq!(
            block_on(link(&store, &mid(1), &mid(1), Relation::Supersession)).unwrap_err(),
            Error::Cycle(mid(1), mid(1))
        );
        block_on(link(&store, &mid(1), &mid(2), Relation::Supersession)).unwrap();
        // exact duplicate is a no-op, not a cycle
        block_on(link(&store, &mid(1), &mid(2), Relation::Supersession)).unwrap();
        assert!(block_on(is_superseded(&store, &mid(2))).unwrap());
    }

    #[test]
    fn is_superseded_and_superseders_read_incoming_edges() {
        let store = FakeStore::new();
        seed_triplet(&store);
        block_on(link(&store, &mid(1), &mid(3), Relation::Supersession)).unwrap();
        block_on(link(&store, &mid(2), &mid(3), Relation::Supersession)).unwrap();
        assert!(block_on(is_superseded(&store, &mid(3))).unwrap());
        assert!(!block_on(is_superseded(&store, &mid(1))).unwrap());
        assert_eq!(
            block_on(superseders_of(&store, &mid(3))).unwrap(),
            vec![mid(1), mid(2)]
        );
    }

    #[test]
    fn effective_pinned_inherits_through_supersession() {
        let store = FakeStore::new();
        store.seed(
            mem(
                1,
                "superseder",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0, 0.0],
        );
        store.seed(
            mem(
                2,
                "superseded pinned",
                MemoryKind::Project,
                Scope::Workspace,
                true,
            ),
            vec![0.0, 1.0],
        );
        // A (1) not pinned, but supersedes pinned B (2) -> effectively pinned
        block_on(link(&store, &mid(1), &mid(2), Relation::Supersession)).unwrap();
        let a = get_mem(&store, &mid(1));
        assert!(block_on(effective_pinned(&store, &a)).unwrap());

        // break the edge -> derived pin reverses
        block_on(unlink(&store, &mid(1), &mid(2))).unwrap();
        assert!(!block_on(effective_pinned(&store, &a)).unwrap());
    }

    #[test]
    fn effective_pinned_follows_a_supersession_chain() {
        let store = FakeStore::new();
        seed_pair(&store, 1, 2);
        store.seed(
            mem(
                3,
                "oldest pinned",
                MemoryKind::Project,
                Scope::Workspace,
                true,
            ),
            vec![0.0, 1.0],
        );
        block_on(link(&store, &mid(1), &mid(2), Relation::Supersession)).unwrap();
        block_on(link(&store, &mid(2), &mid(3), Relation::Supersession)).unwrap();
        let a = get_mem(&store, &mid(1));
        assert!(block_on(effective_pinned(&store, &a)).unwrap());

        block_on(unlink(&store, &mid(2), &mid(3))).unwrap();
        assert!(!block_on(effective_pinned(&store, &a)).unwrap());
    }

    #[test]
    fn effective_pinned_not_inherited_from_unpinned_target() {
        let store = FakeStore::new();
        seed_pair(&store, 1, 2);
        block_on(link(&store, &mid(1), &mid(2), Relation::Supersession)).unwrap();
        let a = get_mem(&store, &mid(1));
        // B (2) not pinned -> A stays unpinned
        assert!(!block_on(effective_pinned(&store, &a)).unwrap());
    }

    #[test]
    fn unlink_removes_any_edge_between_pair() {
        let store = FakeStore::new();
        seed_pair(&store, 1, 2);
        block_on(link(&store, &mid(1), &mid(2), Relation::Relation)).unwrap();
        block_on(link(&store, &mid(1), &mid(2), Relation::Support)).unwrap();
        let removed = block_on(unlink(&store, &mid(1), &mid(2))).unwrap();
        assert_eq!(removed, 2);
        assert!(super_links(&store, &mid(1)).unwrap().is_empty());
    }

    #[test]
    fn retype_link_moves_an_existing_edge() {
        let store = FakeStore::new();
        seed_pair(&store, 1, 2);
        block_on(link(&store, &mid(1), &mid(2), Relation::Relation)).unwrap();
        block_on(retype_link(
            &store,
            &mid(1),
            &mid(2),
            Relation::Supersession,
        ))
        .unwrap();
        let links = super_links(&store, &mid(1)).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].relation, Relation::Supersession);
        assert!(block_on(is_superseded(&store, &mid(2))).unwrap());
    }

    #[test]
    fn recall_surfaces_linked_neighbors_and_suppresses_the_superseded() {
        let store = FakeStore::new();
        store.seed(
            mem(
                1,
                "new deploy process",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0, 0.0],
        );
        store.seed(
            mem(
                2,
                "old deploy process",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.9, 0.1],
        );
        // 2 supersedes 1: 1 is the superseded (suppressed) one.
        block_on(link(&store, &mid(2), &mid(1), Relation::Supersession)).unwrap();

        let ws = Workspace::new("work").unwrap();
        let embedder = FakeEmbedder::new().with("deploy process", vec![0.8, 0.2]);
        let hits = block_on(recall(
            &embedder,
            (&ws, &store),
            None,
            &RecallRequest {
                query: "deploy process".to_string(),
                project: None,
                limit: 10,
                links_in_scope: false,
            },
        ))
        .unwrap();

        // The superseded memory (1) is suppressed from standalone recall...
        assert!(
            hits.iter().all(|h| h.memory.id != mid(1)),
            "superseded memory should not rank on its own"
        );
        // ...but the superseder (2) surfaces and carries 1 as a neighbour.
        let superseder = hits.iter().find(|h| h.memory.id == mid(2)).unwrap();
        assert_eq!(superseder.links.len(), 1);
        assert_eq!(superseder.links[0].id, mid(1));
        assert_eq!(superseder.links[0].phrase, "supersedes");
    }

    #[test]
    fn recall_caps_a_hubs_neighbors_and_flags_the_rest() {
        let store = FakeStore::new();
        store.seed(
            mem(1, "hub fact", MemoryKind::Project, Scope::Workspace, false),
            vec![1.0, 0.0],
        );
        let spokes = RECALL_NEIGHBOUR_CAP as u32 + 3;
        for n in 2..2 + spokes {
            store.seed(
                mem(
                    n,
                    &format!("spoke {n}"),
                    MemoryKind::Project,
                    Scope::Workspace,
                    false,
                ),
                vec![0.0, n as f32],
            );
            block_on(link(&store, &mid(1), &mid(n), Relation::Relation)).unwrap();
        }

        let ws = Workspace::new("work").unwrap();
        let embedder = FakeEmbedder::new().with("hub fact", vec![1.0, 0.0]);
        let hits = block_on(recall(
            &embedder,
            (&ws, &store),
            None,
            &RecallRequest {
                query: "hub fact".to_string(),
                project: None,
                limit: 10,
                links_in_scope: false,
            },
        ))
        .unwrap();

        let hub = hits.iter().find(|h| h.memory.id == mid(1)).unwrap();
        assert_eq!(hub.links.len(), RECALL_NEIGHBOUR_CAP);
        assert!(hub.links_truncated, "the cut neighbours must be announced");
        assert_eq!(hub.links[0].id, mid(2));
    }

    #[test]
    fn recall_keeps_a_contradiction_over_plain_relations_when_it_cuts() {
        let store = FakeStore::new();
        store.seed(
            mem(1, "hub fact", MemoryKind::Project, Scope::Workspace, false),
            vec![1.0, 0.0],
        );
        let spokes = RECALL_NEIGHBOUR_CAP as u32 + 3;
        for n in 2..2 + spokes {
            store.seed(
                mem(
                    n,
                    &format!("spoke {n}"),
                    MemoryKind::Project,
                    Scope::Workspace,
                    false,
                ),
                vec![0.0, n as f32],
            );
            block_on(link(&store, &mid(1), &mid(n), Relation::Relation)).unwrap();
        }
        let contradiction_with_the_highest_id = 2 + spokes;
        store.seed(
            mem(
                contradiction_with_the_highest_id,
                "hub fact is wrong",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![0.0, 1.0],
        );
        block_on(link(
            &store,
            &mid(1),
            &mid(contradiction_with_the_highest_id),
            Relation::Contradiction,
        ))
        .unwrap();

        let ws = Workspace::new("work").unwrap();
        let embedder = FakeEmbedder::new().with("hub fact", vec![1.0, 0.0]);
        let hits = block_on(recall(
            &embedder,
            (&ws, &store),
            None,
            &RecallRequest {
                query: "hub fact".to_string(),
                project: None,
                limit: 10,
                links_in_scope: false,
            },
        ))
        .unwrap();

        let hub = hits.iter().find(|h| h.memory.id == mid(1)).unwrap();
        assert_eq!(hub.links.len(), RECALL_NEIGHBOUR_CAP);
        assert_eq!(hub.links[0].id, mid(contradiction_with_the_highest_id));
        assert_eq!(hub.links[0].phrase, "contradicted by");
    }

    #[test]
    fn recall_links_in_scope_filters_cross_scope_neighbors() {
        let store = FakeStore::new();
        // 1 is workspace-scoped; 2 (its neighbor) is project-scoped.
        store.seed(
            mem(
                1,
                "root cause",
                MemoryKind::Project,
                Scope::Workspace,
                false,
            ),
            vec![1.0, 0.0],
        );
        store.seed(
            mem(
                2,
                "symptom in offers",
                MemoryKind::Project,
                Scope::Project("acme/offers".into()),
                false,
            ),
            vec![0.9, 0.1],
        );
        block_on(link(&store, &mid(1), &mid(2), Relation::Relation)).unwrap();

        let ws = Workspace::new("work").unwrap();
        let embedder = FakeEmbedder::new().with("root cause", vec![0.8, 0.2]);
        // Workspace-scoped recall with the flag ON: the project-scoped neighbor is dropped.
        let hits = block_on(recall(
            &embedder,
            (&ws, &store),
            None,
            &RecallRequest {
                query: "root cause".to_string(),
                project: None,
                limit: 10,
                links_in_scope: true,
            },
        ))
        .unwrap();
        let hit = hits.iter().find(|h| h.memory.id == mid(1)).unwrap();
        assert!(
            hit.links.is_empty(),
            "cross-scope neighbor should be filtered: {:?}",
            hit.links
        );
    }

    #[test]
    fn graph_subgraph_dumps_neighborhood_with_edges() {
        let store = FakeStore::new();
        seed_triplet(&store);
        block_on(link(&store, &mid(1), &mid(2), Relation::Relation)).unwrap();
        block_on(link(&store, &mid(2), &mid(3), Relation::Support)).unwrap();

        let sub = block_on(graph_subgraph(&store, &mid(1), 2)).unwrap();
        let ids: Vec<MemoryId> = sub.nodes.iter().map(|n| n.memory.id.clone()).collect();
        assert_eq!(ids, vec![mid(1), mid(2), mid(3)]);
        assert_eq!(sub.edges.len(), 2);
        assert!(!sub.truncated);
        assert!(sub.nodes.iter().all(|n| !n.truncated));
    }

    #[test]
    fn graph_subgraph_respects_cycle_back_edges() {
        let store = FakeStore::new();
        seed_triplet(&store);
        block_on(link(&store, &mid(1), &mid(2), Relation::Relation)).unwrap();
        block_on(link(&store, &mid(2), &mid(3), Relation::Relation)).unwrap();
        block_on(link(&store, &mid(3), &mid(1), Relation::Relation)).unwrap();

        let sub = block_on(graph_subgraph(&store, &mid(1), 10)).unwrap();
        assert_eq!(sub.nodes.len(), 3);
        assert_eq!(sub.edges.len(), 3);
        assert!(!sub.truncated);
    }

    #[test]
    fn graph_subgraph_truncates_at_depth_with_flag() {
        let store = FakeStore::new();
        seed_triplet(&store);
        block_on(link(&store, &mid(1), &mid(2), Relation::Relation)).unwrap();
        block_on(link(&store, &mid(2), &mid(3), Relation::Relation)).unwrap();

        let sub = block_on(graph_subgraph(&store, &mid(1), 1)).unwrap();
        let ids: Vec<MemoryId> = sub.nodes.iter().map(|n| n.memory.id.clone()).collect();
        assert_eq!(ids, vec![mid(1), mid(2)]);
        assert_eq!(sub.edges.len(), 1);
        let node2 = sub.nodes.iter().find(|n| n.memory.id == mid(2)).unwrap();
        assert!(node2.truncated);
        assert!(sub.truncated);
        let node1 = sub.nodes.iter().find(|n| n.memory.id == mid(1)).unwrap();
        assert!(!node1.truncated);
    }

    #[test]
    fn graph_subgraph_directed_supersession_is_marked() {
        let store = FakeStore::new();
        seed_pair(&store, 1, 2);
        block_on(link(&store, &mid(1), &mid(2), Relation::Supersession)).unwrap();

        let sub = block_on(graph_subgraph(&store, &mid(1), 2)).unwrap();
        assert_eq!(sub.edges.len(), 1);
        assert!(sub.edges[0].directed);
        assert_eq!(sub.edges[0].relation, Relation::Supersession);
        assert!(!sub.edges[0].directed || sub.edges[0].source == mid(1));
    }

    #[test]
    fn graph_subgraph_emits_frontier_cross_edges() {
        // Diamond: root 1 -> A(2), B(3); A and B also linked to each other. At depth 1 the A-B
        // edge only surfaces from a frontier node's side, so it must still be emitted.
        let store = FakeStore::new();
        for n in 1..=3 {
            store.seed(
                mem(
                    n,
                    &format!("fact {n}"),
                    MemoryKind::Project,
                    Scope::Workspace,
                    false,
                ),
                vec![1.0],
            );
        }
        block_on(link(&store, &mid(1), &mid(2), Relation::Relation)).unwrap();
        block_on(link(&store, &mid(1), &mid(3), Relation::Relation)).unwrap();
        block_on(link(&store, &mid(2), &mid(3), Relation::Relation)).unwrap();

        let sub = block_on(graph_subgraph(&store, &mid(1), 1)).unwrap();
        assert_eq!(
            sub.edges.len(),
            3,
            "A-B cross edge must be included: {:?}",
            sub.edges
        );
        assert!(!sub.truncated, "all neighbours are within the window");
    }

    #[test]
    fn graph_subgraph_stays_truncated_at_max_depth() {
        let store = FakeStore::new();
        for n in 1..=(MAX_GRAPH_DEPTH as u32 + 3) {
            store.seed(
                mem(
                    n,
                    &format!("deep {n}"),
                    MemoryKind::Project,
                    Scope::Workspace,
                    false,
                ),
                vec![1.0],
            );
        }
        // Chain 1-2-3-... each relate-forward.
        for n in 1..=(MAX_GRAPH_DEPTH as u32 + 2) {
            block_on(link(&store, &mid(n), &mid(n + 1), Relation::Relation)).unwrap();
        }
        let sub = block_on(graph_subgraph(&store, &mid(1), MAX_GRAPH_DEPTH)).unwrap();
        assert!(
            sub.truncated,
            "the chain continues beyond MAX_GRAPH_DEPTH, so the dump must report truncated"
        );
        assert_eq!(sub.nodes.len(), MAX_GRAPH_DEPTH + 1);
        let frontier = sub
            .nodes
            .iter()
            .find(|n| n.memory.id == mid(MAX_GRAPH_DEPTH as u32 + 1))
            .unwrap();
        assert!(frontier.truncated);
    }
}
