//! Own test binary: it sets repository-selecting variables for the whole process.

use std::path::Path;
use std::process::Command;

use cli::git::GitFacts;

#[test]
fn git_dir_and_work_tree_in_the_environment_do_not_move_the_probe() {
    let root = tempfile::tempdir().unwrap();
    let here = init(root.path(), "here");
    let elsewhere = init(root.path(), "elsewhere");

    unsafe {
        std::env::set_var("GIT_DIR", elsewhere.join(".git"));
        std::env::set_var("GIT_WORK_TREE", &elsewhere);
    }

    let facts = GitFacts::discover(&here).unwrap().expect("facts");
    assert_eq!(facts.toplevel, here.canonicalize().unwrap());
    assert_eq!(facts.anchor, here.canonicalize().unwrap());
}

fn init(root: &Path, name: &str) -> std::path::PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(&path).unwrap();
    let output = Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .output()
        .expect("git init");
    assert!(output.status.success());
    path
}
