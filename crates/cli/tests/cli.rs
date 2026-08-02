mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::{Behavior, Stub, dead_port};
use tempfile::TempDir;

struct Machine {
    home: TempDir,
    cwd: PathBuf,
}

impl Machine {
    fn new(url: &str) -> Self {
        Self::with_config(&format!(
            "url = \"{url}\"\ntoken = \"t\"\ndefault_workspace = \"work\"\n"
        ))
    }

    fn with_config(config: &str) -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let config_dir = home.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), config).unwrap();
        let cwd = home.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        Self { home, cwd }
    }

    fn state_dir(&self) -> PathBuf {
        self.home.path().join("state")
    }

    fn outbox(&self) -> PathBuf {
        self.state_dir().join("outbox")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_syn"))
            .args(args)
            .current_dir(&self.cwd)
            .env("SYNAPSE_CONFIG_DIR", self.home.path().join("config"))
            .env("SYNAPSE_STATE_DIR", self.state_dir())
            .env("HOME", self.home.path())
            .output()
            .expect("run syn")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn json_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".json"))
        .collect();
    names.sort();
    names
}

#[test]
fn a_reachable_server_saves_immediately_and_reports_the_workspace() {
    let stub = Stub::start();
    let machine = Machine::new(&stub.url());

    let output = machine.run(&["save", "proto fields are additive", "--type", "feedback"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("saved m_"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("(work · workspace)"),
        "{}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("no git origin here"),
        "{}",
        stderr(&output)
    );
    assert!(json_files(&machine.outbox()).is_empty());
    stub.with(|state| assert_eq!(state.memories.len(), 1));
}

#[test]
fn an_unreachable_server_queues_the_save_and_says_it_is_not_recallable() {
    let machine = Machine::new(&format!("http://127.0.0.1:{}", dead_port()));

    let output = machine.run(&["save", "queued fact", "--type", "project"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("queued locally, not yet recallable"),
        "{}",
        stdout(&output)
    );
    assert_eq!(json_files(&machine.outbox()).len(), 1);

    let pending = machine.run(&["list", "--pending"]);
    assert!(
        stdout(&pending).contains("(work · workspace) queued"),
        "{}",
        stdout(&pending)
    );
}

#[test]
fn a_connection_dropped_after_the_server_commits_retries_without_duplicating() {
    let stub = Stub::start();
    stub.script(vec![Behavior::DropAfterCommit]);
    let machine = Machine::new(&stub.url());

    let first = machine.run(&["save", "committed but unanswered", "--type", "project"]);
    assert!(
        stdout(&first).contains("queued locally"),
        "{} / {}",
        stdout(&first),
        stderr(&first)
    );
    assert_eq!(json_files(&machine.outbox()).len(), 1);

    let retry = machine.run(&["recall", "anything"]);
    assert!(retry.status.success(), "{}", stderr(&retry));

    assert!(
        json_files(&machine.outbox()).is_empty(),
        "queued item was not cleared"
    );
    stub.with(|state| {
        let puts = state.puts();
        assert_eq!(puts.len(), 2, "expected one send plus one retry");
        assert_eq!(puts[0].path, puts[1].path, "retry used a different id");
        assert_eq!(puts[0].body, puts[1].body, "retry changed the payload");
        assert_eq!(state.memories.len(), 1, "retry created a duplicate memory");
    });
}

#[test]
fn queued_saves_flush_oldest_first_once_the_server_returns() {
    let port = dead_port();
    let machine = Machine::new(&format!("http://127.0.0.1:{port}"));
    for content in ["first", "second", "third"] {
        let output = machine.run(&["save", content, "--type", "project"]);
        assert!(
            stdout(&output).contains("queued locally"),
            "{}",
            stdout(&output)
        );
    }
    assert_eq!(json_files(&machine.outbox()).len(), 3);

    let stub = Stub::start_on(port);
    let output = machine.run(&["context"]);
    assert!(output.status.success(), "{}", stderr(&output));

    assert!(json_files(&machine.outbox()).is_empty());
    stub.with(|state| {
        let sent: Vec<String> = state
            .puts()
            .iter()
            .map(|put| {
                serde_json::from_str::<serde_json::Value>(&put.body).unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(sent, ["first", "second", "third"]);
    });
}

#[test]
fn a_non_retryable_rejection_dead_letters_the_save() {
    let stub = Stub::start();
    stub.script(vec![Behavior::Status(
        400,
        "content exceeds token window".into(),
    )]);
    let machine = Machine::new(&stub.url());

    let output = machine.run(&["save", "too big", "--type", "project"]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("content exceeds token window"),
        "{}",
        stderr(&output)
    );
    assert!(json_files(&machine.outbox()).is_empty());
    assert_eq!(json_files(&machine.outbox().join("dead-letter")).len(), 1);

    let pending = machine.run(&["list", "--pending"]);
    assert!(
        stdout(&pending).contains("dead-letter: server returned 400"),
        "{}",
        stdout(&pending)
    );

    let reassigned = machine.run(&["list", "--pending", "--reassign", "personal"]);
    assert!(
        stdout(&reassigned).contains("reassigned 1 pending saves to personal"),
        "{}",
        stdout(&reassigned)
    );
    assert_eq!(json_files(&machine.outbox()).len(), 1);
    assert!(json_files(&machine.outbox().join("dead-letter")).is_empty());

    let discarded = machine.run(&["list", "--pending", "--discard"]);
    assert!(
        stdout(&discarded).contains("discarded 1"),
        "{}",
        stdout(&discarded)
    );
    assert!(json_files(&machine.outbox()).is_empty());
}

#[test]
fn a_5xx_defers_the_queue_instead_of_dead_lettering_it() {
    let stub = Stub::start();
    stub.script(vec![Behavior::Status(503, "unready".into())]);
    let machine = Machine::new(&stub.url());

    let output = machine.run(&["save", "server is booting", "--type", "project"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("queued locally"),
        "{}",
        stdout(&output)
    );
    assert_eq!(json_files(&machine.outbox()).len(), 1);
    assert!(json_files(&machine.outbox().join("dead-letter")).is_empty());
}

#[test]
fn saves_fail_closed_in_a_git_checkout_with_no_matching_rule() {
    let stub = Stub::start();
    let machine = Machine::with_config(&format!(
        "url = \"{}\"\ntoken = \"t\"\ndefault_workspace = \"work\"\n",
        stub.url()
    ));
    std::fs::create_dir_all(machine.cwd.join(".git")).unwrap();

    let refused = machine.run(&["save", "risky fact", "--type", "project"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("no workspace rule matches"),
        "{}",
        stderr(&refused)
    );
    assert!(
        json_files(&machine.outbox()).is_empty(),
        "a refused save must not queue"
    );

    let explicit = machine.run(&[
        "save",
        "risky fact",
        "--type",
        "project",
        "--workspace",
        "work",
    ]);
    assert!(explicit.status.success(), "{}", stderr(&explicit));
    assert!(
        stdout(&explicit).contains("(work · workspace)"),
        "{}",
        stdout(&explicit)
    );

    let named_shared = machine.run(&[
        "save",
        "risky fact",
        "--type",
        "project",
        "--workspace",
        "shared",
    ]);
    assert!(!named_shared.status.success());
    assert!(
        stderr(&named_shared).contains("syn remember"),
        "{}",
        stderr(&named_shared)
    );

    let read = machine.run(&["recall", "risky"]);
    assert!(
        read.status.success(),
        "reads still use the machine default: {}",
        stderr(&read)
    );
}

#[test]
fn recall_prints_workspace_scope_and_date_per_hit() {
    let stub = Stub::start();
    stub.with(|state| {
        state.search = Some(
            serde_json::json!({"hits": [
                {"origin": {"workspace": "work"}, "score": 0.9, "id": "m_0000000000000000000001",
                 "content": "Staging deploys go through ArgoCD.", "kind": "project",
                 "scope": "fresha/offers", "tags": [], "pinned": false,
                 "created_at": "2026-07-14T09:00:00Z", "updated_at": "2026-07-14T09:00:00Z"},
                {"origin": "preference", "score": 0.4, "id": "m_0000000000000000000002",
                 "content": "Prefers Datadog links.", "kind": "user",
                 "scope": "workspace", "tags": [], "pinned": false,
                 "created_at": "2026-06-02T09:00:00Z", "updated_at": "2026-06-02T09:00:00Z"}
            ]})
            .to_string(),
        );
    });
    let machine = Machine::new(&stub.url());

    let output = machine.run(&["recall", "how do we deploy offers"]);

    let printed = stdout(&output);
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(
        lines[0],
        "[m_0000000000000000000001] (work · fresha/offers, 2026-07-14) Staging deploys go through ArgoCD."
    );
    assert_eq!(
        lines[1],
        "[m_0000000000000000000002] (preference, 2026-06-02) Prefers Datadog links."
    );
    assert!(lines[2].starts_with("(2 results, "), "{}", lines[2]);
}

#[test]
fn all_workspaces_recall_groups_hits_by_workspace() {
    let stub = Stub::start();
    stub.with(|state| {
        state.search = Some(
            serde_json::json!({"groups": [
                {"origin": {"workspace": "work"}, "hits": [
                    {"origin": {"workspace": "work"}, "score": 0.9, "id": "m_0000000000000000000001",
                     "content": "work fact", "kind": "project", "scope": "workspace", "tags": [],
                     "pinned": false, "created_at": "2026-07-14T09:00:00Z", "updated_at": "2026-07-14T09:00:00Z"}]},
                {"origin": "preference", "hits": [
                    {"origin": "preference", "score": 0.5, "id": "m_0000000000000000000003",
                     "content": "a preference", "kind": "user", "scope": "workspace", "tags": [],
                     "pinned": false, "created_at": "2026-07-14T09:00:00Z", "updated_at": "2026-07-14T09:00:00Z"}]}
            ]})
            .to_string(),
        );
    });
    let machine = Machine::new(&stub.url());

    let output = machine.run(&["recall", "anything", "--all-workspaces"]);

    let printed = stdout(&output);
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(lines[0], "## work");
    assert_eq!(
        lines[1],
        "[m_0000000000000000000001] (work, 2026-07-14) work fact"
    );
    assert_eq!(lines[2], "## preference");
    assert_eq!(
        lines[3],
        "[m_0000000000000000000003] (preference, 2026-07-14) a preference"
    );
    assert!(lines[4].starts_with("(2 results, "));
}

#[test]
fn context_prints_a_digest_and_stays_silent_when_empty() {
    let stub = Stub::start();
    let machine = Machine::new(&stub.url());

    let empty = machine.run(&["context"]);
    assert!(empty.status.success(), "{}", stderr(&empty));
    assert_eq!(stdout(&empty), "");

    stub.with(|state| {
        state.context = Some(
            serde_json::json!({
                "pinned": [{"origin": "preference", "id": "m_0000000000000000000002",
                    "content": "Prefers Datadog links", "kind": "user", "scope": "workspace",
                    "tags": [], "pinned": true, "created_at": "2026-06-02T09:00:00Z",
                    "updated_at": "2026-06-02T09:00:00Z"}],
                "recent_project": [],
                "shared_user": []
            })
            .to_string(),
        );
    });
    let filled = machine.run(&["context"]);
    assert_eq!(
        stdout(&filled),
        "## Memory (syn context)\n\
         - [m_0000000000000000000002] Prefers Datadog links\n\
         - (recall more with: syn recall \"<query>\")\n"
    );
}

#[test]
fn config_and_workspace_use_write_a_private_config() {
    let machine = Machine::with_config("");
    let config = machine.home.path().join("config").join("config.toml");

    assert!(
        machine
            .run(&["config", "set-url", "https://memory.example/"])
            .status
            .success()
    );
    assert!(
        machine
            .run(&["config", "set-token", "sekret"])
            .status
            .success()
    );
    assert!(
        machine
            .run(&["workspace", "use", "personal"])
            .status
            .success()
    );

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("url = \"https://memory.example\""), "{text}");
    assert!(text.contains("token = \"sekret\""), "{text}");
    assert!(text.contains("default_workspace = \"personal\""), "{text}");

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&config).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_missing_token_fails_with_one_actionable_line() {
    let machine = Machine::with_config("default_workspace = \"work\"\n");

    let output = machine.run(&["recall", "anything"]);

    assert!(!output.status.success());
    assert_eq!(
        stderr(&output),
        "error: no token configured; run: syn config set-token <token>\n"
    );
}

#[test]
fn id_commands_target_the_workspace_the_hit_came_from() {
    let stub = Stub::start();
    let machine = Machine::new(&stub.url());

    let output = machine.run(&[
        "forget",
        "m_0000000000000000000002",
        "--workspace",
        "personal",
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("(personal)"),
        "{}",
        stdout(&output)
    );
    stub.with(|state| {
        let deleted = state
            .recorded
            .iter()
            .find(|r| r.method == "DELETE")
            .expect("delete reached the server");
        assert_eq!(deleted.path, "/memories/m_0000000000000000000002");
    });
}

#[test]
fn preference_flag_routes_id_commands_away_from_any_workspace() {
    let stub = Stub::start();
    let machine = Machine::new(&stub.url());

    let shown = machine.run(&["show", "m_0000000000000000000002", "--preference"]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    assert!(
        stdout(&shown).contains("(preference,"),
        "{}",
        stdout(&shown)
    );

    let pinned = machine.run(&["pin", "m_0000000000000000000002", "--preference"]);
    assert!(pinned.status.success(), "{}", stderr(&pinned));
    assert!(
        stdout(&pinned).contains("(preference)"),
        "{}",
        stdout(&pinned)
    );

    let forgotten = machine.run(&["forget", "m_0000000000000000000002", "--preference"]);
    assert!(forgotten.status.success(), "{}", stderr(&forgotten));

    stub.with(|state| {
        for method in ["GET", "PATCH", "DELETE"] {
            let hit = state
                .recorded
                .iter()
                .find(|r| r.method == method && r.path.starts_with("/preferences/"))
                .unwrap_or_else(|| panic!("{method} did not reach /preferences"));
            assert_eq!(hit.path, "/preferences/m_0000000000000000000002");
        }
        assert!(
            state
                .recorded
                .iter()
                .all(|r| !r.path.starts_with("/memories")),
            "a preference command touched the workspace surface"
        );
    });
}

#[test]
fn remember_saves_without_naming_a_workspace_or_inheriting_the_repo_scope() {
    let stub = Stub::start();
    let machine = Machine::new(&stub.url());
    std::fs::create_dir_all(machine.cwd.join(".git")).unwrap();

    let output = machine.run(&[
        "remember",
        "Benedikt prefers Datadog links over log tailing",
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("saved m_"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("(preference)"),
        "{}",
        stdout(&output)
    );
    assert!(json_files(&machine.outbox()).is_empty());

    stub.with(|state| {
        let put = state
            .recorded
            .iter()
            .find(|r| r.method == "PUT")
            .expect("preference reached the server");
        assert!(put.path.starts_with("/preferences/"), "{}", put.path);
        let body: serde_json::Value = serde_json::from_str(&put.body).unwrap();
        assert_eq!(body["kind"], "user");
        assert!(
            body.get("scope").is_none(),
            "scope leaked into the body: {body}"
        );
    });
}

#[test]
fn a_queued_preference_replays_as_a_preference() {
    let port = dead_port();
    let machine = Machine::new(&format!("http://127.0.0.1:{port}"));

    let queued = machine.run(&["remember", "prefers oat milk"]);
    assert!(
        stdout(&queued).contains("queued locally"),
        "{}",
        stdout(&queued)
    );

    let pending = machine.run(&["list", "--pending"]);
    assert!(
        stdout(&pending).contains("(preference) queued"),
        "{}",
        stdout(&pending)
    );

    let untouched = machine.run(&["list", "--pending", "--reassign", "personal"]);
    assert!(
        stdout(&untouched).contains("left 1 preferences alone"),
        "{}",
        stdout(&untouched)
    );

    let stub = Stub::start_on(port);
    let flushed = machine.run(&["context"]);
    assert!(flushed.status.success(), "{}", stderr(&flushed));
    assert!(json_files(&machine.outbox()).is_empty());
    stub.with(|state| {
        let put = state.puts()[0];
        assert!(put.path.starts_with("/preferences/"), "{}", put.path);
    });
}

#[test]
fn workspace_map_writes_a_path_rule_that_saves_resolve_against() {
    let stub = Stub::start();
    let machine = Machine::with_config(&format!("url = \"{}\"\ntoken = \"t\"\n", stub.url()));
    std::fs::create_dir_all(machine.cwd.join(".git")).unwrap();

    let refused = machine.run(&["save", "a fact", "--type", "project"]);
    assert!(
        !refused.status.success(),
        "no rule and no default: {}",
        stdout(&refused)
    );

    let mapped = machine.run(&["workspace", "map", machine.cwd.to_str().unwrap(), "work"]);
    assert!(mapped.status.success(), "{}", stderr(&mapped));
    assert!(
        stdout(&mapped).contains("resolves to workspace work"),
        "{}",
        stdout(&mapped)
    );

    let saved = machine.run(&["save", "a fact", "--type", "project"]);
    assert!(saved.status.success(), "{}", stderr(&saved));
    assert!(
        stdout(&saved).contains("(work · workspace)"),
        "{}",
        stdout(&saved)
    );

    let remapped = machine.run(&[
        "workspace",
        "map",
        machine.cwd.to_str().unwrap(),
        "personal",
    ]);
    assert!(remapped.status.success(), "{}", stderr(&remapped));
    let config =
        std::fs::read_to_string(machine.home.path().join("config").join("config.toml")).unwrap();
    assert_eq!(config.matches("[[workspace_rules]]").count(), 1, "{config}");
    assert!(config.contains("workspace = \"personal\""), "{config}");
}
