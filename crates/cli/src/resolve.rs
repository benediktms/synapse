use std::path::{Path, PathBuf};
use std::process::Command;

use domain::{Scope, Workspace};

use crate::config::Config;

pub const SHARED: &str = "shared";

pub fn validate_workspace(name: &str) -> Result<String, String> {
    if name == SHARED {
        return Ok(SHARED.to_string());
    }
    Workspace::new(name)
        .map(|ws| ws.to_string())
        .map_err(|e| e.to_string())
}

pub fn resolve_workspace(
    config: &Config,
    flag: Option<&str>,
    cwd: &Path,
    fail_closed: bool,
) -> Result<String, String> {
    if let Some(name) = flag {
        return validate_workspace(name);
    }
    if let Some(name) = rule_match(config, cwd)? {
        return validate_workspace(&name);
    }
    if fail_closed && in_git_checkout(cwd) {
        return Err(format!(
            "no workspace rule matches {}; pass --workspace <ws> or add a path rule to {}",
            cwd.display(),
            crate::config::config_path().display()
        ));
    }
    match config.default_workspace.as_deref() {
        Some(name) => validate_workspace(name),
        None => Err(
            "no workspace configured; run `syn workspace use <name>` or pass --workspace".into(),
        ),
    }
}

fn rule_match(config: &Config, cwd: &Path) -> Result<Option<String>, String> {
    let cwd = canonical(cwd);
    let mut best: Option<(usize, String)> = None;
    let mut ambiguous: Vec<String> = Vec::new();
    for rule in &config.workspace_rules {
        let root = canonical(Path::new(&rule.path));
        if !starts_with_components(&cwd, &root) {
            continue;
        }
        let depth = root.components().count();
        match &best {
            Some((best_depth, _)) if *best_depth > depth => {}
            Some((best_depth, workspace)) if *best_depth == depth => {
                if workspace != &rule.workspace {
                    ambiguous = vec![workspace.clone(), rule.workspace.clone()];
                }
            }
            _ => {
                ambiguous.clear();
                best = Some((depth, rule.workspace.clone()));
            }
        }
    }
    if !ambiguous.is_empty() {
        ambiguous.sort();
        return Err(format!(
            "workspace rules for {} are ambiguous ({}); pass --workspace to disambiguate",
            cwd.display(),
            ambiguous.join(", ")
        ));
    }
    Ok(best.map(|(_, workspace)| workspace))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn starts_with_components(path: &Path, prefix: &Path) -> bool {
    let mut path = path.components();
    for component in prefix.components() {
        if path.next() != Some(component) {
            return false;
        }
    }
    true
}

pub fn in_git_checkout(cwd: &Path) -> bool {
    canonical(cwd)
        .ancestors()
        .any(|dir| dir.join(".git").exists())
}

pub struct ResolvedScope {
    pub scope: String,
    pub note: Option<String>,
}

impl ResolvedScope {
    pub fn project(&self) -> Option<&str> {
        (self.scope != "workspace").then_some(self.scope.as_str())
    }
}

pub fn resolve_scope(flag: Option<&str>, cwd: &Path) -> Result<ResolvedScope, String> {
    match flag {
        Some("workspace") => Ok(ResolvedScope {
            scope: "workspace".into(),
            note: None,
        }),
        Some(slug) if slug != "project" => {
            let scope = Scope::parse(slug).map_err(|e| e.to_string())?;
            Ok(ResolvedScope {
                scope: scope.as_str().to_string(),
                note: None,
            })
        }
        _ => Ok(
            match git_origin(cwd).as_deref().and_then(parse_origin_url) {
                Some(slug) => ResolvedScope {
                    scope: slug,
                    note: None,
                },
                None => ResolvedScope {
                    scope: "workspace".into(),
                    note: Some("no git origin here; using scope 'workspace'".into()),
                },
            },
        ),
    }
}

fn git_origin(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn parse_origin_url(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = match url.split_once("://") {
        Some(("", _)) => return None,
        Some((_, rest)) => rest,
        None => {
            let (authority, path) = url.split_once(':')?;
            if authority.is_empty() || authority.contains('/') {
                return None;
            }
            return slug(path);
        }
    };
    let (authority, path) = rest.split_once('/')?;
    if authority.is_empty() {
        return None;
    }
    slug(path)
}

fn slug(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.rsplit('/').filter(|part| !part.is_empty());
    let repo = parts.next()?;
    let owner = parts.next()?;
    let slug = format!("{owner}/{repo}");
    Scope::parse(&slug)
        .ok()
        .map(|scope| scope.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceRule;
    use std::fs;

    fn config_with(rules: &[(&str, &str)], default: Option<&str>) -> Config {
        Config {
            default_workspace: default.map(str::to_string),
            workspace_rules: rules
                .iter()
                .map(|(path, workspace)| WorkspaceRule {
                    path: (*path).to_string(),
                    workspace: (*workspace).to_string(),
                })
                .collect(),
            ..Config::default()
        }
    }

    #[test]
    fn parses_ssh_and_https_origins() {
        for url in [
            "git@github.com:fresha/offers.git",
            "git@github.com:fresha/offers",
            "ssh://git@github.com/fresha/offers.git",
            "ssh://git@github.com:22/fresha/offers.git",
            "https://github.com/fresha/offers.git",
            "https://github.com/fresha/offers",
            "https://user:pass@github.com/fresha/offers.git",
            "https://github.com/fresha/offers/",
        ] {
            assert_eq!(
                parse_origin_url(url).as_deref(),
                Some("fresha/offers"),
                "failed on {url}"
            );
        }
    }

    #[test]
    fn nested_groups_use_the_last_two_segments() {
        assert_eq!(
            parse_origin_url("https://gitlab.com/group/sub/repo.git").as_deref(),
            Some("sub/repo")
        );
    }

    #[test]
    fn rejects_origins_without_an_owner_repo_pair() {
        for url in [
            "",
            "/local/path/repo.git",
            "file:///local/path/repo.git",
            "https://github.com/fresha",
            "https://github.com/",
            "::",
        ] {
            assert_eq!(parse_origin_url(url), None, "accepted {url}");
        }
    }

    #[test]
    fn longest_matching_path_rule_wins() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("clients/acme");
        fs::create_dir_all(&nested).unwrap();
        let config = config_with(
            &[
                (root.path().to_str().unwrap(), "work"),
                (nested.to_str().unwrap(), "acme"),
            ],
            Some("personal"),
        );
        assert_eq!(
            resolve_workspace(&config, None, &nested, true).unwrap(),
            "acme"
        );
        assert_eq!(
            resolve_workspace(&config, None, root.path(), true).unwrap(),
            "work"
        );
    }

    #[test]
    fn sibling_prefixes_do_not_match_on_a_partial_component() {
        let root = tempfile::tempdir().unwrap();
        let rule_dir = root.path().join("work");
        let cwd = root.path().join("workshop");
        fs::create_dir_all(&rule_dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let config = config_with(&[(rule_dir.to_str().unwrap(), "work")], Some("personal"));
        assert_eq!(
            resolve_workspace(&config, None, &cwd, false).unwrap(),
            "personal"
        );
    }

    #[test]
    fn symlinked_rule_paths_resolve_to_the_same_workspace() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let project = real.join("project");
        fs::create_dir_all(&project).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let config = config_with(&[(link.to_str().unwrap(), "work")], None);
        assert_eq!(
            resolve_workspace(&config, None, &project, true).unwrap(),
            "work"
        );
    }

    #[test]
    fn equal_depth_rules_naming_different_workspaces_are_ambiguous() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let config = config_with(
            &[
                (real.to_str().unwrap(), "work"),
                (link.to_str().unwrap(), "personal"),
            ],
            None,
        );
        let err = resolve_workspace(&config, None, &real, true).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert_eq!(
            resolve_workspace(&config, Some("work"), &real, true).unwrap(),
            "work"
        );
    }

    #[test]
    fn saves_fail_closed_inside_an_unmapped_git_checkout() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let config = config_with(&[], Some("personal"));

        let err = resolve_workspace(&config, None, &repo, true).unwrap_err();
        assert!(err.contains("no workspace rule matches"), "{err}");

        assert_eq!(
            resolve_workspace(&config, None, &repo, false).unwrap(),
            "personal"
        );
        assert_eq!(
            resolve_workspace(&config, Some("shared"), &repo, true).unwrap(),
            "shared"
        );
        assert_eq!(
            resolve_workspace(&config, None, root.path(), true).unwrap(),
            "personal"
        );
    }

    #[test]
    fn git_worktree_files_count_as_a_checkout() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            "gitdir: /elsewhere/.git/worktrees/wt",
        )
        .unwrap();
        assert!(in_git_checkout(&worktree));
        assert!(!in_git_checkout(root.path()));
    }

    #[test]
    fn explicit_scope_flags_bypass_git_inference() {
        let cwd = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_scope(Some("workspace"), cwd.path()).unwrap().scope,
            "workspace"
        );
        let explicit = resolve_scope(Some("fresha/offers"), cwd.path()).unwrap();
        assert_eq!(explicit.scope, "fresha/offers");
        assert!(explicit.note.is_none());
        assert!(resolve_scope(Some("has space"), cwd.path()).is_err());
    }

    #[test]
    fn scope_falls_back_to_workspace_with_a_note_outside_a_repo() {
        let cwd = tempfile::tempdir().unwrap();
        let resolved = resolve_scope(None, cwd.path()).unwrap();
        assert_eq!(resolved.scope, "workspace");
        assert!(resolved.note.is_some());
        assert_eq!(resolved.project(), None);
    }
}
