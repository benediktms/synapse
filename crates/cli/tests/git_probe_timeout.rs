//! Own test binary: it replaces `git` on `PATH` for the whole process.

#![cfg(unix)]

use std::process::Command;
use std::time::{Duration, Instant};

use cli::git::GitFacts;

const SENTINEL: &str = "31337";

#[test]
fn a_hung_git_fails_the_probe_within_the_deadline_and_leaves_no_child() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("git");
    std::fs::write(&fake, format!("#!/bin/sh\nexec sleep {SENTINEL}\n")).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    unsafe { std::env::set_var("PATH", path) };

    let started = Instant::now();
    let error = GitFacts::discover(dir.path()).unwrap_err();
    let elapsed = started.elapsed();

    assert!(error.contains("deadline"), "{error}");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    assert!(!sleeper_running(), "the killed git was not reaped");
}

fn sleeper_running() -> bool {
    Command::new("pgrep")
        .args(["-f", &format!("sleep {SENTINEL}")])
        .output()
        .is_ok_and(|output| output.status.success())
}
