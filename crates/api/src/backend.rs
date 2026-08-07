use std::future::Future;

use domain::{
    ContextDigest, EditRequest, Error, GraphEdge, GraphSubgraph, Link, Memory, MemoryId,
    MoveOutcome, RecallHit, RecallRequest, Relation, SaveOutcome, SaveRequest, Timestamp,
    Workspace, WorkspaceHits,
};

#[derive(Clone, Debug)]
pub enum BackendError {
    UnknownWorkspace(Workspace),
    Domain(Error),
}

impl From<Error> for BackendError {
    fn from(err: Error) -> Self {
        Self::Domain(err)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RestoreReport {
    pub imported: usize,
    pub unchanged: usize,
}

pub trait Backend: Clone + Send + Sync + 'static {
    fn now(&self) -> Timestamp;
    fn token_window(&self) -> usize;
    fn token_count(&self, text: &str) -> Result<usize, Error>;
    fn ready(&self) -> Result<(), String>;
    fn create_workspace(
        &self,
        ws: &Workspace,
    ) -> impl Future<Output = Result<bool, BackendError>> + Send;
    fn workspaces(&self) -> impl Future<Output = Result<Vec<Workspace>, BackendError>> + Send;
    fn save(
        &self,
        ws: &Workspace,
        req: SaveRequest,
    ) -> impl Future<Output = Result<SaveOutcome, BackendError>> + Send;
    fn edit(
        &self,
        ws: &Workspace,
        id: &MemoryId,
        req: EditRequest,
    ) -> impl Future<Output = Result<Memory, BackendError>> + Send;
    fn forget(
        &self,
        ws: &Workspace,
        id: &MemoryId,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;
    fn move_memory(
        &self,
        from: &Workspace,
        to: &Workspace,
        id: &MemoryId,
    ) -> impl Future<Output = Result<MoveOutcome, BackendError>> + Send;
    fn get(
        &self,
        ws: &Workspace,
        id: &MemoryId,
    ) -> impl Future<Output = Result<Option<Memory>, BackendError>> + Send;
    fn list(
        &self,
        ws: &Workspace,
    ) -> impl Future<Output = Result<Vec<Memory>, BackendError>> + Send;
    fn recall(
        &self,
        ws: &Workspace,
        req: &RecallRequest,
    ) -> impl Future<Output = Result<Vec<RecallHit>, BackendError>> + Send;
    fn recall_all(
        &self,
        req: &RecallRequest,
    ) -> impl Future<Output = Result<Vec<WorkspaceHits>, BackendError>> + Send;
    fn context(
        &self,
        ws: &Workspace,
        project: Option<&str>,
    ) -> impl Future<Output = Result<ContextDigest, BackendError>> + Send;
    fn links(
        &self,
        ws: &Workspace,
        id: &MemoryId,
        depth: usize,
    ) -> impl Future<Output = Result<GraphSubgraph, BackendError>> + Send;
    fn links_all(
        &self,
        ws: &Workspace,
    ) -> impl Future<Output = Result<Vec<GraphEdge>, BackendError>> + Send;
    fn link(
        &self,
        ws: &Workspace,
        source: &MemoryId,
        target: &MemoryId,
        relation: Relation,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;
    fn unlink(
        &self,
        ws: &Workspace,
        a: &MemoryId,
        b: &MemoryId,
    ) -> impl Future<Output = Result<usize, BackendError>> + Send;
    fn retype_link(
        &self,
        ws: &Workspace,
        a: &MemoryId,
        b: &MemoryId,
        relation: Relation,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;
    fn restore(
        &self,
        ws: &Workspace,
        memories: Vec<Memory>,
        links: Vec<Link>,
    ) -> impl Future<Output = Result<RestoreReport, BackendError>> + Send;
}
