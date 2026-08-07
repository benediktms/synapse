use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use adapters_fastembed::FastEmbedder;
use adapters_libsql::{LibsqlStore, TursoPlatform};
use api::{Backend, BackendError, RestoreReport};
use domain::{
    ContextDigest, EditRequest, Embedder, Error, GraphEdge, GraphSubgraph, Link, Memory, MemoryId,
    MoveOutcome, RecallHit, RecallRequest, Relation, SaveOutcome, SaveRequest, Timestamp,
    Workspace, WorkspaceHits,
};
use tokio::sync::{Mutex, RwLock};

use crate::config::{
    Config, Manifest, WorkspaceBinding, offline_bindings, open_binding, replica_path,
    resolve_bindings,
};
use crate::rpc::{RpcHost, WorkspaceStatus};

/// Bound on the network-first sync attempted before freshness-sensitive reads and after
/// writes. On timeout or failure the operation falls back to the local replica as-is.
const SYNC_BOUND: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct DaemonApp {
    inner: Arc<DaemonInner>,
}

struct DaemonInner {
    state_dir: PathBuf,
    config: Config,
    embedder: Arc<FastEmbedder>,
    stores: RwLock<HashMap<Workspace, Arc<LibsqlStore>>>,
    bindings: RwLock<HashMap<Workspace, WorkspaceBinding>>,
    platform: TursoPlatform,
    ready: Result<(), String>,
    /// Serializes link mutations (cycle check + insert) so two concurrent supersessions can't
    /// both observe an acyclic graph and then both insert, closing the TOCTOU race.
    links_mutation: Mutex<()>,
}

impl DaemonApp {
    pub async fn boot(state_dir: PathBuf, config: Config) -> Result<Self, String> {
        std::fs::create_dir_all(&state_dir)
            .map_err(|e| format!("cannot create state dir {}: {e}", state_dir.display()))?;
        let embedder = Arc::new(FastEmbedder::new().map_err(|e| format!("embedder: {e}"))?);

        let mut problems = Vec::new();
        let mut stores = HashMap::new();
        let mut bindings = HashMap::new();

        // Online resolution (list scoped orgs, provision shared if missing) then open; on failure
        // fall back to offline bindings from the cached manifest.
        match resolve_bindings(&state_dir, &config).await {
            Ok(resolved) => {
                for binding in resolved {
                    match open_binding(&binding).await {
                        Ok(store) => {
                            let ws = binding.workspace.clone();
                            bindings.insert(ws.clone(), binding);
                            stores.insert(ws, Arc::new(store));
                        }
                        Err(e) => problems.push(e),
                    }
                }
            }
            Err(_) => {
                for binding in offline_bindings(&state_dir, &config) {
                    match open_binding(&binding).await {
                        Ok(store) => {
                            let ws = binding.workspace.clone();
                            bindings.insert(ws.clone(), binding);
                            stores.insert(ws, Arc::new(store));
                        }
                        Err(e) => problems.push(e),
                    }
                }
            }
        }

        if stores.is_empty() && problems.is_empty() {
            problems.push(
                "no replicas available: the first run needs the network to bootstrap".to_string(),
            );
        }
        let ready = if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        };

        Ok(Self {
            inner: Arc::new(DaemonInner {
                state_dir,
                config,
                embedder,
                stores: RwLock::new(stores),
                bindings: RwLock::new(bindings),
                platform: TursoPlatform::new(),
                ready,
                links_mutation: Mutex::new(()),
            }),
        })
    }

    /// A workspace is only reachable once bound at boot or created explicitly; a lookup
    /// must never provision a cloud database for a mistyped name.
    async fn store(&self, ws: &Workspace) -> Result<Arc<LibsqlStore>, BackendError> {
        self.inner
            .stores
            .read()
            .await
            .get(ws)
            .map(Arc::clone)
            .ok_or_else(|| BackendError::UnknownWorkspace(ws.clone()))
    }

    async fn provision_binding(&self, ws: &Workspace) -> Result<WorkspaceBinding, BackendError> {
        let mut bindings = self.inner.bindings.write().await;
        if let Some(b) = bindings.get(ws) {
            return Ok(b.clone());
        }
        let org = self
            .inner
            .config
            .scoped_orgs
            .first()
            .ok_or_else(|| BackendError::UnknownWorkspace(ws.clone()))?;
        let db = self
            .inner
            .platform
            .create_database(&org.name, &org.token, ws.as_str())
            .await
            .map_err(BackendError::Domain)?;
        let mut manifest = Manifest::load(&self.inner.state_dir);
        manifest.set(ws, &db.url, &org.name);
        manifest.save(&self.inner.state_dir);
        let binding = WorkspaceBinding {
            workspace: ws.clone(),
            replica: replica_path(&self.inner.state_dir, ws),
            url: db.url,
            token: org.token.clone(),
        };
        bindings.insert(ws.clone(), binding.clone());
        Ok(binding)
    }

    /// Network-first sync bounded by SYNC_BOUND; offline or slow degrades to the local
    /// replica without failing the surrounding operation.
    async fn freshen(&self, store: &LibsqlStore) {
        match tokio::time::timeout(SYNC_BOUND, store.sync()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::debug!("replica sync failed open: {e}"),
            Err(_) => tracing::debug!("replica sync timed out"),
        }
    }

    async fn active_and_shared(
        &self,
        ws: &Workspace,
    ) -> Result<(Arc<LibsqlStore>, Option<(Workspace, Arc<LibsqlStore>)>), BackendError> {
        let active = self.store(ws).await?;
        if ws.is_shared() {
            return Ok((active, None));
        }
        let shared = Workspace::shared();
        match self.store(&shared).await {
            Ok(shared_store) => Ok((active, Some((shared, shared_store)))),
            // A missing shared replica degrades recall/context to the active side only.
            Err(BackendError::UnknownWorkspace(_)) => Ok((active, None)),
            Err(e) => Err(e),
        }
    }

    /// Number of open workspaces (for the startup banner).
    pub async fn workspace_count(&self) -> usize {
        self.inner.stores.read().await.len()
    }
}

