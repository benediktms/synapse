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
        let (current_model, current_dim) = store
            .embedding_meta()
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?;
        if current_model == model && current_dim == dim {
            report.skipped += 1;
            continue;
        }
        let rows = domain::reembed(&store, embedder)
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?;
        store
            .set_embedding_meta(model, dim)
            .await
            .map_err(|e| format!("workspace {ws}: {e}"))?;
        tracing::info!(workspace = %ws, rows, "reembedded");
        report.converted += 1;
    }
    std::fs::remove_file(target_path(dir)).map_err(|e| e.to_string())?;
    Ok(report)
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

    #[test]
    fn a_corrupt_marker_is_reported_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(target_path(dir.path()), "{not json").unwrap();

        let err = claim_target(dir.path(), "bge-small-en-v1.5", 384).unwrap_err();
        assert!(err.contains("corrupt reembed.target"), "{err}");
    }
}
