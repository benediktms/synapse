use std::path::{Path, PathBuf};

use adapters_libsql::LibsqlStore;
use domain::Embedder;

use crate::config::{Config, open_binding, resolve_bindings};

/// Written for the duration of a conversion. A run that dies leaves it behind, and the daemon
/// refuses to boot while it is present: a half-converted store answers recall by comparing
/// vectors from two different models.
pub const REEMBED_TARGET_FILE: &str = "reembed.target";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReembedReport {
    pub converted: usize,
    pub skipped: usize,
}

pub fn target_path(dir: &Path) -> PathBuf {
    dir.join(REEMBED_TARGET_FILE)
}

/// What an unfinished conversion was aiming at, if one is in flight.
pub fn pending_target(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(target_path(dir)).ok()?;
    let recorded: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(describe(&recorded))
}

fn describe(recorded: &serde_json::Value) -> String {
    format!(
        "{} ({} dims)",
        recorded["model"].as_str().unwrap_or("an unnamed model"),
        recorded["dim"]
    )
}

fn claim_target(dir: &Path, model: &str, dim: usize) -> Result<(), String> {
    let path = target_path(dir);
    if path.exists() {
        let recorded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?)
                .map_err(|e| format!("corrupt {REEMBED_TARGET_FILE}: {e}"))?;
        if recorded["model"] != model || recorded["dim"] != dim {
            return Err(format!(
                "a reembed towards {} is already in progress; \
                 finish it before targeting {model} ({dim} dims)",
                describe(&recorded)
            ));
        }
        return Ok(());
    }
    std::fs::write(
        &path,
        serde_json::json!({ "model": model, "dim": dim }).to_string(),
    )
    .map_err(|e| format!("cannot write {REEMBED_TARGET_FILE}: {e}"))
}

/// Re-embed every replica that is not already on `model`, then record the conversion.
///
/// A workspace that cannot be resolved or converted fails the run and leaves the marker in
/// place. Converted stores are skipped on the next attempt, so finishing is a rerun.
pub async fn reembed<E: Embedder>(
    dir: &Path,
    config: &Config,
    model: &str,
    dim: usize,
    embedder: &E,
) -> Result<ReembedReport, String> {
    claim_target(dir, model, dim)?;
    let (bindings, problems) = resolve_bindings(dir, config).await;
    if !problems.is_empty() {
        return Err(format!(
            "cannot reach every workspace, so none were converted: {}",
            problems.join("; ")
        ));
    }
    let mut report = ReembedReport::default();
    for binding in bindings {
        let ws = binding.workspace.clone();
        let store = LibsqlStore::open_maintenance(
            &binding.replica,
            binding.url.clone(),
            binding.token.clone(),
            dim,
        )
        .await
        .map_err(|e| format!("workspace {ws}: {e}"))?;
        match convert_store(&store, model, dim, embedder)
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?
        {
            Some(rows) => {
                tracing::info!(workspace = %ws, rows, "reembedded");
                report.converted += 1;
            }
            None => report.skipped += 1,
        }
    }
    std::fs::remove_file(target_path(dir)).map_err(|e| e.to_string())?;
    Ok(report)
}

/// Bring one store to `model`, returning how many vectors were rewritten, or None if it was
/// already there. The stamp lands only after the walk: it is what lets the ordinary open path
/// accept the store again, so recording it early would readmit half-converted vectors.
async fn convert_store<E: Embedder>(
    store: &LibsqlStore,
    model: &str,
    dim: usize,
    embedder: &E,
) -> Result<Option<usize>, domain::Error> {
    let (current_model, current_dim) = store.embedding_meta().await?;
    if current_model == model && current_dim == dim {
        return Ok(None);
    }
    let rows = domain::reembed(store, embedder).await?;
    store.set_embedding_meta(model, dim).await?;
    Ok(Some(rows))
}

