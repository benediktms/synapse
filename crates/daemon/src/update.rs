use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPO: &str = "benediktms/synapse";
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60 * 60);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);
const BINARIES: [&str; 2] = ["synd", "syn"];
const EXE_SUFFIX: &str = if cfg!(windows) { ".exe" } else { "" };

#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    assets: HashMap<String, String>,
}

/// Periodically self-update from the latest GitHub release. On success the process
/// exits non-zero, which the crash-only unit (and the CLI's spawn-on-demand) turns
/// into a restart under the new binary.
pub fn spawn() {
    let Some(install_dir) = updatable_install_dir() else {
        tracing::info!("self-update disabled: not a release install");
        return;
    };
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            match check_once(&install_dir).await {
                Ok(Some(version)) => {
                    tracing::info!("updated to {version}; exiting so the unit restarts");
                    std::process::exit(1);
                }
                Ok(None) => {}
                Err(e) => tracing::debug!("update check failed: {e}"),
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

/// Returns the directory whose binaries this process may replace, or None when
/// self-update must stay off: an opt-out, an unsupported platform, or a dev build
/// (a binary under a cargo `target/` dir is owned by `just install`, not releases).
fn updatable_install_dir() -> Option<PathBuf> {
    if std::env::var_os("SYNAPSE_NO_SELF_UPDATE").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    target_triple()?;
    let exe = std::env::current_exe().ok()?;
    let canonical = exe.canonicalize().ok()?;
    if canonical.components().any(|c| c.as_os_str() == "target") {
        return None;
    }
    canonical.parent().map(Path::to_path_buf)
}

fn target_triple() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        Some("aarch64-pc-windows-msvc")
    } else {
        None
    }
}

async fn check_once(install_dir: &Path) -> Result<Option<String>, String> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("synd/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;

    let manifest_url = format!("https://github.com/{REPO}/releases/latest/download/manifest.json");
    let manifest: Manifest = fetch(&client, &manifest_url)
        .await
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))?;

    if !is_newer(&manifest.version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let triple = target_triple().ok_or("unsupported platform")?;

    let mut staged = Vec::new();
    for bin in BINARIES {
        let asset = format!("{bin}-{triple}{EXE_SUFFIX}");
        let expected = manifest
            .assets
            .get(&asset)
            .ok_or_else(|| format!("release {} has no asset {asset}", manifest.version))?;
        let url = format!(
            "https://github.com/{REPO}/releases/download/v{}/{asset}",
            manifest.version
        );
        let bytes = fetch(&client, &url).await?;
        let actual = hex_sha256(&bytes);
        if &actual != expected {
            return Err(format!(
                "checksum mismatch for {asset}: {actual} != {expected}"
            ));
        }
        let path = stage(install_dir, bin, &bytes)?;
        staged.push((bin, path));
    }

    for (bin, path) in staged {
        swap(install_dir, bin, &path)?;
    }
    Ok(Some(manifest.version))
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("GET {url}: {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("GET {url}: {e}"))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Strict x.y.z compare; anything unparseable is never "newer".
fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// Write the verified bytes next to the final destination so the rename in [`swap`]
/// stays on one filesystem and is atomic.
fn stage(install_dir: &Path, bin: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = install_dir.join(format!(".{bin}.update"));
    std::fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(path)
}

fn swap(install_dir: &Path, bin: &str, staged: &Path) -> Result<(), String> {
    let current = install_dir.join(format!("{bin}{EXE_SUFFIX}"));
    let backup = install_dir.join(format!("{bin}{EXE_SUFFIX}.bak"));
    if current.exists() {
        std::fs::rename(&current, &backup)
            .map_err(|e| format!("backup {}: {e}", current.display()))?;
    }
    if let Err(e) = std::fs::rename(staged, &current) {
        let _ = std::fs::rename(&backup, &current);
        return Err(format!("install {}: {e}", current.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_is_numeric_not_lexicographic() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("1.0", "0.1.0"));
        assert!(!is_newer("1.0.0.0", "0.1.0"));
    }

    #[test]
    fn manifest_parses_the_release_layout() {
        let json = r#"{"version":"0.2.0","assets":{"synd-aarch64-apple-darwin":"ab12"}}"#;
        let manifest: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(
            manifest.assets["synd-aarch64-apple-darwin"],
            "ab12".to_string()
        );
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn swap_replaces_the_binary_and_keeps_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let current = format!("synd{EXE_SUFFIX}");
        std::fs::write(dir.path().join(&current), "old").unwrap();
        let staged = stage(dir.path(), "synd", b"new").unwrap();
        swap(dir.path(), "synd", &staged).unwrap();
        assert_eq!(std::fs::read(dir.path().join(&current)).unwrap(), b"new");
        assert_eq!(
            std::fs::read(dir.path().join(format!("{current}.bak"))).unwrap(),
            b"old"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(&current))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755);
        }
    }

    #[test]
    fn swap_restores_the_backup_when_install_fails() {
        let dir = tempfile::tempdir().unwrap();
        let current = format!("synd{EXE_SUFFIX}");
        std::fs::write(dir.path().join(&current), "old").unwrap();
        let missing = dir.path().join("absent-staged-file");
        assert!(swap(dir.path(), "synd", &missing).is_err());
        assert_eq!(std::fs::read(dir.path().join(&current)).unwrap(), b"old");
    }
}
