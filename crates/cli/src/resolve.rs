use std::path::{Path, PathBuf};

use domain::{Scope, Workspace};

use crate::config::Config;
use crate::git::GitFacts;

pub fn validate_workspace(name: &str) -> Result<String, String> {
    if name == "shared" {
        return Err(
            "\"shared\" is not a workspace; use `syn remember` to save a memory that applies \
             everywhere, or --preference to act on one"
                .into(),
        );
    }
    Workspace::new(name)
        .map(|ws| ws.to_string())
        .map_err(|e| e.to_string())
}

pub fn resolve_workspace(
    config: &Config,
    flag: Option<&str>,
    cwd: &Path,
    facts: Option<&GitFacts>,
    fail_closed: bool,
) -> Result<String, String> {
    if let Some(name) = flag {
        return validate_workspace(name);
    }
    if let Some(name) = rule_match(config, cwd)? {
        return validate_workspace(&name);
    }
    if let Some(facts) = facts
        && let Some(name) = rule_match(config, &facts.anchor)?
    {
        return validate_workspace(&name);
    }
    if let Some(name) = org_rule_match(config, facts.and_then(|facts| facts.owner.as_deref()))? {
        return validate_workspace(&name);
    }
    if fail_closed && facts.is_some() {
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

pub fn validate_org(org: &str) -> Result<String, String> {
    Scope::validate_owner(org)
        .map(|()| org.to_string())
        .map_err(|e| e.to_string())
}

/// Org-rule tier of the ladder: only consulted once no path rule (cwd or anchor) matched.
fn org_rule_match(config: &Config, owner: Option<&str>) -> Result<Option<String>, String> {
    let Some(owner) = owner else {
        return Ok(None);
    };
    let mut matched: Option<String> = None;
    let mut ambiguous: Vec<String> = Vec::new();
    for rule in &config.org_rules {
        if rule.org != owner {
            continue;
        }
        match &matched {
            Some(workspace) if workspace == &rule.workspace => {}
            Some(workspace) => {
                ambiguous = vec![workspace.clone(), rule.workspace.clone()];
            }
            None => matched = Some(rule.workspace.clone()),
        }
    }
    if !ambiguous.is_empty() {
        ambiguous.sort();
        return Err(format!(
            "org rules for {owner} are ambiguous ({}); pass --workspace to disambiguate",
            ambiguous.join(", ")
        ));
    }
    Ok(matched)
}

fn rule_match(config: &Config, path: &Path) -> Result<Option<String>, String> {
    let path = canonical(path);
    let mut best: Option<(usize, String)> = None;
    let mut ambiguous: Vec<String> = Vec::new();
    for rule in &config.workspace_rules {
        let root = canonical(Path::new(&rule.path));
        if !starts_with_components(&path, &root) {
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
            path.display(),
            ambiguous.join(", ")
        ));
    }
    Ok(best.map(|(_, workspace)| workspace))
}

pub(crate) fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn starts_with_components(path: &Path, prefix: &Path) -> bool {
    let mut path = path.components();
    for component in prefix.components() {
        if path.next() != Some(component) {
            return false;
        }
    }
    true
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

/// `Ok(Some(_))` for a flag that fully determines scope without touching the facts probe;
/// `Ok(None)` for `None` or the `project` sentinel, meaning "infer from git".
pub fn explicit_scope(flag: Option<&str>) -> Result<Option<ResolvedScope>, String> {
    match flag {
        Some("workspace") => Ok(Some(ResolvedScope {
            scope: "workspace".into(),
            note: None,
        })),
        Some(slug) if slug != "project" => {
            let scope = Scope::parse(slug).map_err(|e| e.to_string())?;
            Ok(Some(ResolvedScope {
                scope: scope.as_str().to_string(),
                note: None,
            }))
        }
        _ => Ok(None),
    }
}

pub fn scope_from_facts(facts: Option<&GitFacts>) -> ResolvedScope {
    match facts.and_then(|facts| facts.slug.clone()) {
        Some(slug) => ResolvedScope {
            scope: slug,
            note: None,
        },
        None => ResolvedScope {
            scope: "workspace".into(),
            note: Some("no git origin here; using scope 'workspace'".into()),
        },
    }
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
    use crate::config::{OrgRule, WorkspaceRule};
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

    fn facts_at(path: &Path) -> GitFacts {
        GitFacts {
            anchor: canonical(path),
            toplevel: canonical(path),
            slug: None,
            owner: None,
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
            resolve_workspace(&config, None, &nested, None, true).unwrap(),
            "acme"
        );
        assert_eq!(
            resolve_workspace(&config, None, root.path(), None, true).unwrap(),
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
            resolve_workspace(&config, None, &cwd, None, false).unwrap(),
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
            resolve_workspace(&config, None, &project, None, true).unwrap(),
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
        let err = resolve_workspace(&config, None, &real, None, true).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert_eq!(
            resolve_workspace(&config, Some("work"), &real, None, true).unwrap(),
            "work"
        );
    }

    #[test]
    fn saves_fail_closed_inside_an_unmapped_git_checkout() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let config = config_with(&[], Some("personal"));
        let facts = facts_at(&repo);

        let err = resolve_workspace(&config, None, &repo, Some(&facts), true).unwrap_err();
        assert!(err.contains("no workspace rule matches"), "{err}");

        assert_eq!(
            resolve_workspace(&config, None, &repo, Some(&facts), false).unwrap(),
            "personal"
        );
        // Outside the checkout there are no facts, so fail-closed never fires.
        assert_eq!(
            resolve_workspace(&config, None, root.path(), None, true).unwrap(),
            "personal"
        );
    }

    #[test]
    fn with_no_worktree_rule_routing_follows_the_anchor_rule() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let worktree = root.path().join("wt");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        let config = config_with(&[(main.to_str().unwrap(), "work")], Some("personal"));
        let facts = GitFacts {
            anchor: canonical(&main),
            ..facts_at(&worktree)
        };
        assert_eq!(
            resolve_workspace(&config, None, &worktree, Some(&facts), true).unwrap(),
            "work"
        );
    }

    #[test]
    fn an_explicit_worktree_rule_beats_the_inherited_anchor_rule() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let worktree = root.path().join("wt");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        let config = config_with(
            &[
                (main.to_str().unwrap(), "work"),
                (worktree.to_str().unwrap(), "wt-only"),
            ],
            Some("personal"),
        );
        let facts = GitFacts {
            anchor: canonical(&main),
            ..facts_at(&worktree)
        };
        assert_eq!(
            resolve_workspace(&config, None, &worktree, Some(&facts), true).unwrap(),
            "wt-only"
        );
    }

    #[test]
    fn org_rule_routes_a_repo_that_no_path_rule_matched() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let mut config = config_with(&[], Some("personal"));
        config.org_rules.push(OrgRule {
            org: "freshaengineering".into(),
            workspace: "work".into(),
        });
        let facts = GitFacts {
            slug: Some("freshaengineering/widgets".into()),
            owner: Some("freshaengineering".into()),
            ..facts_at(&repo)
        };
        assert_eq!(
            resolve_workspace(&config, None, &repo, Some(&facts), true).unwrap(),
            "work"
        );
    }

    #[test]
    fn org_rule_routes_a_worktree_of_the_repo_too() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let worktree = root.path().join("wt");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        let mut config = config_with(&[], Some("personal"));
        config.org_rules.push(OrgRule {
            org: "freshaengineering".into(),
            workspace: "work".into(),
        });
        let facts = GitFacts {
            anchor: canonical(&main),
            slug: Some("freshaengineering/widgets".into()),
            owner: Some("freshaengineering".into()),
            ..facts_at(&worktree)
        };
        assert_eq!(
            resolve_workspace(&config, None, &worktree, Some(&facts), true).unwrap(),
            "work"
        );
    }

    #[test]
    fn a_nested_path_rule_still_beats_an_org_rule_for_the_same_repo() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("clients/acme");
        fs::create_dir_all(&nested).unwrap();
        let mut config = config_with(
            &[(nested.to_str().unwrap(), "client-a-ws")],
            Some("personal"),
        );
        config.org_rules.push(OrgRule {
            org: "freshaengineering".into(),
            workspace: "work".into(),
        });
        let facts = GitFacts {
            slug: Some("freshaengineering/widgets".into()),
            owner: Some("freshaengineering".into()),
            ..facts_at(&nested)
        };
        assert_eq!(
            resolve_workspace(&config, None, &nested, Some(&facts), true).unwrap(),
            "client-a-ws"
        );
    }

    #[test]
    fn conflicting_org_rules_naming_different_workspaces_are_ambiguous() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let mut config = config_with(&[], None);
        config.org_rules.push(OrgRule {
            org: "freshaengineering".into(),
            workspace: "work".into(),
        });
        config.org_rules.push(OrgRule {
            org: "freshaengineering".into(),
            workspace: "personal".into(),
        });
        let facts = GitFacts {
            slug: Some("freshaengineering/widgets".into()),
            owner: Some("freshaengineering".into()),
            ..facts_at(&repo)
        };
        let err = resolve_workspace(&config, None, &repo, Some(&facts), true).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert_eq!(
            resolve_workspace(&config, Some("work"), &repo, Some(&facts), true).unwrap(),
            "work"
        );
    }

    #[test]
    fn identical_duplicate_org_rules_coalesce_silently() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let mut config = config_with(&[], None);
        config.org_rules.push(OrgRule {
            org: "freshaengineering".into(),
            workspace: "work".into(),
        });
        config.org_rules.push(OrgRule {
            org: "freshaengineering".into(),
            workspace: "work".into(),
        });
        let facts = GitFacts {
            slug: Some("freshaengineering/widgets".into()),
            owner: Some("freshaengineering".into()),
            ..facts_at(&repo)
        };
        assert_eq!(
            resolve_workspace(&config, None, &repo, Some(&facts), true).unwrap(),
            "work"
        );
    }

    #[test]
    fn an_origin_less_repo_falls_through_org_rules_without_crashing() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let mut config = config_with(&[], Some("personal"));
        config.org_rules.push(OrgRule {
            org: "freshaengineering".into(),
            workspace: "work".into(),
        });
        let facts = facts_at(&repo);
        assert_eq!(facts.owner, None);
        assert_eq!(
            resolve_workspace(&config, None, &repo, Some(&facts), false).unwrap(),
            "personal"
        );
        let err = resolve_workspace(&config, None, &repo, Some(&facts), true).unwrap_err();
        assert!(err.contains("no workspace rule matches"), "{err}");
    }

    #[test]
    fn validate_org_shares_the_owner_grammar() {
        assert_eq!(
            validate_org("freshaengineering").unwrap(),
            "freshaengineering"
        );
        assert!(validate_org("has space").is_err());
    }

    #[test]
    fn shared_is_not_addressable_as_a_workspace() {
        let err = validate_workspace("shared").unwrap_err();
        assert!(err.contains("syn remember"), "{err}");
        assert!(err.contains("--preference"), "{err}");

        let config = config_with(&[], Some("work"));
        assert!(resolve_workspace(&config, Some("shared"), Path::new("/"), None, false).is_err());
    }

    #[test]
    fn explicit_scope_flags_bypass_git_inference() {
        assert_eq!(
            explicit_scope(Some("workspace")).unwrap().unwrap().scope,
            "workspace"
        );
        let explicit = explicit_scope(Some("fresha/offers")).unwrap().unwrap();
        assert_eq!(explicit.scope, "fresha/offers");
        assert!(explicit.note.is_none());
        assert!(explicit_scope(Some("has space")).is_err());
        assert!(explicit_scope(None).unwrap().is_none());
        assert!(explicit_scope(Some("project")).unwrap().is_none());
    }

    #[test]
    fn scope_falls_back_to_workspace_with_a_note_outside_a_repo() {
        let resolved = scope_from_facts(None);
        assert_eq!(resolved.scope, "workspace");
        assert!(resolved.note.is_some());
        assert_eq!(resolved.project(), None);
    }

    #[test]
    fn scope_takes_the_slug_from_facts_without_shelling_out() {
        let root = tempfile::tempdir().unwrap();
        let facts = GitFacts {
            slug: Some("fresha/offers".into()),
            owner: Some("fresha".into()),
            ..facts_at(root.path())
        };
        let resolved = scope_from_facts(Some(&facts));
        assert_eq!(resolved.scope, "fresha/offers");
        assert!(resolved.note.is_none());
    }
}
