use std::collections::HashMap;
use std::path::{Path, PathBuf};

use adapters_libsql::{LibsqlStore, TursoPlatform};
use serde::{Deserialize, Serialize};

use domain::Workspace;

const MANIFEST_FILENAME: &str = "workspaces.json";

/// The daemon's config schema lives in daemon-client, so `syn setup` writes exactly what
/// the daemon reads. Per-machine scope decides which orgs appear: personal machines have
/// `benediktms`; work machines also `freshaengineering`.
pub use daemon_client::{DaemonConfig as Config, ScopedOrg, config_path};

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join(MANIFEST_FILENAME)
}

pub fn replica_path(dir: &Path, ws: &Workspace) -> PathBuf {
    dir.join(format!("{ws}.db"))
}

/// A workspace bound to a Turso DB: its replica file path, libsql URL, and the org token used to
/// open and sync it.
#[derive(Clone, Debug)]
pub struct WorkspaceBinding {
    pub workspace: Workspace,
    pub replica: PathBuf,
    pub url: String,
    pub token: String,
}

/// Local cache of workspace -> {url, org} so the daemon can open already-synced replicas while
/// offline (tokens stay in config; the URL and owning org cannot be re-derived offline).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    entries: HashMap<String, Entry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    url: String,
    org: String,
}

impl Manifest {
    pub fn load(dir: &Path) -> Manifest {
        std::fs::read_to_string(manifest_path(dir))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        if let Ok(raw) = serde_json::to_string_pretty(&self) {
            let _ = std::fs::write(manifest_path(dir), raw);
        }
    }

    pub fn entry(&self, ws: &Workspace) -> Option<(&str, &str)> {
        self.entries
            .get(ws.as_str())
            .map(|e| (e.url.as_str(), e.org.as_str()))
    }

    pub fn set(&mut self, ws: &Workspace, url: &str, org: &str) {
        self.entries.insert(
            ws.to_string(),
            Entry {
                url: url.to_string(),
                org: org.to_string(),
            },
        );
    }
}

/// The workspace a Turso DB name maps to. `Workspace::new` rejects the reserved name
/// "shared", but the shared DB does exist remotely and must round-trip through
/// enumeration, or every boot after the first re-creates it and fails on the 409.
fn workspace_named(name: &str) -> Option<Workspace> {
    let shared = Workspace::shared();
    if name == shared.as_str() {
        Some(shared)
    } else {
        Workspace::new(name).ok()
    }
}

fn cached_bindings_for_org(
    dir: &Path,
    manifest: &Manifest,
    org: &ScopedOrg,
) -> Vec<WorkspaceBinding> {
    manifest
        .entries
        .iter()
        .filter(|(_, e)| e.org == org.name)
        .filter_map(|(name, e)| {
            let ws = workspace_named(name)?;
            replica_path(dir, &ws).exists().then(|| WorkspaceBinding {
                workspace: ws.clone(),
                replica: replica_path(dir, &ws),
                url: e.url.clone(),
                token: org.token.clone(),
            })
        })
        .collect()
}

/// Resolve workspace bindings for the scoped orgs, provisioning a missing shared DB in the
/// first reachable org (fully programmatic). An unreachable org degrades to its cached
/// manifest bindings instead of aborting the other orgs. Refreshes the manifest with any
/// URLs learned online. Returns the bindings plus the problems hit along the way.
pub async fn resolve_bindings(dir: &Path, config: &Config) -> (Vec<WorkspaceBinding>, Vec<String>) {
    let platform = TursoPlatform::new();
    let mut manifest = Manifest::load(dir);
    let mut bindings: Vec<WorkspaceBinding> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut online_org: Option<&ScopedOrg> = None;

    for org in &config.scoped_orgs {
        match platform.list_databases(&org.name, &org.token).await {
            Ok(dbs) => {
                online_org.get_or_insert(org);
                for db in dbs {
                    let Some(ws) = workspace_named(&db.name) else {
                        continue;
                    };
                    manifest.set(&ws, &db.url, &org.name);
                    bindings.push(WorkspaceBinding {
                        workspace: ws.clone(),
                        replica: replica_path(dir, &ws),
                        url: db.url,
                        token: org.token.clone(),
                    });
                }
            }
            Err(e) => {
                problems.push(format!(
                    "enumerate {}: {e}; falling back to cached bindings",
                    org.name
                ));
                bindings.extend(cached_bindings_for_org(dir, &manifest, org));
            }
        }
    }

    // Provision the shared workspace in the first reachable org if no org hosts one yet.
    let shared = Workspace::shared();
    if !bindings.iter().any(|b| b.workspace == shared)
        && let Some(org) = online_org
    {
        match platform
            .create_database(&org.name, &org.token, shared.as_str())
            .await
        {
            Ok(db) => {
                manifest.set(&shared, &db.url, &org.name);
                bindings.push(WorkspaceBinding {
                    workspace: shared.clone(),
                    replica: replica_path(dir, &shared),
                    url: db.url,
                    token: org.token.clone(),
                });
            }
            Err(e) => problems.push(format!("create shared in {}: {e}", org.name)),
        }
    }

    manifest.save(dir);
    (bindings, problems)
}

/// Open a workspace's replica, online or offline. Offline opens use the cached manifest (url +
/// org) and work once the replica has synced at least once; the very first open of a workspace
/// requires the network (it must reach the primary to bootstrap).
pub async fn open_binding(binding: &WorkspaceBinding) -> Result<LibsqlStore, String> {
    LibsqlStore::open(
        &binding.replica,
        binding.url.clone(),
        binding.token.clone(),
        adapters_fastembed::MODEL_NAME,
        adapters_fastembed::DIMENSION,
    )
    .await
    .map_err(|e| format!("open {}: {e}", binding.workspace))
}

/// Bindings available offline from the cached manifest, using org tokens from config.
pub fn offline_bindings(dir: &Path, config: &Config) -> Vec<WorkspaceBinding> {
    let manifest = Manifest::load(dir);
    config
        .scoped_orgs
        .iter()
        .flat_map(|org| cached_bindings_for_org(dir, &manifest, org))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::workspace_named;
    use domain::Workspace;

    #[test]
    fn shared_db_name_maps_to_the_shared_workspace() {
        assert_eq!(workspace_named("shared"), Some(Workspace::shared()));
        assert_eq!(
            workspace_named("work"),
            Some(Workspace::new("work").unwrap())
        );
        assert_eq!(workspace_named("Not A Workspace"), None);
    }
}
