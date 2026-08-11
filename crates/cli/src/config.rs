use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Which backend commands talk to: `http` (default, the axum server) or `daemon`
    /// (the local replication daemon). The HTTP path is deleted at cutover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_rules: Vec<WorkspaceRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_rules: Vec<OrgRule>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Http,
    Daemon,
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

    pub fn url(&self) -> &str {
        self.url.as_deref().unwrap_or(api_client::DEFAULT_BASE_URL)
    }

    pub fn token(&self) -> Result<&str, String> {
        self.token
            .as_deref()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| "no token configured; run: syn config set-token <token>".to_string())
    }

    pub fn transport(&self) -> Transport {
        self.transport.unwrap_or_default()
    }
}

pub fn config_path() -> PathBuf {
    base_dir(
        "SYNAPSE_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "APPDATA",
        ".config",
    )
    .join("config.toml")
}

pub fn state_dir() -> PathBuf {
    base_dir(
        "SYNAPSE_STATE_DIR",
        "XDG_STATE_HOME",
        "LOCALAPPDATA",
        ".local/state",
    )
}

fn base_dir(override_var: &str, xdg_var: &str, windows_var: &str, home_suffix: &str) -> PathBuf {
    if let Some(dir) = env_path(override_var) {
        return dir;
    }
    if let Some(dir) = env_path(xdg_var) {
        return dir.join("synapse");
    }
    if cfg!(windows)
        && let Some(dir) = env_path(windows_var)
    {
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

/// On Windows the user-profile ACLs already restrict the directory; there is no
/// mode to set.
pub fn private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    sync_parent(dir)
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
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
#[cfg(unix)]
pub fn sync_parent(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|e| format!("cannot sync {}: {e}", dir.display()))
}

/// Windows cannot open a directory as a plain file (that needs backup semantics),
/// and NTFS journals metadata anyway.
#[cfg(windows)]
pub fn sync_parent(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_toml_with_0600_perms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let config = Config {
            url: Some("https://memory.example".into()),
            token: Some("secret".into()),
            transport: Some(Transport::Daemon),
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

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[org_rules]]"), "{text}");

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.token.as_deref(), Some("secret"));
        assert_eq!(loaded.workspace_rules[0].workspace, "work");
        assert_eq!(loaded.org_rules[0].org, "freshaengineering");
    }

    #[test]
    fn missing_config_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(&dir.path().join("absent.toml")).unwrap();
        assert!(config.token.is_none());
        assert_eq!(config.url(), api_client::DEFAULT_BASE_URL);
        assert!(config.token().is_err());
    }
}
