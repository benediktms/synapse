use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const SKILL_BODY: &str = include_str!("../../assets/skills/synapse/SKILL.md");
const HOOK_BODY: &str = include_str!("../../assets/hooks/session-start.sh");
const OMP_HOOK_BODY: &str = include_str!("../../assets/hooks/omp-session-start.ts");
const MARKER: &str = "synapse:managed 1";

/// Codex has no match-everything wildcard, so the events are named outright.
const CODEX_MATCHER: &str = "startup|resume|clear|compact";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Harness {
    Claude,
    Copilot,
    Codex,
    Omp,
}

impl Harness {
    pub fn all() -> &'static [Harness] {
        &[
            Harness::Claude,
            Harness::Copilot,
            Harness::Codex,
            Harness::Omp,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Harness::Claude => "claude code",
            Harness::Copilot => "copilot cli",
            Harness::Codex => "codex cli",
            Harness::Omp => "oh my pi",
        }
    }
}

pub struct InstallOptions {
    pub dry_run: bool,
    pub force: bool,
}

pub struct Homes {
    claude: PathBuf,
    copilot: PathBuf,
    codex: PathBuf,
    omp: PathBuf,
}

impl Homes {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            claude: harness_home("CLAUDE_CONFIG_DIR", ".claude")?,
            copilot: harness_home("COPILOT_HOME", ".copilot")?,
            codex: harness_home("CODEX_HOME", ".codex")?,
            omp: omp_home()?,
        })
    }

    fn of(&self, harness: Harness) -> &Path {
        match harness {
            Harness::Claude => &self.claude,
            Harness::Copilot => &self.copilot,
            Harness::Codex => &self.codex,
            Harness::Omp => &self.omp,
        }
    }
}

fn harness_home(env: &str, fallback: impl AsRef<Path>) -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os(env) {
        return Ok(PathBuf::from(explicit));
    }
    under_home(fallback)
}

fn under_home(path: impl AsRef<Path>) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(path))
}

/// The env vars omp reads to place its agent directory.
struct OmpEnv {
    profile: Option<String>,
    legacy_profile: Option<String>,
    config_dir: Option<String>,
}

impl OmpEnv {
    fn from_env() -> Self {
        Self {
            profile: std::env::var("OMP_PROFILE").ok(),
            legacy_profile: std::env::var("PI_PROFILE").ok(),
            config_dir: std::env::var("PI_CONFIG_DIR").ok(),
        }
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(self.config_dir.as_deref().unwrap_or(".omp"))
    }

    /// A profile omp treats as named. `OMP_PROFILE` shadows `PI_PROFILE` even
    /// when it is empty, and an empty or `default` name selects no profile.
    fn named_profile(&self) -> Option<&str> {
        self.profile
            .as_deref()
            .or(self.legacy_profile.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty() && *name != "default")
    }

    /// The agent directory below `$HOME`, or `None` for the default profile,
    /// which is the only one where `PI_CODING_AGENT_DIR` still applies.
    fn profile_agent_dir(&self) -> Option<PathBuf> {
        let named = self.named_profile()?;
        Some(self.root().join("profiles").join(named).join("agent"))
    }
}

fn omp_home() -> Result<PathBuf, String> {
    let env = OmpEnv::from_env();
    match env.profile_agent_dir() {
        Some(dir) => under_home(dir),
        None => harness_home("PI_CODING_AGENT_DIR", env.root().join("agent")),
    }
}

pub fn run(harnesses: &[Harness], options: &InstallOptions) -> Result<Report, String> {
    install(harnesses, &Homes::from_env()?, options)
}

pub fn install(
    harnesses: &[Harness],
    homes: &Homes,
    options: &InstallOptions,
) -> Result<Report, String> {
    let mut groups = Vec::new();
    for &harness in harnesses {
        let mut actions = Vec::new();
        for target in targets(harness, homes.of(harness))? {
            actions.push(target.apply(options)?);
        }
        groups.push((harness, actions));
    }
    Ok(Report {
        groups,
        dry_run: options.dry_run,
        syn_on_path: syn_on_path(),
    })
}

