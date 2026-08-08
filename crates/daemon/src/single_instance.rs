use std::fs::File;
use std::path::Path;

/// Holds the flock; dropping releases it, which is what signals the daemon is no longer running.
pub struct DaemonLock {
    _file: File,
}

pub fn acquire(dir: &Path) -> Result<DaemonLock, std::io::Error> {
    let path = dir.join("daemon.lock");
    let file = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)?;
    match file.try_lock() {
        Ok(_) => Ok(DaemonLock { _file: file }),
        Err(_) => Err(std::io::Error::other("daemon already running")),
    }
}