impl RpcHost for DaemonApp {
    async fn statuses(&self) -> Vec<WorkspaceStatus> {
        let stores = self.inner.stores.read().await;
        let mut out: Vec<WorkspaceStatus> = stores
            .iter()
            .map(|(ws, store)| WorkspaceStatus {
                name: ws.to_string(),
                online: store.online(),
                last_synced_at: store.last_synced_at(),
                pending_outbox: store.pending_outbox(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    async fn sync_replicas(&self, only: Option<&Workspace>) -> Result<(), BackendError> {
        match only {
            Some(ws) => {
                let store = self.store(ws).await?;
                store.sync().await.map_err(BackendError::Domain)
            }
            None => {
                let stores: Vec<Arc<LibsqlStore>> =
                    self.inner.stores.read().await.values().cloned().collect();
                let mut failures = Vec::new();
                for store in stores {
                    if let Err(e) = store.sync().await {
                        failures.push(e.to_string());
                    }
                }
                if failures.is_empty() {
                    Ok(())
                } else {
                    Err(BackendError::Domain(Error::Store(failures.join("; "))))
                }
            }
        }
    }
}

impl Backend for DaemonApp {
    fn now(&self) -> Timestamp {
        Timestamp::new(humantime::format_rfc3339_seconds(SystemTime::now()).to_string())
    }

    fn token_window(&self) -> usize {
        self.inner.embedder.token_window()
    }

    fn token_count(&self, text: &str) -> Result<usize, Error> {
        self.inner.embedder.token_count(text)
    }

    fn ready(&self) -> Result<(), String> {
        self.inner.ready.clone()
    }

    async fn create_workspace(&self, ws: &Workspace) -> Result<bool, BackendError> {
        let mut stores = self.inner.stores.write().await;
        if stores.contains_key(ws) {
            return Ok(false);
        }
        let binding = self.provision_binding(ws).await?;
        let store = Arc::new(
            open_binding(&binding)
                .await
                .map_err(|e| BackendError::Domain(Error::Store(e)))?,
        );
        stores.insert(ws.clone(), store);
        Ok(true)
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>, BackendError> {
        let stores = self.inner.stores.read().await;
        let mut ws: Vec<Workspace> = stores.keys().cloned().collect();
        ws.sort();
        Ok(ws)
    }

    async fn save(&self, ws: &Workspace, req: SaveRequest) -> Result<SaveOutcome, BackendError> {
        let store = self.store(ws).await?;
        let outcome = domain::save(&*store, &*self.inner.embedder, req, self.now()).await?;
        self.freshen(&store).await;
        Ok(outcome)
    }

    async fn edit(
        &self,
        ws: &Workspace,
        id: &MemoryId,
        req: EditRequest,
    ) -> Result<Memory, BackendError> {
        let store = self.store(ws).await?;
        let memory = domain::edit(&*store, &*self.inner.embedder, id, req, self.now()).await?;
        self.freshen(&store).await;
        Ok(memory)
    }

    async fn forget(&self, ws: &Workspace, id: &MemoryId) -> Result<(), BackendError> {
        let store = self.store(ws).await?;
        domain::forget(&*store, id).await?;
        self.freshen(&store).await;
        Ok(())
    }

    async fn move_memory(
        &self,
        from: &Workspace,
        to: &Workspace,
        id: &MemoryId,
    ) -> Result<MoveOutcome, BackendError> {
        let _guard = self.inner.links_mutation.lock().await;
        let source = self.store(from).await?;
        let target = self.store(to).await?;
        let outcome = domain::move_memory((from, &*source), (to, &*target), id, self.now()).await?;
        self.freshen(&source).await;
        self.freshen(&target).await;
        Ok(outcome)
    }

    async fn get(&self, ws: &Workspace, id: &MemoryId) -> Result<Option<Memory>, BackendError> {
        use domain::Store;
        let store = self.store(ws).await?;
        store.get(id).await.map_err(Into::into)
    }

    async fn list(&self, ws: &Workspace) -> Result<Vec<Memory>, BackendError> {
        let store = self.store(ws).await?;
        self.freshen(&store).await;
        domain::list_memories(&*store).await.map_err(Into::into)
    }

    async fn recall(
        &self,
        ws: &Workspace,
        req: &RecallRequest,
    ) -> Result<Vec<RecallHit>, BackendError> {
        let (active, shared) = self.active_and_shared(ws).await?;
        self.freshen(&active).await;
        if let Some((_, store)) = &shared {
            self.freshen(store).await;
        }
        let shared_side = shared
            .as_ref()
            .map(|(shared_ws, store)| (shared_ws, &**store));
        domain::recall(&*self.inner.embedder, (ws, &*active), shared_side, req)
            .await
            .map_err(Into::into)
    }

    async fn recall_all(&self, req: &RecallRequest) -> Result<Vec<WorkspaceHits>, BackendError> {
        let workspaces = self.workspaces().await?;
        let mut sides: Vec<(Workspace, Arc<LibsqlStore>)> = Vec::with_capacity(workspaces.len());
        for ws in workspaces {
            let store = self.store(&ws).await?;
            self.freshen(&store).await;
            sides.push((ws, store));
        }
        let refs: Vec<(Workspace, &LibsqlStore)> = sides
            .iter()
            .map(|(ws, store)| (ws.clone(), &**store))
            .collect();
        domain::recall_grouped(&*self.inner.embedder, &refs, req)
            .await
            .map_err(Into::into)
    }

    async fn context(
        &self,
        ws: &Workspace,
        project: Option<&str>,
    ) -> Result<ContextDigest, BackendError> {
        let (active, shared) = self.active_and_shared(ws).await?;
        self.freshen(&active).await;
        if let Some((_, store)) = &shared {
            self.freshen(store).await;
        }
        let shared_side = shared
            .as_ref()
            .map(|(shared_ws, store)| (shared_ws, &**store));
        domain::context_digest((ws, &*active), shared_side, project)
            .await
            .map_err(Into::into)
    }

    async fn links(
        &self,
        ws: &Workspace,
        id: &MemoryId,
        depth: usize,
    ) -> Result<GraphSubgraph, BackendError> {
        let (active, _) = self.active_and_shared(ws).await?;
        domain::graph_subgraph(&*active, id, depth)
            .await
            .map_err(Into::into)
    }

    async fn links_all(&self, ws: &Workspace) -> Result<Vec<GraphEdge>, BackendError> {
        use domain::Store;
        let (active, _) = self.active_and_shared(ws).await?;
        let links = active.links_all().await.map_err(BackendError::Domain)?;
        Ok(links
            .into_iter()
            .map(|link| GraphEdge {
                source: link.source,
                target: link.target,
                relation: link.relation,
                directed: link.relation.is_directed(),
            })
            .collect())
    }

    async fn link(
        &self,
        ws: &Workspace,
        source: &MemoryId,
        target: &MemoryId,
        relation: Relation,
    ) -> Result<(), BackendError> {
        let _guard = self.inner.links_mutation.lock().await;
        let (active, _) = self.active_and_shared(ws).await?;
        domain::link(&*active, source, target, relation).await?;
        self.freshen(&active).await;
        Ok(())
    }

    async fn unlink(
        &self,
        ws: &Workspace,
        a: &MemoryId,
        b: &MemoryId,
    ) -> Result<usize, BackendError> {
        let _guard = self.inner.links_mutation.lock().await;
        let (active, _) = self.active_and_shared(ws).await?;
        let removed = domain::unlink(&*active, a, b).await?;
        self.freshen(&active).await;
        Ok(removed)
    }

    async fn retype_link(
        &self,
        ws: &Workspace,
        a: &MemoryId,
        b: &MemoryId,
        relation: Relation,
    ) -> Result<(), BackendError> {
        let _guard = self.inner.links_mutation.lock().await;
        let (active, _) = self.active_and_shared(ws).await?;
        domain::retype_link(&*active, a, b, relation).await?;
        self.freshen(&active).await;
        Ok(())
    }

    async fn restore(
        &self,
        ws: &Workspace,
        memories: Vec<Memory>,
        links: Vec<Link>,
    ) -> Result<RestoreReport, BackendError> {
        use domain::Store;
        let store = self.store(ws).await?;
        let mut report = RestoreReport::default();
        let _guard = self.inner.links_mutation.lock().await;
        domain::check_import_acyclic(&store.links_all().await?, &links)?;
        for memory in memories {
            if let Some(existing) = store.get(&memory.id).await? {
                let same_payload = existing.content == memory.content
                    && existing.kind == memory.kind
                    && existing.scope == memory.scope
                    && existing.tags == memory.tags;
                if same_payload {
                    report.unchanged += 1;
                    continue;
                }
                return Err(BackendError::Domain(Error::Conflict(memory.id)));
            }
            let embedding = self.inner.embedder.embed(&memory.content).await?;
            store.insert(&memory, &embedding).await?;
            report.imported += 1;
        }
        for link in links {
            domain::link(&*store, &link.source, &link.target, link.relation)
                .await
                .map_err(BackendError::Domain)?;
        }
        self.freshen(&store).await;
        Ok(report)
    }
}