fn targets(harness: Harness, home: &Path) -> Result<Vec<Target>, String> {
    let skill = Target::Owned(Owned {
        path: home.join("skills/synapse/SKILL.md"),
        body: SKILL_BODY.to_string(),
        style: Comment::Html,
        executable: false,
    });
    if harness == Harness::Omp {
        // omp auto-discovers `.ts` extension modules from the agent extensions dir.
        // It does read `~/.claude`, but only `hooks/pre|post` there — never
        // `settings.json` — so the Claude registration cannot serve it. This `.ts`
        // module is the target itself; there is no JSON hook config to merge.
        let hook = Target::Owned(Owned {
            path: home.join("extensions/synapse.ts"),
            body: OMP_HOOK_BODY.to_string(),
            style: Comment::Slash,
            executable: false,
        });
        return Ok(vec![skill, hook]);
    }
    let (script, hook) = match harness {
        Harness::Claude => {
            let script = home.join("hooks/synapse-session-start.sh");
            let hook = matched_hook(home.join("settings.json"), "*", &script)?;
            (script, hook)
        }
        Harness::Codex => {
            let script = home.join("hooks/synapse-session-start.sh");
            let hook = matched_hook(home.join("hooks.json"), CODEX_MATCHER, &script)?;
            (script, hook)
        }
        Harness::Copilot => {
            let script = home.join("synapse-session-start.sh");
            let hook = copilot_hook(home.join("hooks/synapse.json"), &script)?;
            (script, hook)
        }
        Harness::Omp => unreachable!("handled above"),
    };
    Ok(vec![skill, hook_script(script), hook])
}

/// Claude Code and Codex CLI share a hook shape: an event holds matcher groups,
/// each group holding the handlers that run.
fn matched_hook(path: PathBuf, matcher: &'static str, script: &Path) -> Result<Target, String> {
    let command = quote(script)?;
    Ok(Target::JsonHook(JsonHook {
        path,
        keys: vec!["hooks".into(), "SessionStart".into()],
        defaults: Vec::new(),
        handler: json!({ "type": "command", "command": command.clone(), "timeout": 10 }),
        command_key: "command",
        command,
        matcher: Some(matcher),
        label: "SessionStart hook",
    }))
}

fn copilot_hook(path: PathBuf, script: &Path) -> Result<Target, String> {
    let command = format!("{} --json", quote(script)?);
    Ok(Target::JsonHook(JsonHook {
        path,
        keys: vec!["hooks".into(), "sessionStart".into()],
        defaults: vec![("version".into(), json!(1))],
        handler: json!({ "type": "command", "bash": command.clone(), "timeoutSec": 10 }),
        command_key: "bash",
        command,
        matcher: None,
        label: "sessionStart hook",
    }))
}

fn hook_script(path: PathBuf) -> Target {
    Target::Owned(Owned {
        path,
        body: HOOK_BODY.to_string(),
        style: Comment::Hash,
        executable: true,
    })
}

enum Target {
    Owned(Owned),
    JsonHook(JsonHook),
}

impl Target {
    fn apply(&self, options: &InstallOptions) -> Result<Action, String> {
        match self {
            Target::Owned(owned) => owned.apply(options),
            Target::JsonHook(hook) => hook.apply(options),
        }
    }
}

#[derive(Clone, Copy)]
enum Comment {
    Hash,
    Html,
    Slash,
}

impl Comment {
    fn open(self) -> &'static str {
        match self {
            Comment::Hash => "# ",
            Comment::Html => "<!-- ",
            Comment::Slash => "// ",
        }
    }

    fn close(self) -> &'static str {
        match self {
            Comment::Hash => "",
            Comment::Html => " -->",
            Comment::Slash => "",
        }
    }

    fn wrap(self, text: &str) -> String {
        format!("{}{text}{}", self.open(), self.close())
    }
}

