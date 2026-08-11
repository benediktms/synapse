//! Own test binary: it replaces `git` on `PATH` for the whole process.

#![cfg(unix)]

use cli::git::GitFacts;

#[test]
fn an_unexpected_line_count_is_an_error_never_a_guess() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("git");
    std::fs::write(&fake, "#!/bin/sh\nseq 1 \"$FAKE_GIT_LINES\"\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    unsafe { std::env::set_var("PATH", path) };

    for lines in ["2", "5"] {
        unsafe { std::env::set_var("FAKE_GIT_LINES", lines) };
        let error = GitFacts::discover(dir.path()).unwrap_err();
        assert!(error.contains(&format!("printed {lines} lines")), "{error}");
    }
}
