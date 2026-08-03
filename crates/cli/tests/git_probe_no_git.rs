//! Own test binary: it empties `git` off `PATH` for the whole process.

use std::process::Command;

use cli::git::GitFacts;

#[test]
fn a_missing_git_is_a_hard_error_not_an_absence_of_facts() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );

    unsafe { std::env::set_var("PATH", root.path().join("empty")) };

    for cwd in [&repo, &root.path().to_path_buf()] {
        let error = GitFacts::discover(cwd).unwrap_err();
        assert!(error.contains("could not run git"), "{error}");
    }
}