/// A file this installer writes whole. It carries a trailing marker holding the
/// digest of the body above it, which is what tells a stale install apart from a
/// file somebody edited by hand.
struct Owned {
    path: PathBuf,
    body: String,
    style: Comment,
    executable: bool,
}

impl Owned {
    fn rendered(&self) -> String {
        stamp(self.style, &ensure_trailing_newline(&self.body))
    }

    fn apply(&self, options: &InstallOptions) -> Result<Action, String> {
        let desired = self.rendered();
        let existing = read(&self.path)?;
        let outcome = match existing {
            None => Outcome::Created,
            Some(current) if current == desired => Outcome::Unchanged,
            Some(_) if options.force => Outcome::Updated,
            Some(current) => match self.ownership(&current) {
                Ownership::Ours => Outcome::Updated,
                Ownership::Unmanaged => Outcome::Blocked(
                    "not written by this installer; re-run with --force to replace it".into(),
                ),
                Ownership::Edited => Outcome::Blocked(
                    "edited since it was installed; re-run with --force to discard those edits"
                        .into(),
                ),
            },
        };
        if outcome.writes() && !options.dry_run {
            write_atomic(&self.path, &desired, self.executable)?;
        }
        Ok(Action {
            path: self.path.clone(),
            note: None,
            outcome,
        })
    }

    fn ownership(&self, current: &str) -> Ownership {
        let opener = format!("{}{MARKER}", self.style.open());
        let Some(start) = current.rfind(&opener) else {
            return Ownership::Unmanaged;
        };
        if current == stamp(self.style, &current[..start]) {
            Ownership::Ours
        } else {
            Ownership::Edited
        }
    }
}

/// A body plus the marker that records its digest — what an untouched install
/// looks like byte for byte, whatever version of the template produced it.
fn stamp(style: Comment, body: &str) -> String {
    let marker = style.wrap(&format!("{MARKER} {}", digest(body)));
    format!("{body}{marker}\n")
}

enum Ownership {
    Ours,
    Edited,
    Unmanaged,
}

/// A hook handler merged into a config file that may hold unrelated handlers.
/// Ours is the one whose command is exactly the command we write, so repeated
/// installs replace that handler alone and leave everything beside it — in the
/// same matcher group or elsewhere — untouched.
struct JsonHook {
    path: PathBuf,
    keys: Vec<String>,
    defaults: Vec<(String, Value)>,
    handler: Value,
    command_key: &'static str,
    command: String,
    /// `Some` for the harnesses whose entries are matcher groups of handlers,
    /// `None` for Copilot, whose entries are the handlers themselves.
    matcher: Option<&'static str>,
    label: &'static str,
}

/// Where an installed handler sits: the entry index, plus its index inside that
/// entry's group for the grouped harnesses.
struct Spot {
    entry: usize,
    handler: Option<usize>,
}

impl JsonHook {
    fn apply(&self, options: &InstallOptions) -> Result<Action, String> {
        let existing = read(&self.path)?;
        let action = |outcome| Action {
            path: self.path.clone(),
            note: Some(self.label),
            outcome,
        };
        let root = match &existing {
            Some(text) if !text.trim().is_empty() => match serde_json::from_str(text) {
                Ok(root) => root,
                Err(error) => {
                    return Ok(action(Outcome::Blocked(format!("invalid JSON: {error}"))));
                }
            },
            _ => json!({}),
        };
        let merged = match self.merge(root.clone(), options) {
            Ok(merged) => merged,
            Err(reason) => return Ok(action(Outcome::Blocked(reason))),
        };
        if merged == root {
            return Ok(action(Outcome::Unchanged));
        }
        if !options.dry_run {
            let mut text = serde_json::to_string_pretty(&merged).map_err(|e| e.to_string())?;
            text.push('\n');
            write_atomic(&self.path, &text, false)?;
        }
        Ok(action(if existing.is_some() {
            Outcome::Updated
        } else {
            Outcome::Created
        }))
    }

