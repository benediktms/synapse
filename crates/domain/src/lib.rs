#![allow(async_fn_in_trait)]

mod error;
mod fusion;
mod links;
mod memory;
mod ports;
mod query;
mod similarity;
mod usecases;
mod workspace;

pub mod fakes;

pub use error::Error;
pub use fusion::{KEYWORD_RANK_RATIO, RRF_K, rrf_scores, trim_keyword_tail};
pub use links::{Link, Relation};
pub use memory::{
    Importance, Memory, MemoryId, MemoryKind, Scope, TITLE_MAX_CHARS, Timestamp, embed_text,
    short_form,
};
pub use ports::{Embedder, KeywordHit, ScopeFilter, Store, VectorHit};
pub use query::content_terms;
pub use similarity::cosine_similarity;
pub use usecases::{
    ContextDigest, DIGEST_ENTRY_BUDGET, DigestEntry, EditRequest, GRAPH_EDGE_BUDGET,
    GRAPH_NODE_BUDGET, GraphEdge, GraphNode, GraphSubgraph, LINK_CANDIDATE_CAP,
    LINK_CANDIDATE_SIMILARITY, LinkCandidate, MAX_GRAPH_DEPTH, MIN_VECTOR_SIMILARITY, MoveOutcome,
    RECALL_LIMIT_CAP, RECALL_NEIGHBOUR_CAP, RecallHit, RecallLink, RecallRequest, SaveOutcome,
    SaveRequest, WorkspaceHits, check_import_acyclic, context_digest, edit, effective_pinned,
    forget, graph_subgraph, is_superseded, link, list_memories, move_memory, recall,
    recall_grouped, reembed, retype_link, save, superseders_of, unlink,
};
pub use workspace::Workspace;

#[cfg(test)]
pub(crate) fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
    }
}
