use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use adapters_fastembed::FastEmbedder;
use adapters_sqlite::SqliteStore;
use api::{Backend, BackendError, RestoreReport};
use domain::{
    ContextDigest, EditRequest, Embedder, Error, Memory, MemoryId, RecallHit, RecallRequest,
    SaveOutcome, SaveRequest, Timestamp, Workspace, WorkspaceHits,
};
use tokio::sync::RwLock;

pub const REEMBED_TARGET_FILE: &str = "reembed.target";

#[derive(Clone)]
pub struct App {
    inner: Arc<AppInner>,
}

struct AppInner {
    data_dir: PathBuf,
    embedder: Arc<FastEmbedder>,
    stores: RwLock<HashMap<Workspace, Arc<SqliteStore>>>,
    ready: Result<(), String>,
}

impl App {
    pub async fn boot(data_dir: PathBuf, embedder: Arc<FastEmbedder>) -> Result<Self, String> {
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("cannot create data dir {}: {e}", data_dir.display()))?;
        let mut problems = Vec::new();
        if data_dir.join(REEMBED_TARGET_FILE).exists() {
            problems.push(format!(
                "a reembed is in progress ({REEMBED_TARGET_FILE} present); \
                 run `synapse-server reembed` to completion"
            ));
        }
        let mut registry = Vec::new();
        match scan_workspaces(&data_dir) {
            Ok(scanned) => registry = scanned,
            Err(problem) => problems.push(problem),
        }
        let shared = Workspace::shared();
        if !registry.iter().any(|(ws, _)| *ws == shared) {
            let path = db_path(&data_dir, &shared);
            registry.push((shared, path));
        }
        let mut stores = HashMap::new();
        for (ws, path) in registry {
            match SqliteStore::open(
                &path,
                adapters_fastembed::MODEL_NAME,
                adapters_fastembed::DIMENSION,
            )
            .await
            {
                Ok(store) => {
                    stores.insert(ws, Arc::new(store));
                }
                Err(e) => problems.push(format!("workspace {ws}: {e}")),
            }
        }
        let ready = if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        };
        Ok(Self {
            inner: Arc::new(AppInner {
                data_dir,
                embedder,
                stores: RwLock::new(stores),
                ready,
            }),
        })
    }

    async fn store(&self, ws: &Workspace) -> Result<Arc<SqliteStore>, BackendError> {
        if let Some(store) = self.inner.stores.read().await.get(ws) {
            return Ok(Arc::clone(store));
        }
        let path = db_path(&self.inner.data_dir, ws);
        if !path.exists() {
            return Err(BackendError::UnknownWorkspace(ws.clone()));
        }
        let mut stores = self.inner.stores.write().await;
        if let Some(store) = stores.get(ws) {
            return Ok(Arc::clone(store));
        }
        let store = Arc::new(
            SqliteStore::open(
                &path,
                adapters_fastembed::MODEL_NAME,
                adapters_fastembed::DIMENSION,
            )
            .await
            .map_err(BackendError::Domain)?,
        );
        stores.insert(ws.clone(), Arc::clone(&store));
        Ok(store)
    }

    async fn active_and_shared(
        &self,
        ws: &Workspace,
    ) -> Result<(Arc<SqliteStore>, Option<(Workspace, Arc<SqliteStore>)>), BackendError> {
        let active = self.store(ws).await?;
        if ws.is_shared() {
            return Ok((active, None));
        }
        let shared = Workspace::shared();
        let shared_store = self.store(&shared).await?;
        Ok((active, Some((shared, shared_store))))
    }
}

impl Backend for App {
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
        let path = db_path(&self.inner.data_dir, ws);
        let existed = stores.contains_key(ws) || path.exists();
        if !stores.contains_key(ws) {
            let store = SqliteStore::open(
                &path,
                adapters_fastembed::MODEL_NAME,
                adapters_fastembed::DIMENSION,
            )
            .await
            .map_err(BackendError::Domain)?;
            stores.insert(ws.clone(), Arc::new(store));
        }
        Ok(!existed)
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>, BackendError> {
        let scanned = scan_workspaces(&self.inner.data_dir)
            .map_err(|e| BackendError::Domain(Error::Store(e)))?;
        Ok(scanned.into_iter().map(|(ws, _)| ws).collect())
    }

    async fn save(&self, ws: &Workspace, req: SaveRequest) -> Result<SaveOutcome, BackendError> {
        let store = self.store(ws).await?;
        domain::save(&*store, &*self.inner.embedder, req, self.now())
            .await
            .map_err(Into::into)
    }

    async fn edit(
        &self,
        ws: &Workspace,
        id: &MemoryId,
        req: EditRequest,
    ) -> Result<Memory, BackendError> {
        let store = self.store(ws).await?;
        domain::edit(&*store, &*self.inner.embedder, id, req, self.now())
            .await
            .map_err(Into::into)
    }