    fn merge(&self, mut root: Value, options: &InstallOptions) -> Result<Value, String> {
        let object = root
            .as_object_mut()
            .ok_or_else(|| "top level is not a JSON object".to_string())?;
        for (key, value) in &self.defaults {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
        let entries = array_at(object, &self.keys)?;
        let installed = self.installed(entries);
        let intact = matches!(installed.as_slice(), [spot] if self.intact(entries, spot));
        if !intact {
            if !installed.is_empty() && !options.force {
                return Err(format!(
                    "the registered {} is not the one this installer writes; \
                     re-run with --force to replace it",
                    self.label
                ));
            }
            self.strip(entries);
            entries.push(self.entry());
        }
        Ok(root)
    }

    fn entry(&self) -> Value {
        match self.matcher {
            Some(matcher) => json!({ "matcher": matcher, "hooks": [self.handler.clone()] }),
            None => self.handler.clone(),
        }
    }

    fn installed(&self, entries: &[Value]) -> Vec<Spot> {
        let mut spots = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            if self.matcher.is_none() {
                if self.ours(entry) {
                    spots.push(Spot {
                        entry: index,
                        handler: None,
                    });
                }
                continue;
            }
            let Some(handlers) = entry.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for (inner, handler) in handlers.iter().enumerate() {
                if self.ours(handler) {
                    spots.push(Spot {
                        entry: index,
                        handler: Some(inner),
                    });
                }
            }
        }
        spots
    }

    fn intact(&self, entries: &[Value], spot: &Spot) -> bool {
        let entry = &entries[spot.entry];
        match spot.handler {
            None => *entry == self.handler,
            Some(inner) => {
                entry.get("matcher").and_then(Value::as_str) == self.matcher
                    && entry["hooks"][inner] == self.handler
            }
        }
    }

    fn strip(&self, entries: &mut Vec<Value>) {
        if self.matcher.is_none() {
            entries.retain(|entry| !self.ours(entry));
            return;
        }
        entries.retain_mut(|entry| {
            let Some(handlers) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = handlers.len();
            handlers.retain(|handler| !self.ours(handler));
            before == handlers.len() || !handlers.is_empty()
        });
    }

    fn ours(&self, handler: &Value) -> bool {
        handler.get(self.command_key).and_then(Value::as_str) == Some(self.command.as_str())
    }
}

fn array_at<'a>(
    object: &'a mut Map<String, Value>,
    keys: &[String],
) -> Result<&'a mut Vec<Value>, String> {
    let (last, parents) = keys.split_last().expect("a hook target names its keys");
    let mut cursor = object;
    for key in parents {
        cursor = cursor
            .entry(key.clone())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| format!("`{key}` is not a JSON object"))?;
    }
    cursor
        .entry(last.clone())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| format!("`{last}` is not a JSON array"))
}

pub struct Report {
    groups: Vec<(Harness, Vec<Action>)>,
    dry_run: bool,
    syn_on_path: bool,
}

impl Report {
    pub fn blocked(&self) -> bool {
        self.groups
            .iter()
            .flat_map(|(_, actions)| actions)
            .any(|action| matches!(action.outcome, Outcome::Blocked(_)))
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.dry_run {
            writeln!(f, "dry run — nothing was written")?;
        }
        for (harness, actions) in &self.groups {
            writeln!(f, "{}", harness.label())?;
            for action in actions {
                let note = action.note.map(|n| format!(" ({n})")).unwrap_or_default();
                writeln!(
                    f,
                    "  {:<9} {}{note}",
                    action.outcome.verb(),
                    action.path.display()
                )?;
                if let Outcome::Blocked(reason) = &action.outcome {
                    writeln!(f, "            {reason}")?;
                }
            }
        }
        if !self.syn_on_path {
            writeln!(
                f,
                "note: `syn` is not on PATH — hooks will exit silently until it is"
            )?;
        }
        Ok(())
    }
}

