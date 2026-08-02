use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use api::PutMemoryBody;
use api_client::SynapseApiClient;
use serde::{Deserialize, Serialize};

use crate::config::{private_dir, state_dir, write_private};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingSave {
    pub id: String,
    pub workspace: String,
    pub queued_at: u64,
    pub body: PutMemoryBody,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

#[derive(Debug, Default)]
pub struct FlushReport {
    pub sent: Vec<String>,
    pub dead_lettered: Vec<(String, String)>,
    pub deferred: Option<String>,
}

pub struct Outbox {
    dir: PathBuf,
}

impl Outbox {
    pub fn open() -> Result<Self, String> {
        Self::at(state_dir().join("outbox"))
    }

    pub fn at(dir: PathBuf) -> Result<Self, String> {
        private_dir(&dir)?;
        private_dir(&dir.join("dead-letter"))?;
        Ok(Self { dir })
    }

    fn dead_dir(&self) -> PathBuf {
        self.dir.join("dead-letter")
    }

    pub fn enqueue(&self, item: &PendingSave) -> Result<(), String> {
        let _lock = lock(&self.dir.join(".lock"), true)?;
        self.write_item(&self.dir, item)
    }

    fn write_item(&self, dir: &Path, item: &PendingSave) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(item).map_err(|e| e.to_string())?;
        write_private(&dir.join(file_name(item)), &bytes)
    }

    pub fn pending(&self) -> Result<Vec<(PathBuf, PendingSave)>, String> {
        read_items(&self.dir)
    }

    pub fn dead_letters(&self) -> Result<Vec<(PathBuf, PendingSave)>, String> {
        read_items(&self.dead_dir())
    }

    /// Sends queued saves oldest-first, stopping at the first retryable failure so
    /// ordering survives an outage. Returns an empty report if another `syn` holds the lock.
    pub fn flush(&self, client: &SynapseApiClient) -> Result<FlushReport, String> {
        let mut report = FlushReport::default();
        let Some(_lock) = lock(&self.dir.join(".lock"), false)? else {
            return Ok(report);
        };
        for (path, item) in self.pending()? {
            match client.save(&item.workspace, &item.id, &item.body) {
                Ok(_) => {
                    remove(&path)?;
                    report.sent.push(item.id);
                }
                Err(err) if err.is_retryable() => {
                    report.deferred = Some(err.to_string());
                    break;
                }
                Err(err) => {
                    let failure = err.to_string();
                    let dead = PendingSave {
                        failure: Some(failure.clone()),
                        ..item.clone()
                    };
                    self.write_item(&self.dead_dir(), &dead)?;
                    remove(&path)?;
                    report.dead_lettered.push((item.id, failure));
                }
            }
        }
        Ok(report)
    }

    pub fn reassign(&self, workspace: &str, id: Option<&str>) -> Result<usize, String> {
        let _lock = lock(&self.dir.join(".lock"), true)?;
        let mut moved = 0;
        for (path, item) in self.selected(id)? {
            let item = PendingSave {
                workspace: workspace.to_string(),
                failure: None,
                ..item
            };
            self.write_item(&self.dir, &item)?;
            if path.parent() != Some(self.dir.as_path()) {
                remove(&path)?;
            }
            moved += 1;
        }
        Ok(moved)
    }

    pub fn discard(&self, id: Option<&str>) -> Result<usize, String> {
        let _lock = lock(&self.dir.join(".lock"), true)?;
        let mut discarded = 0;
        for (path, _) in self.selected(id)? {
            remove(&path)?;
            discarded += 1;
        }
        Ok(discarded)
    }

    fn selected(&self, id: Option<&str>) -> Result<Vec<(PathBuf, PendingSave)>, String> {
        let mut items = self.pending()?;
        items.extend(self.dead_letters()?);
        Ok(match id {
            Some(id) => items.into_iter().filter(|(_, it)| it.id == id).collect(),
            None => items,
        })
    }
}

fn file_name(item: &PendingSave) -> String {
    format!("{:013}-{}.json", item.queued_at, item.id)
}

fn read_items(dir: &Path) -> Result<Vec<(PathBuf, PendingSave)>, String> {
    let mut paths: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    paths.sort();
    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let item: PendingSave = serde_json::from_slice(&bytes)
            .map_err(|e| format!("corrupt outbox item {}: {e}", path.display()))?;
        items.push((path, item));
    }
    Ok(items)
}

fn remove(path: &Path) -> Result<(), String> {
    fs::remove_file(path).map_err(|e| format!("cannot remove {}: {e}", path.display()))
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Dropping the returned file closes the descriptor, which releases the flock.
fn lock(path: &Path, block: bool) -> Result<Option<fs::File>, String> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot open lock {}: {e}", path.display()))?;
    let operation = if block {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        return Ok(Some(file));
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) if !block => Ok(None),
        _ => Err(format!("cannot lock {}: {err}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn item(id: &str, queued_at: u64) -> PendingSave {
        PendingSave {
            id: id.to_string(),
            workspace: "work".into(),
            queued_at,
            body: PutMemoryBody {
                content: "fact".into(),
                kind: "project".into(),
                scope: "workspace".into(),
                tags: vec![],
            },
            failure: None,
        }
    }

    #[test]
    fn queued_items_are_private_and_ordered_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Outbox::at(dir.path().join("outbox")).unwrap();
        outbox.enqueue(&item("m_bbb", 300)).unwrap();
        outbox.enqueue(&item("m_aaa", 100)).unwrap();
        outbox.enqueue(&item("m_ccc", 200)).unwrap();

        let ids: Vec<String> = outbox
            .pending()
            .unwrap()
            .into_iter()
            .map(|(_, it)| it.id)
            .collect();
        assert_eq!(ids, ["m_aaa", "m_ccc", "m_bbb"]);

        for (path, _) in outbox.pending().unwrap() {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
        let dir_mode = fs::metadata(dir.path().join("outbox").join("dead-letter"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn reassign_moves_dead_letters_back_to_pending() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Outbox::at(dir.path().join("outbox")).unwrap();
        let dead = PendingSave {
            failure: Some("404 unknown workspace".into()),
            ..item("m_aaa", 100)
        };
        outbox.write_item(&outbox.dead_dir(), &dead).unwrap();

        assert_eq!(outbox.reassign("personal", None).unwrap(), 1);
        assert!(outbox.dead_letters().unwrap().is_empty());
        let pending = outbox.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.workspace, "personal");
        assert!(pending[0].1.failure.is_none());

        assert_eq!(outbox.discard(Some("m_zzz")).unwrap(), 0);
        assert_eq!(outbox.discard(None).unwrap(), 1);
        assert!(outbox.pending().unwrap().is_empty());
    }

    #[test]
    fn a_second_holder_gets_none_rather_than_blocking() {
        let dir = tempfile::tempdir().unwrap();
        Outbox::at(dir.path().join("outbox")).unwrap();
        let lock_path = dir.path().join("outbox").join(".lock");
        let held = lock(&lock_path, false).unwrap();
        assert!(held.is_some());
        assert!(lock(&lock_path, false).unwrap().is_none());
    }
}
