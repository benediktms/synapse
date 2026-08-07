use std::collections::HashMap;
use std::path::{Path, PathBuf};

use adapters_libsql::{LibsqlStore, TursoPlatform};
use serde::{Deserialize, Serialize};

use domain::Workspace;

const CONFIG_FILENAME: &str = "daemon.toml";
const MANIFEST_FILENAME: &str = "workspaces.json";

/// One org this machine replicates, with its org-scoped Turso token. Per-machine scope decides
/// which orgs appear: personal machines have `benediktms`; work machines also `freshaengineering`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScopedOrg {
    pub name: String,
    pub token: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    pub scoped_orgs: Vec<ScopedOrg>,
}

impl Config {
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let toml = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, toml).map_err(|e| e.to_string())
    }

    pub fn load(path: &Path) -> Result<Config, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        toml::from_str(&raw).map_err(|e| e.to_string())
    }
}

pub fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILENAME)
}

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

/// Resolve workspace bindings for the scoped orgs, provisioning a missing shared DB in the first
/// scoped org (fully programmatic). Refreshes the manifest with any URLs learned online.
pub async fn resolve_bindings(
    dir: &Path,
    config: &Config,
) -> Result<Vec<WorkspaceBinding>, String> {
    let platform = TursoPlatform::new();
    let mut manifest = Manifest::load(dir);
    let mut bindings: Vec<WorkspaceBinding> = Vec::new();

    for org in &config.scoped_orgs {
        let dbs = platform
            .list_databases(&org.name, &org.token)
            .await
            .map_err(|e| format!("enum {}: {e}", org.name))?;
        for db in dbs {
            let Ok(ws) = Workspace::new(&db.name) else {
                continue; // DB name isn't a valid workspace id; ignore
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

    // Provision the shared workspace in the first scoped org if no org hosts one yet.
    let shared = Workspace::shared();
    if !bindings.iter().any(|b| b.workspace == shared)
        && let Some(org) = config.scoped_orgs.first()
    {
        let db = platform
            .create_database(&org.name, &org.token, shared.as_str())
            .await
            .map_err(|e| format!("create shared in {}: {e}", org.name))?;
        manifest.set(&shared, &db.url, &org.name);
        bindings.push(WorkspaceBinding {
            workspace: shared.clone(),
            replica: replica_path(dir, &shared),
            url: db.url,
            token: org.token.clone(),
        });
    }

    manifest.save(dir);
    Ok(bindings)
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
    let tokens: HashMap<&str, &str> = config
        .scoped_orgs
        .iter()
        .map(|o| (o.name.as_str(), o.token.as_str()))
        .collect();
    config
        .scoped_orgs
        .iter()
        .flat_map(|org| {
            manifest
                .entries
                .iter()
                .filter(move |(_, e)| e.org == org.name)
                .filter_map(|(name, e)| {
                    let ws = Workspace::new(name).ok()?;
                    if !replica_path(dir, &ws).exists() {
                        return None;
                    }
                    Some(WorkspaceBinding {
                        workspace: ws.clone(),
                        replica: replica_path(dir, &ws),
                        url: e.url.clone(),
                        token: tokens.get(org.name.as_str())?.to_string(),
                    })
                })
        })
        .collect()
}