struct Action {
    path: PathBuf,
    note: Option<&'static str>,
    outcome: Outcome,
}

enum Outcome {
    Created,
    Updated,
    Unchanged,
    Blocked(String),
}

impl Outcome {
    fn writes(&self) -> bool {
        matches!(self, Outcome::Created | Outcome::Updated)
    }

    fn verb(&self) -> &'static str {
        match self {
            Outcome::Created => "create",
            Outcome::Updated => "update",
            Outcome::Unchanged => "unchanged",
            Outcome::Blocked(_) => "blocked",
        }
    }
}

fn read(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

fn write_atomic(path: &Path, contents: &str, executable: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} names no file", path.display()))?;
    let temporary = parent.join(format!(".{}.synapse-tmp", name.to_string_lossy()));
    fs::write(&temporary, contents)
        .map_err(|e| format!("cannot write {}: {e}", temporary.display()))?;
    set_mode(&temporary, executable)?;
    fs::rename(&temporary, path).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        format!("cannot install {}: {e}", path.display())
    })
}

#[cfg(unix)]
fn set_mode(path: &Path, executable: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot set mode on {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _executable: bool) -> Result<(), String> {
    Ok(())
}

fn digest(body: &str) -> String {
    let hash = Sha256::digest(body.as_bytes());
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn ensure_trailing_newline(body: &str) -> String {
    if body.ends_with('\n') {
        body.to_string()
    } else {
        format!("{body}\n")
    }
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))
}

fn quote(path: &Path) -> Result<String, String> {
    Ok(format!("'{}'", path_str(path)?.replace('\'', r"'\''")))
}