    async fn forget(&self, ws: &Workspace, id: &MemoryId) -> Result<(), BackendError> {
        let store = self.store(ws).await?;
        domain::forget(&*store, id).await.map_err(Into::into)
    }

    async fn get(&self, ws: &Workspace, id: &MemoryId) -> Result<Option<Memory>, BackendError> {
        use domain::Store;
        let store = self.store(ws).await?;
        store.get(id).await.map_err(Into::into)
    }

    async fn list(&self, ws: &Workspace) -> Result<Vec<Memory>, BackendError> {
        let store = self.store(ws).await?;
        domain::list_memories(&*store).await.map_err(Into::into)
    }

    async fn recall(
        &self,
        ws: &Workspace,
        req: &RecallRequest,
    ) -> Result<Vec<RecallHit>, BackendError> {
        let (active, shared) = self.active_and_shared(ws).await?;
        let shared_side = shared
            .as_ref()
            .map(|(shared_ws, store)| (shared_ws, &**store));
        domain::recall(&*self.inner.embedder, (ws, &*active), shared_side, req)
            .await
            .map_err(Into::into)
    }

    async fn recall_all(&self, req: &RecallRequest) -> Result<Vec<WorkspaceHits>, BackendError> {
        let workspaces = self.workspaces().await?;
        let mut sides = Vec::with_capacity(workspaces.len());
        for ws in workspaces {
            let store = self.store(&ws).await?;
            sides.push((ws, store));
        }
        let refs: Vec<(Workspace, &SqliteStore)> = sides
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
        let shared_side = shared
            .as_ref()
            .map(|(shared_ws, store)| (shared_ws, &**store));
        domain::context_digest((ws, &*active), shared_side, project)
            .await
            .map_err(Into::into)
    }

    async fn restore(
        &self,
        ws: &Workspace,
        memories: Vec<Memory>,
    ) -> Result<RestoreReport, BackendError> {
        use domain::Store;
        let store = self.store(ws).await?;
        let mut report = RestoreReport::default();
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
        Ok(report)
    }
}

pub fn db_path(data_dir: &Path, ws: &Workspace) -> PathBuf {
    data_dir.join(format!("{ws}.db"))
}

pub fn scan_workspaces(data_dir: &Path) -> Result<Vec<(Workspace, PathBuf)>, String> {
    let entries = std::fs::read_dir(data_dir)
        .map_err(|e| format!("cannot read data dir {}: {e}", data_dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("db") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("non-UTF-8 db filename: {}", path.display()))?;
        let ws = if stem == "shared" {
            Workspace::shared()
        } else {
            Workspace::new(stem)
                .map_err(|e| format!("invalid workspace db file {}: {e}", path.display()))?
        };
        out.push((ws, path));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReembedReport {
    pub converted: usize,
    pub skipped: usize,
}

pub async fn reembed<E: Embedder>(
    data_dir: &Path,
    model: &str,
    dim: usize,
    embedder: &E,
) -> Result<ReembedReport, String> {
    let target_path = data_dir.join(REEMBED_TARGET_FILE);
    if target_path.exists() {
        let recorded: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&target_path).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("corrupt {REEMBED_TARGET_FILE}: {e}"))?;
        if recorded["model"] != model || recorded["dim"] != dim {
            return Err(format!(
                "a reembed towards {} ({} dims) is already in progress; \
                 finish it before targeting {model} ({dim} dims)",
                recorded["model"], recorded["dim"]
            ));
        }
    } else {
        std::fs::write(
            &target_path,
            serde_json::json!({ "model": model, "dim": dim }).to_string(),
        )
        .map_err(|e| format!("cannot write {REEMBED_TARGET_FILE}: {e}"))?;
    }
    let mut report = ReembedReport::default();
    for (ws, path) in scan_workspaces(data_dir)? {
        let store = SqliteStore::open_maintenance(&path)
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?;
        let (current_model, current_dim) = store
            .embedding_meta()
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?;
        if current_model == model && current_dim == dim {
            report.skipped += 1;
            continue;
        }
        let rows = store
            .reembed(model, dim, embedder)
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?;
        tracing::info!(workspace = %ws, rows, "reembedded");
        report.converted += 1;
    }
    std::fs::remove_file(&target_path).map_err(|e| e.to_string())?;
    Ok(report)
}

pub async fn fts_rebuild(data_dir: &Path) -> Result<usize, String> {
    let mut rebuilt = 0;
    for (ws, path) in scan_workspaces(data_dir)? {
        let store = SqliteStore::open_maintenance(&path)
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?;
        store
            .fts_rebuild()
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?;
        rebuilt += 1;
    }
    Ok(rebuilt)
}
