use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::resolve::{canonical, parse_origin_url, starts_with_components};

const DEADLINE: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(5);
const MAX_SUPERPROJECT_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    /// Routing anchor: the outermost working tree cwd belongs to.
    pub anchor: PathBuf,
    /// This working tree's own root.
    pub toplevel: PathBuf,
    /// `owner/repo` of the innermost repo, from its origin.
    pub slug: Option<String>,
    /// Owner half of `slug`, for org rules.
    pub owner: Option<String>,
}

impl GitFacts {
    /// `Ok(None)` when cwd is not in a working tree; `Err` when git could not be
    /// asked at all, or answered something unparseable.
    pub fn discover(cwd: &Path) -> Result<Option<Self>, String> {
        let deadline = Instant::now() + DEADLINE;
        let Some(inner) = structural(cwd, deadline)? else {
            return Ok(None);
        };
        let toplevel = inner.toplevel.clone();
        let anchor = outermost_anchor(inner, deadline)?;
        let slug = origin_slug(&toplevel, deadline)?;
        let owner = slug
            .as_deref()
            .and_then(|slug| slug.split('/').next())
            .map(str::to_string);
        Ok(Some(Self {
            anchor,
            toplevel,
            slug,
            owner,
        }))
    }
}

#[derive(Clone)]
struct Structural {
    git_dir: PathBuf,
    common_dir: PathBuf,
    toplevel: PathBuf,
    superproject: Option<PathBuf>,
}

// `--show-superproject-working-tree` prints nothing outside a submodule, and a bare
// repo prints the two dirs then fails `--show-toplevel`, so the count is the signal.
fn structural(dir: &Path, deadline: Instant) -> Result<Option<Structural>, String> {
    let output = run_git(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
            "--show-toplevel",
            "--show-superproject-working-tree",
        ],
        deadline,
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    if !output.status.success() {
        return match lines.len() {
            0 => Ok(None),
            2 => Ok(None),
            _ => Err(format!(
                "git rev-parse failed in {}: {}",
                dir.display(),
                stderr_summary(&output)
            )),
        };
    }
    match lines[..] {
        [git_dir, common_dir, toplevel] => Ok(Some(Structural {
            git_dir: PathBuf::from(git_dir),
            common_dir: PathBuf::from(common_dir),
            toplevel: canonical(Path::new(toplevel)),
            superproject: None,
        })),
        [git_dir, common_dir, toplevel, superproject] => Ok(Some(Structural {
            git_dir: PathBuf::from(git_dir),
            common_dir: PathBuf::from(common_dir),
            toplevel: canonical(Path::new(toplevel)),
            superproject: Some(canonical(Path::new(superproject))),
        })),
        _ => Err(format!(
            "git rev-parse printed {} lines for {}; expected 3 or 4",
            lines.len(),
            dir.display()
        )),
    }
}

fn outermost_anchor(mut tree: Structural, deadline: Instant) -> Result<PathBuf, String> {
    let mut visited = HashSet::from([tree.toplevel.clone()]);
    for _ in 0..MAX_SUPERPROJECT_DEPTH {
        let Some(superproject) = tree.superproject.clone() else {
            return primary_worktree(&tree, deadline);
        };
        if superproject == tree.toplevel
            || !starts_with_components(&tree.toplevel, &superproject)
            || !visited.insert(superproject.clone())
        {
            return Err(format!(
                "git reports {} as the superproject of {}, which is not a move outwards",
                superproject.display(),
                tree.toplevel.display()
            ));
        }
        tree = structural(&superproject, deadline)?.ok_or_else(|| {
            format!(
                "superproject {} is not a working tree",
                superproject.display()
            )
        })?;
    }
    Err(format!(
        "submodule nesting above {} is deeper than {MAX_SUPERPROJECT_DEPTH} levels",
        tree.toplevel.display()
    ))
}

fn primary_worktree(tree: &Structural, deadline: Instant) -> Result<PathBuf, String> {
    if canonical(&tree.git_dir) == canonical(&tree.common_dir) {
        return Ok(tree.toplevel.clone());
    }
    let output = run_git(
        &tree.toplevel,
        &["worktree", "list", "--porcelain"],
        deadline,
    )?;
    if !output.status.success() {
        return Err(format!(
            "git worktree list failed in {}: {}",
            tree.toplevel.display(),
            stderr_summary(&output)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut primary = stdout.lines().take_while(|line| !line.is_empty());
    let path = primary
        .next()
        .and_then(|line| line.strip_prefix("worktree "))
        .ok_or_else(|| {
            format!(
                "git worktree list named no primary worktree for {}",
                tree.toplevel.display()
            )
        })?;
    if primary.any(|line| line == "bare") {
        return Ok(tree.toplevel.clone());
    }
    Ok(canonical(Path::new(path)))
}

fn origin_slug(dir: &Path, deadline: Instant) -> Result<Option<String>, String> {
    let output = run_git(dir, &["config", "--get", "remote.origin.url"], deadline)?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(parse_origin_url(&String::from_utf8_lossy(&output.stdout)))
}

fn run_git(dir: &Path, args: &[&str], deadline: Instant) -> Result<Output, String> {
    let child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run git: {e}"))?;
    output_by(child, deadline)
}

fn output_by(mut child: Child, deadline: Instant) -> Result<Output, String> {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|e| format!("could not read git output: {e}"));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "git did not answer within the {}s probe deadline",
                    DEADLINE.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => return Err(format!("could not wait for git: {e}")),
        }
    }
}