fn syn_on_path() -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join("syn").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox {
        _root: tempfile::TempDir,
        homes: Homes,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temp home");
            let homes = Homes {
                claude: root.path().join(".claude"),
                copilot: root.path().join(".copilot"),
                codex: root.path().join(".codex"),
                omp: root.path().join(".omp/agent"),
            };
            Self { _root: root, homes }
        }

        fn install(&self, harness: Harness, options: &InstallOptions) -> Report {
            install(&[harness], &self.homes, options).expect("install")
        }

        fn files(&self, harness: Harness) -> Vec<(PathBuf, String)> {
            let mut found = Vec::new();
            collect(self.homes.of(harness), &mut found);
            found.sort_by(|a, b| a.0.cmp(&b.0));
            found
        }
    }

    fn collect(dir: &Path, into: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, into);
            } else {
                into.push((path.clone(), fs::read_to_string(&path).expect("read back")));
            }
        }
    }

    fn fresh() -> InstallOptions {
        InstallOptions {
            dry_run: false,
            force: false,
        }
    }

    fn verbs(report: &Report) -> Vec<&'static str> {
        report
            .groups
            .iter()
            .flat_map(|(_, actions)| actions)
            .map(|action| action.outcome.verb())
            .collect()
    }

    #[test]
    fn a_second_install_changes_nothing() {
        for &harness in Harness::all() {
            let sandbox = Sandbox::new();
            let first = sandbox.install(harness, &fresh());
            assert!(verbs(&first).iter().all(|verb| *verb == "create"));
            let after_first = sandbox.files(harness);

            let second = sandbox.install(harness, &fresh());
            assert_eq!(
                verbs(&second),
                vec!["unchanged"; verbs(&second).len()],
                "{harness:?} reinstall"
            );
            assert_eq!(after_first, sandbox.files(harness), "{harness:?} bytes");
        }
    }

    #[test]
    fn the_session_hook_is_registered_exactly_once() {
        for (harness, config, event) in [
            (Harness::Claude, "settings.json", "SessionStart"),
            (Harness::Copilot, "hooks/synapse.json", "sessionStart"),
            (Harness::Codex, "hooks.json", "SessionStart"),
        ] {
            let sandbox = Sandbox::new();
            sandbox.install(harness, &fresh());
            sandbox.install(harness, &fresh());
            let text = fs::read_to_string(sandbox.homes.of(harness).join(config)).expect("config");
            let root: Value = serde_json::from_str(&text).expect("valid JSON");
            assert_eq!(
                root["hooks"][event].as_array().expect("array").len(),
                1,
                "{harness:?}"
            );
        }
    }

    #[test]
    fn unrelated_hooks_survive_and_the_file_keeps_its_other_settings() {
        let sandbox = Sandbox::new();
        let settings = sandbox.homes.claude.join("settings.json");
        fs::create_dir_all(&sandbox.homes.claude).expect("home");
        fs::write(
            &settings,
            r#"{"model":"opus","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}],"Stop":[]}}"#,
        )
        .expect("seed");

        sandbox.install(Harness::Claude, &fresh());
        sandbox.install(Harness::Claude, &fresh());

        let root: Value =
            serde_json::from_str(&fs::read_to_string(&settings).expect("read")).expect("JSON");
        let starts = root["hooks"]["SessionStart"].as_array().expect("array");
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["hooks"][0]["command"], "echo hi");
        assert_eq!(root["model"], "opus");
        assert!(root["hooks"]["Stop"].is_array());
    }

    fn config_of(sandbox: &Sandbox, harness: Harness) -> PathBuf {
        sandbox.homes.of(harness).join(match harness {
            Harness::Claude => "settings.json",
            Harness::Copilot => "hooks/synapse.json",
            Harness::Codex => "hooks.json",
            Harness::Omp => "extensions/synapse.ts",
        })
    }

    /// The harnesses whose hook registration is a JSON hook config merge.
    /// omp's hook is an `Owned` `.ts` module, not a JSON hook.
    fn json_hooks() -> [Harness; 3] {
        [Harness::Claude, Harness::Copilot, Harness::Codex]
    }

    fn json_at(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("JSON")
    }

    #[test]
    fn a_handler_beside_ours_in_the_same_group_survives_a_reinstall() {
        for harness in [Harness::Claude, Harness::Codex] {
            let sandbox = Sandbox::new();
            sandbox.install(harness, &fresh());
            let config = config_of(&sandbox, harness);

            let mut root = json_at(&config);
            root["hooks"]["SessionStart"][0]["hooks"]
                .as_array_mut()
                .expect("our group")
                .push(json!({ "type": "command", "command": "echo hi" }));
            fs::write(&config, serde_json::to_string(&root).expect("render")).expect("seed");

            let report = sandbox.install(harness, &fresh());
            assert!(!report.blocked(), "{harness:?}");

            let root = json_at(&config);
            let groups = root["hooks"]["SessionStart"].as_array().expect("array");
            assert_eq!(groups.len(), 1, "{harness:?} groups");
            let handlers = groups[0]["hooks"].as_array().expect("handlers");
            assert_eq!(handlers.len(), 2, "{harness:?} handlers");
            assert_eq!(handlers[1]["command"], "echo hi", "{harness:?}");
        }
    }

    #[test]
    fn an_edited_registration_blocks_until_forced() {
        for harness in json_hooks() {
            let sandbox = Sandbox::new();
            sandbox.install(harness, &fresh());
            let config = config_of(&sandbox, harness);

            let mut root = json_at(&config);
            match harness {
                Harness::Copilot => root["hooks"]["sessionStart"][0]["timeoutSec"] = json!(60),
                _ => root["hooks"]["SessionStart"][0]["hooks"][0]["timeout"] = json!(60),
            }
            let edited = serde_json::to_string(&root).expect("render");
            fs::write(&config, &edited).expect("seed");

            let report = sandbox.install(harness, &fresh());
            assert!(report.blocked(), "{harness:?}");
            assert_eq!(fs::read_to_string(&config).expect("read"), edited);

            let forced = sandbox.install(
                harness,
                &InstallOptions {
                    dry_run: false,
                    force: true,
                },
            );
            assert!(!forced.blocked(), "{harness:?}");
            let root = json_at(&config);
            match harness {
                Harness::Copilot => {
                    let entries = root["hooks"]["sessionStart"].as_array().expect("array");
                    assert_eq!(entries.len(), 1, "{harness:?}");
                    assert_eq!(entries[0]["timeoutSec"], 10, "{harness:?}");
                }
                _ => {
                    let groups = root["hooks"]["SessionStart"].as_array().expect("array");
                    assert_eq!(groups.len(), 1, "{harness:?}");
                    assert_eq!(groups[0]["hooks"].as_array().expect("handlers").len(), 1);
                    assert_eq!(groups[0]["hooks"][0]["timeout"], 10, "{harness:?}");
                }
            }
        }
    }

    #[test]
    fn a_file_we_did_not_write_is_left_alone_until_forced() {
        let sandbox = Sandbox::new();
        let skill = sandbox.homes.codex.join("skills/synapse/SKILL.md");
        fs::create_dir_all(skill.parent().expect("parent")).expect("dirs");
        fs::write(&skill, "mine, hands off\n").expect("seed");

        let report = sandbox.install(Harness::Codex, &fresh());
        assert!(report.blocked());
        assert_eq!(
            fs::read_to_string(&skill).expect("read"),
            "mine, hands off\n"
        );

        let forced = sandbox.install(
            Harness::Codex,
            &InstallOptions {
                dry_run: false,
                force: true,
            },
        );
        assert!(!forced.blocked());
        assert!(fs::read_to_string(&skill).expect("read").contains(MARKER));
    }

    fn owned(path: &Path, body: &str) -> Owned {
        Owned {
            path: path.to_path_buf(),
            body: body.to_string(),
            style: Comment::Html,
            executable: false,
        }
    }

    fn outcome(owned: &Owned) -> Outcome {
        owned.apply(&fresh()).expect("apply").outcome
    }

    #[test]
    fn a_changed_template_replaces_an_untouched_install() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("SKILL.md");
        assert_eq!(outcome(&owned(&path, "one\n")).verb(), "create");
        assert_eq!(outcome(&owned(&path, "two\n")).verb(), "update");
        assert!(fs::read_to_string(&path).expect("read").starts_with("two"));
    }

    #[test]
    fn a_block_says_whether_the_file_was_ours_to_begin_with() {
        let dir = tempfile::tempdir().expect("temp");
        let ours = dir.path().join("ours.md");
        outcome(&owned(&ours, "one\n"));
        fs::write(
            &ours,
            format!("{}tweak\n", fs::read_to_string(&ours).expect("read")),
        )
        .expect("edit");
        let Outcome::Blocked(reason) = outcome(&owned(&ours, "two\n")) else {
            panic!("an edited install must block");
        };
        assert!(reason.contains("edited since"), "{reason}");

        let theirs = dir.path().join("theirs.md");
        fs::write(&theirs, "not ours\n").expect("seed");
        let Outcome::Blocked(reason) = outcome(&owned(&theirs, "two\n")) else {
            panic!("a foreign file must block");
        };
        assert!(reason.contains("not written by this installer"), "{reason}");
    }

    #[test]
    fn hand_edits_to_an_installed_file_are_reported_not_discarded() {
        let sandbox = Sandbox::new();
        sandbox.install(Harness::Codex, &fresh());
        let skill = sandbox.homes.codex.join("skills/synapse/SKILL.md");
        let edited = format!(
            "{}\nlocal tweak\n",
            fs::read_to_string(&skill).expect("read")
        );
        fs::write(&skill, &edited).expect("edit");

        let report = sandbox.install(Harness::Codex, &fresh());
        assert!(report.blocked());
        assert_eq!(fs::read_to_string(&skill).expect("read"), edited);
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let sandbox = Sandbox::new();
        let report = sandbox.install(
            Harness::Claude,
            &InstallOptions {
                dry_run: true,
                force: false,
            },
        );
        assert_eq!(verbs(&report), vec!["create", "create", "create"]);
        assert!(sandbox.files(Harness::Claude).is_empty());
    }

    #[test]
    fn every_harness_gets_the_same_skill() {
        let sandbox = Sandbox::new();
        let installed: Vec<String> = Harness::all()
            .iter()
            .map(|&harness| {
                sandbox.install(harness, &fresh());
                fs::read_to_string(sandbox.homes.of(harness).join("skills/synapse/SKILL.md"))
                    .expect("read")
            })
            .collect();
        assert!(installed.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn the_codex_hook_names_its_session_events_and_spares_the_other_events() {
        let sandbox = Sandbox::new();
        let config = sandbox.homes.codex.join("hooks.json");
        fs::create_dir_all(&sandbox.homes.codex).expect("home");
        fs::write(
            &config,
            r#"{"hooks":{"PostToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","command":"audit.sh"}]}]}}"#,
        )
        .expect("seed");

        sandbox.install(Harness::Codex, &fresh());
        sandbox.install(Harness::Codex, &fresh());

        let root: Value =
            serde_json::from_str(&fs::read_to_string(&config).expect("read")).expect("JSON");
        let starts = root["hooks"]["SessionStart"].as_array().expect("array");
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0]["matcher"], CODEX_MATCHER);
        assert_eq!(
            root["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "audit.sh"
        );
    }

    #[test]
    fn a_named_omp_profile_keeps_its_own_agent_dir() {
        let named = |profile: Option<&str>, legacy: Option<&str>| OmpEnv {
            profile: profile.map(str::to_string),
            legacy_profile: legacy.map(str::to_string),
            config_dir: None,
        };

        assert_eq!(
            named(Some("work"), None).profile_agent_dir(),
            Some(PathBuf::from(".omp/profiles/work/agent"))
        );
        assert_eq!(
            named(None, Some("work")).profile_agent_dir(),
            Some(PathBuf::from(".omp/profiles/work/agent"))
        );
        assert_eq!(named(Some(""), Some("work")).profile_agent_dir(), None);
        assert_eq!(named(Some("default"), None).profile_agent_dir(), None);
        assert_eq!(named(None, None).profile_agent_dir(), None);

        let relocated = OmpEnv {
            profile: Some("work".into()),
            legacy_profile: None,
            config_dir: Some(".pi".into()),
        };
        assert_eq!(
            relocated.profile_agent_dir(),
            Some(PathBuf::from(".pi/profiles/work/agent"))
        );
        assert_eq!(relocated.root().join("agent"), PathBuf::from(".pi/agent"));
    }

    #[test]
    fn omp_installs_a_ts_hook_module_not_a_json_hook() {
        let sandbox = Sandbox::new();
        sandbox.install(Harness::Omp, &fresh());
        sandbox.install(Harness::Omp, &fresh());

        let hook = sandbox.homes.omp.join("extensions/synapse.ts");
        let text = fs::read_to_string(&hook).expect("read hook");
        assert!(
            text.contains(r#"run("syn", ["context"]"#),
            "the module reads the digest from `syn context`"
        );
        assert!(
            text.contains("// synapse:managed 1"),
            "managed marker present"
        );
        assert!(!sandbox.homes.omp.join("settings.json").exists());
        let files = sandbox.files(Harness::Omp);
        assert_eq!(
            files
                .iter()
                .map(|(p, _)| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                sandbox
                    .homes
                    .omp
                    .join("extensions/synapse.ts")
                    .display()
                    .to_string(),
                sandbox
                    .homes
                    .omp
                    .join("skills/synapse/SKILL.md")
                    .display()
                    .to_string(),
            ]
        );
    }

    #[test]
    fn the_installed_hook_script_is_executable() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let sandbox = Sandbox::new();
            sandbox.install(Harness::Claude, &fresh());
            let script = sandbox.homes.claude.join("hooks/synapse-session-start.sh");
            let mode = fs::metadata(&script).expect("stat").permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }
}