/// Rebuild every replica's keyword index. Uses the ordinary open path: a store on the wrong
/// model needs `reembed` before its index is worth repairing.
pub async fn fts_rebuild(dir: &Path, config: &Config) -> Result<usize, String> {
    let (bindings, problems) = resolve_bindings(dir, config).await;
    if !problems.is_empty() {
        return Err(format!(
            "cannot reach every workspace: {}",
            problems.join("; ")
        ));
    }
    let mut rebuilt = 0;
    for binding in bindings {
        let ws = binding.workspace.clone();
        open_binding(&binding)
            .await?
            .fts_rebuild()
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?;
        rebuilt += 1;
    }
    Ok(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_is_resumable_by_the_same_target_and_refused_by_another() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(pending_target(dir.path()), None);

        claim_target(dir.path(), "bge-small-en-v1.5", 384).unwrap();
        assert_eq!(
            pending_target(dir.path()),
            Some("bge-small-en-v1.5 (384 dims)".to_string())
        );

        claim_target(dir.path(), "bge-small-en-v1.5", 384)
            .expect("re-claiming the same target resumes it");

        let refused = claim_target(dir.path(), "other-model", 512).unwrap_err();
        assert!(refused.contains("already in progress"), "{refused}");
    }

    /// Drives the conversion over a branch database rather than the walk's own binding
    /// resolution, which needs the platform API and the machine's config. Create the branch
    /// first: `turso db create synapse-reembed-smoke --from-db personal`. The `synapse-` prefix
    /// keeps a stranded branch from being adopted as a workspace.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Turso Cloud credentials, a branch database, and network"]
    async fn a_branch_converts_to_the_runtime_model_and_reopens() {
        use adapters_fastembed::{DIMENSION, FastEmbedder, MODEL_NAME};
        use domain::Store;

        let Some(token) = std::env::var("SYNAPSE_TURSO_TEST_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        else {
            eprintln!("skipping: SYNAPSE_TURSO_TEST_TOKEN is not set");
            return;
        };
        let org =
            std::env::var("SYNAPSE_TURSO_TEST_ORG").unwrap_or_else(|_| "benediktms".to_string());
        let name = std::env::var("SYNAPSE_REEMBED_TEST_DB")
            .unwrap_or_else(|_| "synapse-reembed-smoke".to_string());

        let platform = adapters_libsql::TursoPlatform::new();
        let group = platform.ensure_group(&org, &token).await.unwrap();
        let db_token = platform.mint_db_token(&org, &token, &group).await.unwrap();
        let (dbs, _) = platform.list_databases(&org, &token).await.unwrap();
        let db = dbs
            .iter()
            .find(|db| db.name == name)
            .unwrap_or_else(|| panic!("no database named {name} in {org}"));

        let dir = tempfile::tempdir().unwrap();
        let replica = dir.path().join("branch.db");
        let embedder = tokio::task::spawn_blocking(|| FastEmbedder::new().expect("model init"))
            .await
            .expect("join");

        let store =
            LibsqlStore::open_maintenance(&replica, db.url.clone(), db_token.clone(), DIMENSION)
                .await
                .unwrap();
        let memories = store.list().await.unwrap();
        assert!(!memories.is_empty(), "{name} came back empty");

        let no_model_produces_this = vec![0.0f32; DIMENSION];
        let mut vectors_before_the_walk = Vec::new();
        for memory in &memories {
            let (_, embedding) = store.get_with_embedding(&memory.id).await.unwrap().unwrap();
            vectors_before_the_walk.push((memory.id.clone(), embedding));
            store
                .set_embedding(&memory.id, &no_model_produces_this)
                .await
                .unwrap();
        }
        store
            .set_embedding_meta("stale-model", DIMENSION)
            .await
            .unwrap();

        let written = convert_store(&store, MODEL_NAME, DIMENSION, &embedder)
            .await
            .unwrap();
        assert_eq!(written, Some(memories.len()));
        assert_eq!(
            convert_store(&store, MODEL_NAME, DIMENSION, &embedder)
                .await
                .unwrap(),
            None,
            "a converted store is skipped on a rerun"
        );

        for (id, original) in &vectors_before_the_walk {
            let (memory, embedding) = store.get_with_embedding(id).await.unwrap().unwrap();
            assert_eq!(
                &embedding, original,
                "{id} did not come back to its original vector"
            );
            assert_eq!(
                memory.updated_at,
                memories
                    .iter()
                    .find(|m| &m.id == id)
                    .unwrap()
                    .updated_at
                    .clone(),
                "{id} had its timestamp bumped by the walk"
            );
        }

        let reopened = adapters_libsql::LibsqlStore::open(
            dir.path().join("reopened.db"),
            db.url.clone(),
            db_token,
            MODEL_NAME,
            DIMENSION,
        )
        .await
        .expect("a converted store opens on the runtime model");
        assert_eq!(reopened.list().await.unwrap().len(), memories.len());
        eprintln!("{name}: {} memories converted and reopened", memories.len());
    }

    #[test]
    fn a_corrupt_marker_is_reported_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(target_path(dir.path()), "{not json").unwrap();

        let err = claim_target(dir.path(), "bge-small-en-v1.5", 384).unwrap_err();
        assert!(err.contains("corrupt reembed.target"), "{err}");
    }
}