fn stderr_summary(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no diagnostic")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "user.name=probe",
                "-c",
                "user.email=probe@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "protocol.file.allow=always",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repo(root: &Path, name: &str, origin: &str) -> PathBuf {
        let path = root.join(name);
        fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q"]);
        git(&path, &["remote", "add", "origin", origin]);
        git(&path, &["commit", "-q", "--allow-empty", "-m", "init"]);
        path
    }

    fn probe(cwd: &Path) -> GitFacts {
        GitFacts::discover(cwd)
            .expect("probe")
            .expect("facts in a working tree")
    }

    #[test]
    fn an_ordinary_checkout_anchors_on_its_own_toplevel() {
        let root = TempDir::new().unwrap();
        let path = repo(root.path(), "plain", "git@github.com:acme/plain.git");
        let deep = path.join("sub/deep");
        fs::create_dir_all(&deep).unwrap();

        for cwd in [&path, &deep] {
            let facts = probe(cwd);
            assert_eq!(facts.anchor, canonical(&path));
            assert_eq!(facts.toplevel, canonical(&path));
            assert_eq!(facts.slug.as_deref(), Some("acme/plain"));
            assert_eq!(facts.owner.as_deref(), Some("acme"));
        }
    }

    #[test]
    fn a_linked_worktree_anchors_on_the_main_checkout() {
        let root = TempDir::new().unwrap();
        let main = repo(root.path(), "main", "git@github.com:acme/main.git");
        let linked = root.path().join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                linked.to_str().unwrap(),
                "-b",
                "wt",
            ],
        );

        let facts = probe(&linked);
        assert_eq!(facts.anchor, canonical(&main));
        assert_eq!(facts.toplevel, canonical(&linked));
        assert_eq!(facts.slug.as_deref(), Some("acme/main"));
    }

    #[test]
    fn a_separate_git_dir_anchors_on_the_working_tree_not_the_metadata_parent() {
        let root = TempDir::new().unwrap();
        let meta = root.path().join("meta");
        fs::create_dir_all(&meta).unwrap();
        let work = root.path().join("work");
        git(
            root.path(),
            &[
                "init",
                "-q",
                &format!("--separate-git-dir={}", meta.join("repo.git").display()),
                work.to_str().unwrap(),
            ],
        );

        let facts = probe(&work);
        assert_eq!(facts.anchor, canonical(&work));
        assert_ne!(facts.anchor, canonical(&meta));
    }

    #[test]
    fn a_bare_primary_falls_back_to_the_linked_worktrees_own_toplevel() {
        let root = TempDir::new().unwrap();
        let source = repo(root.path(), "source", "git@github.com:acme/source.git");
        let store = root.path().join("store.git");
        git(
            root.path(),
            &["init", "-q", "--bare", store.to_str().unwrap()],
        );
        git(&source, &["push", "-q", store.to_str().unwrap(), "main"]);
        let linked = root.path().join("linked");
        git(
            &store,
            &["worktree", "add", "-q", linked.to_str().unwrap(), "main"],
        );

        let facts = probe(&linked);
        assert_eq!(facts.anchor, canonical(&linked));
        assert_eq!(facts.toplevel, canonical(&linked));
    }

    #[test]
    fn a_bare_repo_and_a_plain_directory_yield_no_facts() {
        let root = TempDir::new().unwrap();
        let store = root.path().join("store.git");
        git(
            root.path(),
            &["init", "-q", "--bare", store.to_str().unwrap()],
        );

        assert_eq!(GitFacts::discover(&store).unwrap(), None);
        assert_eq!(GitFacts::discover(&store.join("refs")).unwrap(), None);
        assert_eq!(GitFacts::discover(root.path()).unwrap(), None);
    }

    #[test]
    fn a_nested_submodule_anchors_outermost_and_scopes_innermost() {
        let root = TempDir::new().unwrap();
        let inner = repo(root.path(), "inner", "git@github.com:acme/inner.git");
        let mid = repo(root.path(), "mid", "git@github.com:acme/mid.git");
        let outer = repo(root.path(), "outer", "git@github.com:acme/outer.git");
        git(
            &mid,
            &[
                "submodule",
                "add",
                "-q",
                inner.to_str().unwrap(),
                "vend/inner",
            ],
        );
        git(&mid, &["commit", "-q", "-m", "vendor inner"]);
        git(
            &outer,
            &["submodule", "add", "-q", mid.to_str().unwrap(), "vend/mid"],
        );
        git(&outer, &["commit", "-q", "-m", "vendor mid"]);
        git(
            &outer,
            &["submodule", "update", "--init", "--recursive", "-q"],
        );

        let one_level = outer.join("vend/mid");
        let two_levels = one_level.join("vend/inner");
        for (tree, slug) in [(&one_level, "mid"), (&two_levels, "inner")] {
            git(
                tree,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    &format!("git@github.com:acme/{slug}.git"),
                ],
            );
        }

        let facts = probe(&one_level);
        assert_eq!(facts.anchor, canonical(&outer));
        assert_eq!(facts.toplevel, canonical(&one_level));
        assert_eq!(facts.slug.as_deref(), Some("acme/mid"));

        let facts = probe(&two_levels);
        assert_eq!(facts.anchor, canonical(&outer));
        assert_eq!(facts.toplevel, canonical(&two_levels));
        assert_eq!(facts.slug.as_deref(), Some("acme/inner"));
        assert_eq!(facts.owner.as_deref(), Some("acme"));
    }

    #[test]
    fn a_repo_without_a_usable_origin_has_no_slug() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("orphan");
        fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q"]);

        let facts = probe(&path);
        assert_eq!(facts.anchor, canonical(&path));
        assert_eq!(facts.slug, None);
        assert_eq!(facts.owner, None);

        git(&path, &["remote", "add", "origin", "/somewhere/local.git"]);
        assert_eq!(probe(&path).slug, None);
    }
}
