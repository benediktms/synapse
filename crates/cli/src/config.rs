use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Unknown keys are ignored rather than rejected, so a file written before the HTTP
/// transport was removed — carrying `url`, `token` or `transport` — still loads.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_rules: Vec<WorkspaceRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_rules: Vec<OrgRule>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceRule {
    pub path: String,
    pub workspace: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OrgRule {
    pub org: String,
    pub workspace: String,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| format!("invalid config at {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("cannot read config at {}: {e}", path.display())),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let dir = path
            .parent()
            .ok_or_else(|| format!("config path {} has no parent", path.display()))?;
        private_dir(dir)?;
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_private(path, text.as_bytes())
    }
}

pub fn config_path() -> PathBuf {
    base_dir("SYNAPSE_CONFIG_DIR", "XDG_CONFIG_HOME", ".config").join("config.toml")
}

pub fn state_dir() -> PathBuf {
    base_dir("SYNAPSE_STATE_DIR", "XDG_STATE_HOME", ".local/state")
}

fn base_dir(override_var: &str, xdg_var: &str, home_suffix: &str) -> PathBuf {
    if let Some(dir) = env_path(override_var) {
        return dir;
    }
    if let Some(dir) = env_path(xdg_var) {
        return dir.join("synapse");
    }
    let home = env_path("HOME").unwrap_or_else(|| PathBuf::from("."));
    home.join(home_suffix).join("synapse")
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn private_dir(dir: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    sync_parent(dir)
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    sync_parent(path)
}

/// A rename or unlink only survives power loss once the directory entry itself is on disk.
pub fn sync_parent(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|e| format!("cannot sync {}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn roundtrips_through_toml_with_0600_perms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let config = Config {
            default_workspace: Some("personal".into()),
            workspace_rules: vec![WorkspaceRule {
                path: "/Users/x/work".into(),
                workspace: "work".into(),
            }],
            org_rules: vec![OrgRule {
                org: "freshaengineering".into(),
                workspace: "work".into(),
            }],
        };
        config.save_to(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[org_rules]]"), "{text}");

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.default_workspace.as_deref(), Some("personal"));
        assert_eq!(loaded.workspace_rules[0].workspace, "work");
        assert_eq!(loaded.org_rules[0].org, "freshaengineering");
    }

    #[test]
    fn missing_config_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(&dir.path().join("absent.toml")).unwrap();
        assert!(config.default_workspace.is_none());
        assert!(config.workspace_rules.is_empty());
        assert!(config.org_rules.is_empty());
    }

    /// A file written before the HTTP transport was removed still loads: its `url`, `token`
    /// and `transport` keys are ignored rather than rejected, so no one has to hand-edit a
    /// config to keep working.
    #[test]
    fn a_config_from_the_http_era_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "url = \"http://127.0.0.1:8737\"\ntoken = \"secret\"\ntransport = \"http\"\n\
             default_workspace = \"work\"\n\n[[org_rules]]\norg = \"benediktms\"\n\
             workspace = \"personal\"\n",
        )
        .unwrap();

        let loaded = Config::load_from(&path).expect("an HTTP-era config must still load");
        assert_eq!(loaded.default_workspace.as_deref(), Some("work"));
        assert_eq!(loaded.org_rules[0].workspace, "personal");

        loaded.save_to(&path).unwrap();
        let rewritten = fs::read_to_string(&path).unwrap();
        for retired in ["url", "token", "transport"] {
            assert!(
                !rewritten.contains(retired),
                "a rewrite kept the retired {retired} key: {rewritten}"
            );
        }
    }
}
