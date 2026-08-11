use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use api::{PutMemoryBody, PutPreferenceBody};
use serde::{Deserialize, Serialize};

use crate::client::Client;
use crate::config::{private_dir, state_dir, sync_parent, write_private};

/// A failed send, classified by the transport: only a definitive rejection dead-letters
/// an item; anything that may succeed on replay stays queued.
pub struct SendFailure {
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "target", rename_all = "lowercase")]
pub enum SaveTarget {
    Memory {
        workspace: String,
        body: PutMemoryBody,
    },
    Preference {
        body: PutPreferenceBody,
    },
}

impl SaveTarget {
    pub fn label(&self) -> String {
        match self {
            Self::Memory { workspace, body } => format!("{workspace} \u{b7} {}", body.scope),
            Self::Preference { .. } => "everywhere".to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingSave {
    pub id: String,
    pub queued_at: u64,
    #[serde(flatten)]
    pub target: SaveTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

#[derive(Debug, Default)]
pub struct FlushReport {
    pub sent: Vec<String>,
    pub dead_lettered: Vec<(String, String)>,
    pub deferred: Option<String>,
    pub still_queued: usize,
    pub oldest_queued_at: Option<u64>,
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
    /// ordering survives an outage. `budget` bounds the whole call — lock wait included —
    /// for callers that must return within a deadline; `None` waits for the lock and
    /// drains the queue. `still_queued` always reports what remains unsent.
    pub fn flush(&self, client: &Client, budget: Option<Duration>) -> Result<FlushReport, String> {
        let mut report = FlushReport::default();
        let deadline = budget.map(|budget| Instant::now() + budget);
        let held = match deadline {
            Some(deadline) => lock_until(&self.dir.join(".lock"), deadline)?,
            None => lock(&self.dir.join(".lock"), true)?,
        };
        let Some(_lock) = held else {
            report.deferred = Some("another syn is flushing the outbox".to_string());
            self.record_backlog(&mut report)?;
            return Ok(report);
        };
        for (path, item) in self.pending()? {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                report.deferred = Some("flush budget spent before the queue drained".to_string());
                break;
            }
            match client.send_save(&item.id, &item.target) {
                Ok(()) => {
                    remove(&path)?;
                    report.sent.push(item.id);
                }
                Err(err) if err.retryable => {
                    report.deferred = Some(err.message);
                    break;
                }
                Err(err) => {
                    let dead = PendingSave {
                        failure: Some(err.message.clone()),
                        ..item.clone()
                    };
                    self.write_item(&self.dead_dir(), &dead)?;
                    remove(&path)?;
                    report.dead_lettered.push((item.id, err.message));
                }
            }
        }
        self.record_backlog(&mut report)?;
        Ok(report)
    }

    fn record_backlog(&self, report: &mut FlushReport) -> Result<(), String> {
        let pending = self.pending()?;
        report.still_queued = pending.len();
        report.oldest_queued_at = pending.first().map(|(_, item)| item.queued_at);
        Ok(())
    }

    /// Preferences belong to no workspace, so they are reported as skipped rather
    /// than silently rewritten into one.
    pub fn reassign(&self, workspace: &str, id: Option<&str>) -> Result<(usize, usize), String> {
        let _lock = lock(&self.dir.join(".lock"), true)?;
        let (mut moved, mut skipped) = (0, 0);
        for (path, item) in self.selected(id)? {
            let SaveTarget::Memory { body, .. } = item.target else {
                skipped += 1;
                continue;
            };
            let item = PendingSave {
                target: SaveTarget::Memory {
                    workspace: workspace.to_string(),
                    body,
                },
                failure: None,
                ..item
            };
            self.write_item(&self.dir, &item)?;
            if path.parent() != Some(self.dir.as_path()) {
                remove(&path)?;
            }
            moved += 1;
        }
        Ok((moved, skipped))
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
    fs::remove_file(path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    sync_parent(path)
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

fn lock_until(path: &Path, deadline: Instant) -> Result<Option<fs::File>, String> {
    loop {
        if let Some(file) = lock(path, false)? {
            return Ok(Some(file));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn item(id: &str, queued_at: u64) -> PendingSave {
        PendingSave {
            id: id.to_string(),
            queued_at,
            target: SaveTarget::Memory {
                workspace: "work".into(),
                body: PutMemoryBody {
                    content: "fact".into(),
                    title: None,
                    kind: "project".into(),
                    scope: "workspace".into(),
                    tags: vec![],
                    importance: None,
                },
            },
            failure: None,
        }
    }

    fn preference(id: &str, queued_at: u64) -> PendingSave {
        PendingSave {
            id: id.to_string(),
            queued_at,
            target: SaveTarget::Preference {
                body: PutPreferenceBody {
                    content: "prefers oat milk".into(),
                    title: None,
                    kind: "user".into(),
                    tags: vec![],
                    importance: None,
                },
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

        assert_eq!(outbox.reassign("personal", None).unwrap(), (1, 0));
        assert!(outbox.dead_letters().unwrap().is_empty());
        let pending = outbox.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.target.label(), "personal \u{b7} workspace");
        assert!(pending[0].1.failure.is_none());

        assert_eq!(outbox.discard(Some("m_zzz")).unwrap(), 0);
        assert_eq!(outbox.discard(None).unwrap(), 1);
        assert!(outbox.pending().unwrap().is_empty());
    }

    #[test]
    fn preferences_round_trip_and_are_never_reassigned_to_a_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Outbox::at(dir.path().join("outbox")).unwrap();
        outbox.enqueue(&preference("m_aaa", 100)).unwrap();
        outbox.enqueue(&item("m_bbb", 200)).unwrap();

        let stored = outbox.pending().unwrap();
        assert!(matches!(stored[0].1.target, SaveTarget::Preference { .. }));
        assert_eq!(stored[0].1.target.label(), "everywhere");

        assert_eq!(outbox.reassign("personal", None).unwrap(), (1, 1));
        let after = outbox.pending().unwrap();
        assert_eq!(after.len(), 2);
        assert!(matches!(after[0].1.target, SaveTarget::Preference { .. }));
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
