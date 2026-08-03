#![allow(async_fn_in_trait)]

mod error;
mod fusion;
mod memory;
mod ports;
mod similarity;
mod usecases;
mod workspace;

pub mod fakes;

pub use error::Error;
pub use fusion::{RRF_K, rrf_scores};
pub use memory::{Memory, MemoryId, MemoryKind, Scope, Timestamp};
pub use ports::{Embedder, ScopeFilter, Store};
pub use similarity::cosine_similarity;
pub use usecases::{
    ContextDigest, DIGEST_PINNED_CAP, DIGEST_RECENT_PROJECT_CAP, DIGEST_SHARED_USER_CAP,
    DigestEntry, EditRequest, MIN_VECTOR_SIMILARITY, MoveOutcome, RECALL_LIMIT_CAP, RecallHit,
    RecallRequest, SaveOutcome, SaveRequest, WorkspaceHits, context_digest, edit, forget,
    list_memories, move_memory, recall, recall_grouped, save,
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
