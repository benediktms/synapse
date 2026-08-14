mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::{BAD_REQUEST, Behavior, CONFLICT, NOT_READY, Recorded, Stub, flock, socket_in};
use tempfile::TempDir;

struct Machine {
    home: TempDir,
    cwd: PathBuf,
}

impl Machine {
    fn new() -> Self {
        Self::with_config("transport = \"daemon\"\ndefault_workspace = \"work\"\n")
    }

    /// Where `syn` will look for the daemon, given this machine's state dir.
    fn socket(&self) -> PathBuf {
        socket_in(&self.state_dir())
    }

    /// Bind the stub where this machine's `syn` will look. Leaving it unbound is how a
    /// test exercises an unreachable daemon.
    fn stub(&self) -> Stub {
        Stub::start_at(self.socket())
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

    /// The routing ladder now confirms a checkout via a real `git rev-parse`, so a
    /// fixture testing fail-closed behaviour needs a real repository, not a bare `.git`.
    /// The initial commit gives `git worktree add -b` a branch to check out.
    fn init_git_repo(&self) {
        self.git(&["-c", "init.defaultBranch=main", "init", "-q"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    }

    fn init_git_repo_with_origin(&self, origin: &str) {
        self.init_git_repo();
        self.git(&["remote", "add", "origin", origin]);
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .arg("-C")
            .arg(&self.cwd)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_syn"))
            .args(args)
            .current_dir(&self.cwd)
            .env("SYNAPSE_CONFIG_DIR", self.home.path().join("config"))
            .env("SYNAPSE_STATE_DIR", self.state_dir())
            .env("SYNAPSE_NO_DAEMON_AUTOSTART", "1")
            .env("HOME", self.home.path())
            .output()
            .expect("run syn")
    }

    /// A `PATH` with nothing on it, so any command that shells out to `git` fails to
    /// spawn — proves facts are never probed eagerly for commands that don't need them.
    fn run_without_git(&self, args: &[&str]) -> Output {
        let empty_path = self.home.path().join("no-git-path");
        std::fs::create_dir_all(&empty_path).unwrap();
        Command::new(env!("CARGO_BIN_EXE_syn"))
            .args(args)
            .current_dir(&self.cwd)
            .env("SYNAPSE_CONFIG_DIR", self.home.path().join("config"))
            .env("SYNAPSE_STATE_DIR", self.state_dir())
            .env("SYNAPSE_NO_DAEMON_AUTOSTART", "1")
            .env("HOME", self.home.path())
            .env("PATH", &empty_path)
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
    let machine = Machine::new();
    let stub = machine.stub();

    let output = machine.run(&[
        "save",
        "--body",
        "proto fields are additive",
        "--title",
        "A title",
        "--type",
        "feedback",
    ]);

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
fn save_with_importance_sends_the_tier_to_the_server() {
    let machine = Machine::new();
    let stub = machine.stub();

    let output = machine.run(&[
        "save",
        "--body",
        "architectural decision",
        "--title",
        "A title",
        "--type",
        "project",
        "--importance",
        "high",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    stub.with(|state| {
        assert_eq!(state.memories.len(), 1);
        let body: serde_json::Value =
            serde_json::from_str(state.memories.values().next().unwrap()).unwrap();
        assert_eq!(body["importance"], "high");
    });
}

#[test]
fn everywhere_save_forwards_importance_to_the_preference() {
    let machine = Machine::new();
    let stub = machine.stub();

    let output = machine.run(&[
        "save",
        "--body",
        "prefers live demo by default",
        "--title",
        "A title",
        "--type",
        "user",
        "--scope",
        "everywhere",
        "--importance",
        "high",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    stub.with(|state| {
        assert_eq!(state.memories.len(), 1);
        let body: serde_json::Value =
            serde_json::from_str(state.memories.values().next().unwrap()).unwrap();
        assert_eq!(body["importance"], "high");
    });
}

#[test]
fn save_sends_the_title_to_the_server() {
    let machine = Machine::new();
    let stub = machine.stub();

    let output = machine.run(&[
        "save",
        "--body",
        "Deploys go through ArgoCD, with a staging lane in front of production.",
        "--type",
        "project",
        "--title",
        "ArgoCD owns deploys",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    stub.with(|state| {
        let body: serde_json::Value =
            serde_json::from_str(state.memories.values().next().unwrap()).unwrap();
        assert_eq!(body["title"], "ArgoCD owns deploys");
    });
}

#[test]
fn save_rejects_an_unknown_importance_tier() {
    let machine = Machine::new();
    let output = machine.run(&[
        "save",
        "--body",
        "fact",
        "--title",
        "A title",
        "--type",
        "project",
        "--importance",
        "urgent",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("possible values"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unreachable_server_queues_the_save_and_says_it_is_not_recallable() {
    let machine = Machine::new();

    let output = machine.run(&[
        "save",
        "--body",
        "queued fact",
        "--title",
        "A title",
        "--type",
        "project",
    ]);

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

/// The suite drives `syn` against a stub socket, and a real daemon would build an embedder
/// and download a model into the temp state dir. `ensure_running` writes `daemon.log` only
/// when it spawns, so its absence is the evidence that nothing was started.
#[test]
fn a_command_against_an_unreachable_daemon_starts_no_daemon() {
    let machine = Machine::new();

    let output = machine.run(&[
        "save",
        "--body",
        "queued fact",
        "--title",
        "A title",
        "--type",
        "project",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let log = machine.state_dir().join("daemon").join("daemon.log");
    assert!(
        !log.exists(),
        "a daemon was spawned; {} exists",
        log.display()
    );
}

#[test]
fn a_connection_dropped_after_the_server_commits_retries_without_duplicating() {
    let machine = Machine::new();
    let stub = machine.stub();
    stub.script(vec![Behavior::DropAfterCommit]);

    let first = machine.run(&[
        "save",
        "--body",
        "committed but unanswered",
        "--title",
        "A title",
        "--type",
        "project",
    ]);
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
        let puts = state.saves();
        assert_eq!(puts.len(), 2, "expected one send plus one retry");
        assert_eq!(puts[0].id(), puts[1].id(), "retry used a different id");
        assert_eq!(puts[0].params, puts[1].params, "retry changed the payload");
        assert_eq!(state.memories.len(), 1, "retry created a duplicate memory");
    });
}

#[test]
fn queued_saves_flush_oldest_first_once_the_server_returns() {
    let machine = Machine::new();
    for content in ["first", "second", "third"] {
        let output = machine.run(&[
            "save", "--body", content, "--title", "A title", "--type", "project",
        ]);
        assert!(
            stdout(&output).contains("queued locally"),
            "{}",
            stdout(&output)
        );
    }
    assert_eq!(json_files(&machine.outbox()).len(), 3);

    let stub = machine.stub();
    let output = machine.run(&["context"]);
    assert!(output.status.success(), "{}", stderr(&output));

    assert!(json_files(&machine.outbox()).is_empty());
    stub.with(|state| {
        let sent: Vec<String> = state
            .saves()
            .iter()
            .map(|put| {
                serde_json::from_str::<serde_json::Value>(&put.params).unwrap()["content"]
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
    let machine = Machine::new();
    let stub = machine.stub();
    stub.script(vec![Behavior::Error(CONFLICT, "id already taken".into())]);

    let output = machine.run(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("id already taken"),
        "{}",
        stderr(&output)
    );
    assert!(json_files(&machine.outbox()).is_empty());
    assert_eq!(json_files(&machine.outbox().join("dead-letter")).len(), 1);

    let pending = machine.run(&["list", "--pending"]);
    assert!(
        stdout(&pending).contains("dead-letter: id already taken"),
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
fn a_400_drops_the_draft_instead_of_dead_lettering_it() {
    let machine = Machine::new();
    let stub = machine.stub();
    stub.script(vec![Behavior::Error(
        BAD_REQUEST,
        "content is 533 tokens, model window is 512".into(),
    )]);

    let output = machine.run(&[
        "save",
        "--body",
        "prose the tokenizer counts higher than the byte cap suggests",
        "--title",
        "A title",
        "--type",
        "project",
    ]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("model window is 512"),
        "{}",
        stderr(&output)
    );
    assert!(json_files(&machine.outbox()).is_empty());
    assert!(
        json_files(&machine.outbox().join("dead-letter")).is_empty(),
        "a rejection nothing can drain was kept as work to drain"
    );
    assert_eq!(stdout(&machine.run(&["list", "--pending"])), "");
}

#[test]
fn an_over_long_body_is_refused_before_it_reaches_the_outbox() {
    let machine = Machine::new();
    let stub = machine.stub();

    let output = machine.run(&[
        "save",
        "--body",
        &"x".repeat(api::CONTENT_MAX_BYTES + 1),
        "--title",
        "A title",
        "--type",
        "project",
    ]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("syn relate"),
        "{}",
        stderr(&output)
    );
    assert!(json_files(&machine.outbox()).is_empty());
    assert!(json_files(&machine.outbox().join("dead-letter")).is_empty());
    stub.with(|state| assert!(state.recorded.is_empty()));
}

#[test]
fn a_5xx_defers_the_queue_instead_of_dead_lettering_it() {
    let machine = Machine::new();
    let stub = machine.stub();
    stub.script(vec![Behavior::Error(NOT_READY, "unready".into())]);

    let output = machine.run(&[
        "save",
        "--body",
        "server is booting",
        "--title",
        "A title",
        "--type",
        "project",
    ]);

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
fn export_refuses_an_incomplete_dump_and_flushes_the_queue_once_the_server_returns() {
    let machine = Machine::new();
    let queued = machine.run(&[
        "save",
        "--body",
        "saved during the outage",
        "--title",
        "A title",
        "--type",
        "project",
    ]);
    assert!(
        stdout(&queued).contains("queued locally"),
        "{}",
        stdout(&queued)
    );

    let refused = machine.run(&["export", "--workspace", "work"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("1 saves are queued locally")
            && stderr(&refused).contains("the dump would omit them"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        stdout(&refused),
        "",
        "an incomplete dump was written anyway"
    );

    let stub = machine.stub();
    let exported = machine.run(&["export", "--workspace", "work"]);

    assert!(exported.status.success(), "{}", stderr(&exported));
    assert!(json_files(&machine.outbox()).is_empty());
    assert!(
        stdout(&exported).contains("saved during the outage"),
        "the dump omitted the queued save: {}",
        stdout(&exported)
    );
    stub.with(|state| assert_eq!(state.saves().len(), 1));
}

#[test]
fn a_read_says_so_when_it_may_predate_a_queued_save() {
    let machine = Machine::new();
    assert!(
        machine
            .run(&[
                "save",
                "--body",
                "unsendable",
                "--title",
                "A title",
                "--type",
                "project"
            ])
            .status
            .success()
    );

    let read = machine.run(&["recall", "unsendable"]);

    assert!(!read.status.success(), "the read itself needs the server");
    assert!(
        stderr(&read).contains("1 saves are queued locally (")
            && stderr(&read).contains("this read may predate them"),
        "a queue younger than a minute carries no age: {}",
        stderr(&read)
    );
    assert!(
        stderr(&read).contains(&machine.socket().display().to_string()),
        "an unreachable daemon must name the socket it was pointed at: {}",
        stderr(&read)
    );
}

#[test]
fn a_url_that_can_never_reach_a_server_is_refused_where_it_is_set() {
    let machine = Machine::with_config("");

    for bad in ["127.0.0.1:8737", "htttp://127.0.0.1:8737", ""] {
        let output = machine.run(&["config", "set-url", bad]);
        assert!(!output.status.success(), "{bad:?} was accepted");
        assert!(
            stderr(&output).contains("is not a usable server url"),
            "{bad:?}: {}",
            stderr(&output)
        );
    }

    assert!(
        machine
            .run(&["config", "set-url", "https://memory.example/"])
            .status
            .success()
    );
}

#[test]
fn a_read_stays_within_its_flush_budget_when_another_process_holds_the_lock() {
    let machine = Machine::new();
    assert!(
        machine
            .run(&[
                "save",
                "--body",
                "held back",
                "--title",
                "A title",
                "--type",
                "project"
            ])
            .status
            .success()
    );
    let _held = flock(&machine.outbox().join(".lock"));
    let stub = machine.stub();

    let started = std::time::Instant::now();
    let read = machine.run(&["context"]);

    assert!(read.status.success(), "{}", stderr(&read));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(8),
        "the read waited {:?} on a lock it could not get",
        started.elapsed()
    );
    assert!(
        stderr(&read).contains("another syn is flushing the outbox"),
        "{}",
        stderr(&read)
    );
    assert_eq!(json_files(&machine.outbox()).len(), 1);
    stub.with(|state| assert!(state.saves().is_empty()));
}

#[test]
fn an_unreadable_success_is_retried_under_the_same_id_rather_than_dead_lettered() {
    let machine = Machine::new();
    let stub = machine.stub();
    stub.script(vec![Behavior::UndecodableSuccess]);

    let output = machine.run(&[
        "save",
        "--body",
        "committed but unreadable",
        "--title",
        "A title",
        "--type",
        "project",
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("queued locally"),
        "{}",
        stdout(&output)
    );
    assert_eq!(json_files(&machine.outbox()).len(), 1);
    assert!(
        json_files(&machine.outbox().join("dead-letter")).is_empty(),
        "an unreadable 2xx was dead-lettered; the retry would duplicate the memory"
    );

    let retry = machine.run(&["recall", "anything"]);
    assert!(retry.status.success(), "{}", stderr(&retry));
    assert!(json_files(&machine.outbox()).is_empty());
    stub.with(|state| {
        let puts = state.saves();
        assert_eq!(puts.len(), 2, "expected one send plus one retry");
        assert_eq!(puts[0].id(), puts[1].id(), "retry minted a new id");
        assert_eq!(state.memories.len(), 1, "retry created a duplicate memory");
    });
}

#[test]
fn saves_fail_closed_in_a_git_checkout_with_no_matching_rule() {
    let machine = Machine::new();
    let _stub = machine.stub();
    machine.init_git_repo();

    let refused = machine.run(&[
        "save",
        "--body",
        "risky fact",
        "--title",
        "A title",
        "--type",
        "project",
    ]);
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
        "--body",
        "risky fact",
        "--title",
        "A title",
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
        "--body",
        "risky fact",
        "--title",
        "A title",
        "--type",
        "project",
        "--workspace",
        "shared",
    ]);
    assert!(!named_shared.status.success());
    assert!(
        stderr(&named_shared).contains("--scope everywhere"),
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
    let machine = Machine::new();
    let stub = machine.stub();
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

    let output = machine.run(&["recall", "how do we deploy offers"]);

    let printed = stdout(&output);
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(
        lines[0],
        "[m_0000000000000000000001] (work · fresha/offers, 2026-07-14) Staging deploys go through ArgoCD."
    );
    assert_eq!(
        lines[1],
        "[m_0000000000000000000002] (everywhere, 2026-06-02) Prefers Datadog links."
    );
    assert!(lines[2].starts_with("(2 results, "), "{}", lines[2]);
}

#[test]
fn all_workspaces_recall_groups_hits_by_workspace() {
    let machine = Machine::new();
    let stub = machine.stub();
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

    let output = machine.run(&["recall", "anything", "--all-workspaces"]);

    let printed = stdout(&output);
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(lines[0], "## work");
    assert_eq!(
        lines[1],
        "[m_0000000000000000000001] (work, 2026-07-14) work fact"
    );
    assert_eq!(lines[2], "## everywhere");
    assert_eq!(
        lines[3],
        "[m_0000000000000000000003] (everywhere, 2026-07-14) a preference"
    );
    assert!(lines[4].starts_with("(2 results, "));
}

#[test]
fn context_prints_a_digest_and_stays_silent_when_empty() {
    let machine = Machine::new();
    let stub = machine.stub();

    let empty = machine.run(&["context"]);
    assert!(empty.status.success(), "{}", stderr(&empty));
    assert_eq!(stdout(&empty), "");

    stub.with(|state| {
        state.context = Some(
            serde_json::json!({
                "entries": [{"origin": "preference", "id": "m_0000000000000000000002",
                    "content": "Prefers Datadog links", "kind": "user", "scope": "workspace",
                    "tags": [], "pinned": true, "created_at": "2026-06-02T09:00:00Z",
                    "updated_at": "2026-06-02T09:00:00Z"}]
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
fn the_digest_shortens_a_long_untitled_memory_to_its_first_sentence() {
    let machine = Machine::new();
    let stub = machine.stub();

    stub.with(|state| {
        state.context = Some(
            serde_json::json!({
                "entries": [{"origin": "preference", "id": "m_0000000000000000000002",
                    "content": "Never push unsigned commits. Benedikt said so after three \
                                landed unsigned on a PR branch, and re-signing is a rebase.",
                    "kind": "user", "scope": "workspace",
                    "tags": [], "pinned": true, "created_at": "2026-06-02T09:00:00Z",
                    "updated_at": "2026-06-02T09:00:00Z"}]
            })
            .to_string(),
        );
    });
    assert_eq!(
        stdout(&machine.run(&["context"])),
        "## Memory (syn context)\n\
         - [m_0000000000000000000002] Never push unsigned commits…\n\
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
    let machine = Machine::new();
    let stub = machine.stub();

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
            .find(|r| r.method == "memory.forget")
            .expect("forget reached the daemon");
        assert_eq!(deleted.id(), "m_0000000000000000000002");
        assert_eq!(deleted.origin(), "personal");
    });
}

#[test]
fn scope_everywhere_routes_id_commands_away_from_any_workspace() {
    let machine = Machine::new();
    let stub = machine.stub();

    let shown = machine.run(&["show", "m_0000000000000000000002", "--scope", "everywhere"]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    assert!(
        stdout(&shown).contains("(everywhere,"),
        "{}",
        stdout(&shown)
    );

    let pinned = machine.run(&["pin", "m_0000000000000000000002", "--scope", "everywhere"]);
    assert!(pinned.status.success(), "{}", stderr(&pinned));
    assert!(
        stdout(&pinned).contains("(everywhere)"),
        "{}",
        stdout(&pinned)
    );

    let forgotten = machine.run(&[
        "forget",
        "m_0000000000000000000002",
        "--scope",
        "everywhere",
    ]);
    assert!(forgotten.status.success(), "{}", stderr(&forgotten));

    stub.with(|state| {
        for method in ["memory.get", "memory.edit", "memory.forget"] {
            let hit = state
                .recorded
                .iter()
                .find(|call| call.method == method && call.is_preference())
                .unwrap_or_else(|| panic!("{method} did not address the preference store"));
            assert_eq!(hit.id(), "m_0000000000000000000002");
        }
        assert!(
            !state.recorded.iter().any(Recorded::addresses_workspace),
            "an everywhere command touched a workspace store"
        );
    });
}

#[test]
fn move_names_both_ends_and_defaults_its_source_to_the_resolved_workspace() {
    let machine = Machine::new();
    let stub = machine.stub();

    let output = machine.run(&["move", "m_0000000000000000000002", "--to", "personal"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        "moved m_0000000000000000000002 (work → personal)"
    );
    stub.with(|state| {
        let sent = state
            .recorded
            .iter()
            .find(|r| r.method == "memory.move")
            .expect("move reached the daemon");
        assert_eq!(sent.id(), "m_0000000000000000000002");
        let body: serde_json::Value = serde_json::from_str(&sent.params).unwrap();
        assert_eq!(body["from"], serde_json::json!({ "workspace": "work" }));
        assert_eq!(body["to"], serde_json::json!({ "workspace": "personal" }));
    });
}

#[test]
fn moving_into_preferences_reports_the_widened_scope() {
    let machine = Machine::new();
    let stub = machine.stub();
    stub.with(|state| state.move_from_scope = Some("fresha/offers".into()));

    let output = machine.run(&["move", "m_0000000000000000000002", "--to", "everywhere"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        "moved m_0000000000000000000002 (work · fresha/offers → everywhere)"
    );
    assert!(
        stderr(&output).contains("applies everywhere now"),
        "{}",
        stderr(&output)
    );
    stub.with(|state| {
        let sent = state
            .recorded
            .iter()
            .find(|call| call.method == "memory.move")
            .expect("move reached the daemon");
        let body: serde_json::Value = serde_json::from_str(&sent.params).unwrap();
        assert_eq!(body["to"], serde_json::json!("preference"));
    });
}

#[test]
fn move_needs_a_destination_and_refuses_to_name_the_backing_store() {
    let machine = Machine::new();
    let stub = machine.stub();

    let no_target = machine.run(&["move", "m_0000000000000000000002"]);
    assert!(!no_target.status.success());
    assert!(
        stderr(&no_target).contains("--to"),
        "{}",
        stderr(&no_target)
    );

    let reserved = machine.run(&["move", "m_0000000000000000000002", "--to", "shared"]);
    assert!(!reserved.status.success());
    assert!(
        stderr(&reserved).contains("--scope everywhere"),
        "{}",
        stderr(&reserved)
    );

    stub.with(|state| {
        assert!(
            state.recorded.is_empty(),
            "a rejected move still called out"
        )
    });
}

#[test]
fn scope_everywhere_saves_without_naming_a_workspace_or_inheriting_the_repo_scope() {
    let machine = Machine::new();
    let stub = machine.stub();
    std::fs::create_dir_all(machine.cwd.join(".git")).unwrap();

    let output = machine.run(&[
        "save",
        "--body",
        "Benedikt prefers Datadog links over log tailing",
        "--title",
        "A title",
        "--kind",
        "user",
        "--scope",
        "everywhere",
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("saved m_"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("(everywhere)"),
        "{}",
        stdout(&output)
    );
    assert!(json_files(&machine.outbox()).is_empty());

    stub.with(|state| {
        let put = state
            .recorded
            .iter()
            .find(|call| call.method == "memory.save")
            .expect("the preference save reached the daemon");
        assert!(put.is_preference(), "{}", put.params);
        let body: serde_json::Value = serde_json::from_str(&put.params).unwrap();
        assert_eq!(body["kind"], "user");
        assert_eq!(
            body["scope"], "workspace",
            "an everywhere save inherited a repo scope: {body}"
        );
    });
}

#[test]
fn a_queued_everywhere_save_replays_as_one() {
    let machine = Machine::new();

    let queued = machine.run(&[
        "save",
        "--body",
        "prefers oat milk",
        "--title",
        "A title",
        "--kind",
        "user",
        "--scope",
        "everywhere",
    ]);
    assert!(
        stdout(&queued).contains("queued locally"),
        "{}",
        stdout(&queued)
    );

    let pending = machine.run(&["list", "--pending"]);
    assert!(
        stdout(&pending).contains("(everywhere) queued"),
        "{}",
        stdout(&pending)
    );

    let untouched = machine.run(&["list", "--pending", "--reassign", "personal"]);
    assert!(
        stdout(&untouched).contains("left 1 everywhere saves alone"),
        "{}",
        stdout(&untouched)
    );

    let stub = machine.stub();
    let flushed = machine.run(&["context"]);
    assert!(flushed.status.success(), "{}", stderr(&flushed));
    assert!(json_files(&machine.outbox()).is_empty());
    stub.with(|state| {
        let put = state.saves()[0];
        assert!(put.is_preference(), "{}", put.params);
    });
}

#[test]
fn workspace_map_writes_a_path_rule_that_saves_resolve_against() {
    let machine = Machine::with_config("transport = \"daemon\"\n");
    let _stub = machine.stub();
    machine.init_git_repo();

    let refused = machine.run(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);
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

    let saved = machine.run(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);
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

#[test]
fn routing_free_commands_work_with_git_absent_from_path() {
    let machine = Machine::with_config("token = \"t\"\n");

    let set = machine.run_without_git(&["config", "set-token", "abc123"]);
    assert!(set.status.success(), "{}", stderr(&set));

    let pending = machine.run_without_git(&["list", "--pending"]);
    assert!(pending.status.success(), "{}", stderr(&pending));
}

#[test]
fn explicit_workspace_and_scope_save_succeeds_without_git() {
    let machine = Machine::new();
    let _stub = machine.stub();

    let saved = machine.run_without_git(&[
        "save",
        "--body",
        "a fact",
        "--title",
        "A title",
        "--type",
        "project",
        "--workspace",
        "work",
        "--scope",
        "acme/repo",
    ]);
    assert!(saved.status.success(), "{}", stderr(&saved));
    assert!(
        stdout(&saved).contains("(work · acme/repo)"),
        "{}",
        stdout(&saved)
    );
}

#[test]
fn a_save_that_reaches_everywhere_needs_neither_git_nor_a_workspace() {
    let machine = Machine::new();
    let _stub = machine.stub();

    let saved = machine.run_without_git(&[
        "save",
        "--body",
        "Benedikt wants the failing test output before a fix",
        "--title",
        "A title",
        "--kind",
        "feedback",
        "--scope",
        "everywhere",
    ]);
    assert!(saved.status.success(), "{}", stderr(&saved));
    assert!(
        stdout(&saved).contains("(everywhere)"),
        "{}",
        stdout(&saved)
    );

    let contradiction = machine.run(&[
        "save",
        "--body",
        "a fact",
        "--title",
        "A title",
        "--kind",
        "feedback",
        "--scope",
        "everywhere",
        "--workspace",
        "work",
    ]);
    assert!(!contradiction.status.success());
    assert!(
        stderr(&contradiction).contains("cannot take --workspace"),
        "{}",
        stderr(&contradiction)
    );
}

#[test]
fn the_retired_remember_command_names_both_reaches_and_writes_nothing() {
    let machine = Machine::new();
    let stub = machine.stub();

    let refused = machine.run(&["remember", "the clients are web, android and ios"]);

    assert!(!refused.status.success());
    let err = stderr(&refused);
    assert!(err.contains("--scope everywhere"), "{err}");
    assert!(err.contains("--scope workspace"), "{err}");
    assert!(
        err.contains("the clients are web, android and ios"),
        "the fact is echoed back so nothing has to be retyped: {err}"
    );
    assert!(json_files(&machine.outbox()).is_empty());
    stub.with(|state| {
        assert!(
            state.recorded.is_empty(),
            "a retired command reached the server"
        )
    });
}

#[test]
fn kind_decision_is_stored_as_the_project_kind_the_server_knows() {
    let machine = Machine::new();
    let stub = machine.stub();

    let saved = machine.run(&[
        "save",
        "--body",
        "staging deploys go through ArgoCD",
        "--title",
        "A title",
        "--kind",
        "decision",
        "--scope",
        "workspace",
        "--workspace",
        "work",
    ]);
    assert!(saved.status.success(), "{}", stderr(&saved));
    stub.with(|state| {
        let body: serde_json::Value = serde_json::from_str(&state.saves()[0].params).unwrap();
        assert_eq!(body["kind"], "project");
    });
}

#[test]
fn a_save_needing_inference_errors_rather_than_defaulting_without_git() {
    let machine = Machine::new();
    let _stub = machine.stub();

    let refused = machine.run_without_git(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("could not run git"),
        "{}",
        stderr(&refused)
    );
    assert!(
        json_files(&machine.outbox()).is_empty(),
        "a hard git failure must not queue a save"
    );
}

#[test]
fn map_org_round_trips_through_list_and_keeps_the_config_private() {
    let machine = Machine::new();
    let _stub = machine.stub();

    let mapped = machine.run(&["workspace", "map-org", "acme", "acme-ws"]);
    assert!(mapped.status.success(), "{}", stderr(&mapped));
    assert!(stdout(&mapped).contains("acme"), "{}", stdout(&mapped));
    assert!(stdout(&mapped).contains("acme-ws"), "{}", stdout(&mapped));
    assert!(
        stderr(&mapped).contains("existing memories do not move"),
        "{}",
        stderr(&mapped)
    );

    let listed = machine.run(&["workspace", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(
        stdout(&listed).contains("acme -> acme-ws"),
        "{}",
        stdout(&listed)
    );

    let config_path = machine.home.path().join("config").join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    assert!(config.contains("[[org_rules]]"), "{config}");
    assert!(config.contains("org = \"acme\""), "{config}");

    let mode = std::fs::metadata(&config_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn re_mapping_an_org_under_a_different_case_replaces_the_old_rule() {
    let machine = Machine::with_config("transport = \"daemon\"\n");
    let _stub = machine.stub();

    let first = machine.run(&["workspace", "map-org", "Acme", "one"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let second = machine.run(&["workspace", "map-org", "acme", "two"]);
    assert!(second.status.success(), "{}", stderr(&second));

    let config_path = machine.home.path().join("config").join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    assert_eq!(config.matches("[[org_rules]]").count(), 1, "{config}");
    assert!(config.contains("workspace = \"two\""), "{config}");

    machine.init_git_repo_with_origin("git@github.com:Acme/widgets.git");
    let saved = machine.run(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);
    assert!(saved.status.success(), "{}", stderr(&saved));
    assert!(
        stdout(&saved).contains("(two · Acme/widgets)"),
        "{}",
        stdout(&saved)
    );
}

#[test]
fn a_present_but_unusable_repo_refuses_the_save_instead_of_defaulting() {
    let machine = Machine::new();
    let _stub = machine.stub();
    std::fs::write(machine.cwd.join(".git"), "gitdir: /nonexistent/path.git\n").unwrap();

    let refused = machine.run(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains(machine.cwd.to_str().unwrap()),
        "{}",
        stderr(&refused)
    );
    assert!(
        json_files(&machine.outbox()).is_empty(),
        "a repo git cannot use must not queue a save into the default workspace"
    );
}

#[test]
fn an_org_rule_routes_an_unmapped_repo_and_its_worktree() {
    let machine = Machine::with_config("transport = \"daemon\"\n");
    let _stub = machine.stub();
    machine.init_git_repo_with_origin("git@github.com:acme/widgets.git");

    let refused = machine.run(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);
    assert!(!refused.status.success(), "{}", stdout(&refused));

    let mapped = machine.run(&["workspace", "map-org", "acme", "acme-ws"]);
    assert!(mapped.status.success(), "{}", stderr(&mapped));

    let saved = machine.run(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);
    assert!(saved.status.success(), "{}", stderr(&saved));
    assert!(
        stdout(&saved).contains("(acme-ws · acme/widgets)"),
        "{}",
        stdout(&saved)
    );

    let worktree = machine.home.path().join("wt");
    let status = Command::new("git")
        .arg("-C")
        .arg(&machine.cwd)
        .args([
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "-b",
            "wt",
        ])
        .status()
        .expect("git worktree add");
    assert!(status.success(), "git worktree add failed");

    let output = Command::new(env!("CARGO_BIN_EXE_syn"))
        .args([
            "save", "--body", "a fact", "--title", "A title", "--type", "project",
        ])
        .current_dir(&worktree)
        .env("SYNAPSE_CONFIG_DIR", machine.home.path().join("config"))
        .env("SYNAPSE_STATE_DIR", machine.state_dir())
        .env("HOME", machine.home.path())
        .output()
        .expect("run syn in worktree");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("(acme-ws · acme/widgets)"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_nested_path_rule_still_beats_an_org_rule_for_the_same_repo() {
    let machine = Machine::with_config("transport = \"daemon\"\n");
    let _stub = machine.stub();
    machine.init_git_repo_with_origin("git@github.com:acme/widgets.git");

    let org_mapped = machine.run(&["workspace", "map-org", "acme", "acme-ws"]);
    assert!(org_mapped.status.success(), "{}", stderr(&org_mapped));

    let path_mapped = machine.run(&[
        "workspace",
        "map",
        machine.cwd.to_str().unwrap(),
        "client-a-ws",
    ]);
    assert!(path_mapped.status.success(), "{}", stderr(&path_mapped));

    let saved = machine.run(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);
    assert!(saved.status.success(), "{}", stderr(&saved));
    assert!(
        stdout(&saved).contains("(client-a-ws · acme/widgets)"),
        "{}",
        stdout(&saved)
    );
}

#[test]
fn an_origin_less_repo_falls_through_org_rules_without_crashing() {
    let machine = Machine::with_config("transport = \"daemon\"\n");
    let _stub = machine.stub();
    machine.init_git_repo();

    let mapped = machine.run(&["workspace", "map-org", "acme", "acme-ws"]);
    assert!(mapped.status.success(), "{}", stderr(&mapped));

    let refused = machine.run(&[
        "save", "--body", "a fact", "--title", "A title", "--type", "project",
    ]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("no workspace rule matches"),
        "{}",
        stderr(&refused)
    );
}
