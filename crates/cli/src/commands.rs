use std::cell::OnceCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use api::{
    ExportDoc, MemoryDto, MoveBody, Origin, PatchMemoryBody, PutMemoryBody, PutPreferenceBody,
    SearchResponse, limits,
};
use api_client::SynapseApiClient;
use daemon_client::{DaemonClient, DaemonConfig, ScopedOrg};
use domain::MemoryId;

use crate::args::{
    Cli, Command, ConfigCommand, ContextArgs, EditArgs, IdArgs, ImportArgs, LinkPairArgs,
    LinksArgs, ListArgs, MoveArgs, RecallArgs, RetiredArgs, SCOPE_EVERYWHERE, SaveArgs, ShowArgs,
    StoreArgs, SupersedeArgs, SyncArgs, WorkspaceArgs, WorkspaceCommand,
};
use crate::client::Client;
use crate::config::{Config, OrgRule, Transport, WorkspaceRule};
use crate::git::GitFacts;
use crate::outbox::{FlushReport, Outbox, PendingSave, SaveTarget, now_millis};
use crate::output;
use crate::resolve;

const READ_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const BULK_TIMEOUT: Duration = Duration::from_secs(600);
/// Socket timeouts on the daemon transport sit above the daemon's own per-request
/// deadlines (30s, 600s for import), so the client always receives the daemon's verdict
/// instead of timing out first on an operation that then succeeds.
const DAEMON_RPC_TIMEOUT: Duration = Duration::from_secs(35);
const DAEMON_BULK_TIMEOUT: Duration = Duration::from_secs(610);
/// One total budget for the pre-read flush, so a backlog cannot push a read past the
/// session hook's ten seconds however many items are queued.
const FLUSH_BUDGET: Duration = Duration::from_secs(2);
const FLUSH_SEND_TIMEOUT: Duration = Duration::from_secs(1);

pub fn run(cli: Cli) -> Result<(), String> {
    let config = Config::load()?;
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;
    let ctx = Context {
        config,
        cwd,
        facts: OnceCell::new(),
    };
    match cli.command {
        Command::Save(args) => save(&ctx, args),
        Command::Remember(args) => retired_remember(args),
        Command::Recall(args) => recall(&ctx, args),
        Command::Context(args) => context(&ctx, args),
        Command::Edit(args) => edit(&ctx, args),
        Command::Forget(args) => forget(&ctx, args),
        Command::Move(args) => move_memory(&ctx, args),
        Command::List(args) => list(&ctx, args),
        Command::Show(args) => show(&ctx, args),
        Command::Links(args) => links(&ctx, args),
        Command::Relate(args) => link_pair(&ctx, args, domain::Relation::Relation),
        Command::Support(args) => link_pair(&ctx, args, domain::Relation::Support),
        Command::Contradict(args) => link_pair(&ctx, args, domain::Relation::Contradiction),
        Command::Supersede(args) => supersede(&ctx, args),
        Command::Unlink(args) => unlink(&ctx, args),
        Command::Pin(args) => set_pinned(&ctx, args, true),
        Command::Unpin(args) => set_pinned(&ctx, args, false),
        Command::Workspace(command) => workspace(&ctx, command),
        Command::Export(args) => export(&ctx, args),
        Command::Import(args) => import(&ctx, args),
        Command::Config(command) => set_config(ctx.config, command),
        Command::Setup => setup(&ctx),
        Command::Sync(args) => sync(&ctx, args),
        Command::Status => status(&ctx),
        Command::Daemon(command) => crate::daemon::run(command),
    }
}

struct Context {
    config: Config,
    cwd: PathBuf,
    /// Lazy and memoized: probing git is skipped entirely by commands that never
    /// resolve a workspace or scope, and never repeated within one invocation.
    facts: OnceCell<Result<Option<GitFacts>, String>>,
}

impl Context {
    fn client(&self, timeout: Duration) -> Result<Client, String> {
        match self.config.transport() {
            Transport::Http => Ok(Client::Http(self.http_client(timeout)?)),
            Transport::Daemon => Ok(Client::Daemon(self.daemon_client(timeout, true)?)),
        }
    }

    fn http_client(&self, timeout: Duration) -> Result<SynapseApiClient, String> {
        SynapseApiClient::new(self.config.url(), self.config.token()?, timeout)
            .map_err(|e| e.to_string())
    }

    /// Spawn-on-demand: a dead daemon is started detached and polled until it answers.
    fn daemon_client(&self, timeout: Duration, spawn: bool) -> Result<DaemonClient, String> {
        let dir = daemon_client::state_dir()?;
        let timeout = if timeout >= BULK_TIMEOUT {
            DAEMON_BULK_TIMEOUT
        } else {
            DAEMON_RPC_TIMEOUT
        };
        let client = DaemonClient::new(daemon_client::socket_path(&dir), timeout);
        if spawn {
            daemon_client::ensure_running(&client, &dir)?;
        }
        Ok(client)
    }

    fn facts(&self) -> Result<Option<&GitFacts>, String> {
        match self.facts.get_or_init(|| GitFacts::discover(&self.cwd)) {
            Ok(facts) => Ok(facts.as_ref()),
            Err(e) => Err(e.clone()),
        }
    }

    fn workspace(&self, flag: Option<&str>, fail_closed: bool) -> Result<String, String> {
        if let Some(name) = flag {
            return resolve::validate_workspace(name);
        }
        let facts = self.facts()?;
        resolve::resolve_workspace(&self.config, None, &self.cwd, facts, fail_closed)
    }

    fn scope(&self, flag: Option<&str>) -> Result<resolve::ResolvedScope, String> {
        if let Some(explicit) = resolve::explicit_scope(flag)? {
            return Ok(explicit);
        }
        let facts = self.facts()?;
        Ok(resolve::scope_from_facts(facts))
    }
}

/// Read commands drain the outbox first so a queued save becomes recallable at the first
/// opportunity; an outage here must not fail the read, but it must not pass unmentioned
/// either — whatever stays queued is missing from what the read is about to print.
/// The flush targets whichever transport is active, so saves queued under one transport
/// still land after a switch.
fn flush_before_read(ctx: &Context) {
    let Ok(outbox) = Outbox::open() else {
        return;
    };
    let flushed = ctx
        .client(FLUSH_SEND_TIMEOUT)
        .and_then(|client| outbox.flush(&client, Some(FLUSH_BUDGET)));
    let report = match flushed {
        Ok(report) => report,
        Err(e) => {
            eprintln!(
                "note: the local queue could not be flushed ({e}); this read may predate anything in it — see: syn list --pending"
            );
            return;
        }
    };
    for (id, failure) in &report.dead_lettered {
        eprintln!("note: {id} moved to dead-letter: {failure}");
    }
    for (id, failure) in &report.rejected {
        eprintln!("note: {id} dropped as invalid: {failure}");
    }
    if report.still_queued > 0 {
        let reason = report.deferred.as_deref().unwrap_or("still unsent");
        eprintln!(
            "note: {} ({reason}); this read may predate them — see: syn list --pending",
            backlog_note(&report)
        );
    }
}

/// A count alone reads as a momentary outage, which under a minute is what it usually is.
/// Past that, the age of the oldest item is what names a queue that has stopped draining.
fn backlog_note(report: &FlushReport) -> String {
    let count = report.still_queued;
    let now = now_millis();
    match report.oldest_queued_at {
        Some(queued_at) if now.saturating_sub(queued_at) >= 60_000 => format!(
            "{count} saves are queued locally, oldest {}",
            age(now, queued_at)
        ),
        _ => format!("{count} saves are queued locally"),
    }
}

fn save(ctx: &Context, args: SaveArgs) -> Result<(), String> {
    let kind = wire_kind(&args.kind);
    let target = if args.scope.as_deref() == Some(SCOPE_EVERYWHERE) {
        if args.workspace.is_some() {
            return Err(
                "--scope everywhere applies in every workspace, so it cannot take --workspace"
                    .to_string(),
            );
        }
        SaveTarget::Preference {
            body: PutPreferenceBody {
                content: args.body,
                title: Some(args.title.clone()),
                kind,
                tags: args.tags,
                importance: args.importance,
            },
        }
    } else {
        let workspace = ctx.workspace(args.workspace.as_deref(), true)?;
        let scope = ctx.scope(args.scope.as_deref())?;
        if let Some(note) = &scope.note {
            eprintln!("note: {note}");
        }
        SaveTarget::Memory {
            workspace,
            body: PutMemoryBody {
                content: args.body,
                title: Some(args.title),
                kind,
                scope: scope.scope,
                tags: args.tags,
                importance: args.importance,
            },
        }
    };
    queue_and_flush(ctx, target)
}

/// `decision` is the CLI's name for what the store calls a `project` memory: `--scope project`
/// already means "infer the repo from git", so one word could not carry both jobs.
fn wire_kind(kind: &str) -> String {
    match kind {
        "decision" => "project".to_string(),
        other => other.to_string(),
    }
}

fn cli_kind(kind: &str) -> &str {
    match kind {
        "project" => "decision",
        other => other,
    }
}

fn retired_remember(args: RetiredArgs) -> Result<(), String> {
    let fact = args
        .rest
        .iter()
        .find(|word| !word.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| "<fact>".to_string());
    let quoted = format!("{fact:?}");
    Err(format!(
        "`syn remember` is gone — say how far the fact reaches:\n  \
         syn save {quoted} --kind feedback --scope everywhere   every workspace, every project\n  \
         syn save {quoted} --kind decision --scope workspace    this workspace's business\n  \
         syn save {quoted} --kind decision                      this repo alone"
    ))
}

/// The outbox is written before the first send, so a reply lost in flight replays
/// against the same id and the idempotent PUT collapses it.
fn queue_and_flush(ctx: &Context, target: SaveTarget) -> Result<(), String> {
    check_before_queueing(&target)?;
    let id = MemoryId::generate().to_string();
    let where_to = target.label();
    let item = PendingSave {
        id: id.clone(),
        queued_at: now_millis(),
        target,
        failure: None,
    };
    let outbox = Outbox::open()?;
    outbox.enqueue(&item)?;
    // The item is durable before the client exists: a backend that cannot even be
    // constructed (daemon spawn failure, missing token) leaves it queued, not lost.
    let client = match ctx.client(WRITE_TIMEOUT) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("note: {e}");
            println!("queued {id} ({where_to}) — queued locally, not yet recallable");
            return Ok(());
        }
    };
    let report = outbox.flush(&client, None)?;
    if report.sent.contains(&id) {
        println!("saved {id} ({where_to})");
        report_candidates(&report, &id);
        return Ok(());
    }
    if let Some((_, failure)) = report.rejected.iter().find(|(rejected, _)| *rejected == id) {
        return Err(failure.clone());
    }
    if let Some((_, failure)) = report.dead_lettered.iter().find(|(dead, _)| *dead == id) {
        return Err(format!("{failure} (see: syn list --pending)"));
    }
    report_backlog(&report);
    println!("queued {id} ({where_to}) — queued locally, not yet recallable");
    Ok(())
}

/// Names the stored memories that closely resemble the one just written, so the writer can
/// record the relation it alone can name. Nothing is linked here.
fn report_candidates(report: &FlushReport, id: &str) {
    let Some((_, candidates)) = report.candidates.iter().find(|(sent, _)| sent == id) else {
        return;
    };
    eprintln!("similar memories already stored:");
    for candidate in candidates {
        eprintln!(
            "  {} ({:.2}) {}",
            candidate.id, candidate.similarity, candidate.title
        );
    }
    eprintln!("link one with: syn relate|support|contradict|supersede {id} <id>");
}

/// Rejected here, a bad draft never reaches the outbox, so it cannot dead-letter. The daemon
/// still checks the token window, which needs a tokenizer this crate does not link.
fn check_before_queueing(target: &SaveTarget) -> Result<(), String> {
    let (content, title, tags) = match target {
        SaveTarget::Memory { body, .. } => (&body.content, &body.title, &body.tags),
        SaveTarget::Preference { body } => (&body.content, &body.title, &body.tags),
    };
    limits::content(content)?;
    if let Some(title) = title {
        limits::title(title, false)?;
    }
    limits::tags(tags)
}

fn report_backlog(report: &FlushReport) {
    if let Some(reason) = &report.deferred {
        eprintln!("note: {reason}");
    }
    for (id, failure) in &report.dead_lettered {
        eprintln!("note: {id} moved to dead-letter: {failure}");
    }
    for (id, failure) in &report.rejected {
        eprintln!("note: {id} dropped as invalid: {failure}");
    }
}

fn recall(ctx: &Context, args: RecallArgs) -> Result<(), String> {
    let client = ctx.client(READ_TIMEOUT)?;
    flush_before_read(ctx);
    let workspace = ctx.workspace(args.workspace.as_deref(), false)?;
    let scope = ctx.scope(args.project.as_deref())?;
    let started = Instant::now();
    let response = client.search(
        &workspace,
        &args.query,
        scope.project(),
        args.limit,
        args.all_workspaces,
        args.links_in_scope,
    )?;
    let elapsed = started.elapsed().as_millis();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let count = match &response {
        SearchResponse::Flat { hits } => {
            for hit in hits {
                let _ = writeln!(out, "{}", output::hit_line(hit, args.detail.detail));
            }
            hits.len()
        }
        SearchResponse::Grouped { groups } => {
            for group in groups {
                let _ = writeln!(out, "## {}", output::store_label(&group.origin));
                for hit in &group.hits {
                    let _ = writeln!(out, "{}", output::hit_line(hit, args.detail.detail));
                }
            }
            groups.iter().map(|group| group.hits.len()).sum()
        }
    };
    let _ = writeln!(out, "({count} results, {elapsed}ms)");
    Ok(())
}

fn context(ctx: &Context, args: ContextArgs) -> Result<(), String> {
    let client = ctx.client(READ_TIMEOUT)?;
    flush_before_read(ctx);
    let workspace = ctx.workspace(args.workspace.as_deref(), false)?;
    let scope = ctx.scope(args.project.as_deref())?;
    let digest = client.context(&workspace, scope.project())?;
    if let Some(text) = output::digest(&digest) {
        println!("{text}");
    }
    Ok(())
}

/// Which store an id-based command acts on. `--preference` skips workspace
/// resolution entirely — preferences belong to no workspace.
fn resolve_target(ctx: &Context, store: &StoreArgs) -> Result<Origin, String> {
    if store.everywhere() {
        Ok(Origin::Preference)
    } else {
        ctx.workspace(store.workspace.as_deref(), false)
            .map(Origin::Workspace)
    }
}

fn patch(
    client: &Client,
    target: &Origin,
    id: &str,
    body: &PatchMemoryBody,
) -> Result<MemoryDto, String> {
    match target {
        Origin::Preference => client.edit_preference(id, body),
        Origin::Workspace(workspace) => client.edit(workspace, id, body),
    }
}

fn edit(ctx: &Context, args: EditArgs) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    let target = resolve_target(ctx, &args.store)?;

    if let Some(relation) = args.relation.as_deref() {
        let ty = args
            .type_
            .as_deref()
            .ok_or("retyping requires --type <relation|support|contradiction|supersession>")?;
        let workspace = match &target {
            Origin::Preference => {
                return Err("syn edit --relation acts on a workspace memory".into());
            }
            Origin::Workspace(workspace) => workspace.to_string(),
        };
        client.retype_link(&workspace, &args.id, relation, ty)?;
        println!("retyped {} ↔ {relation} ({ty})", args.id);
    }

    let wants_memory_edit =
        args.body.is_some() || args.importance.is_some() || args.title.is_some();
    if wants_memory_edit {
        let body = PatchMemoryBody {
            content: args.body,
            title: args.title,
            importance: args.importance,
            ..PatchMemoryBody::default()
        };
        let memory = patch(&client, &target, &args.id, &body)?;
        println!("updated {} ({})", memory.id, output::store_label(&target));
    }

    if args.relation.is_none() && !wants_memory_edit {
        return Err(
            "syn edit needs --content (or --title/--importance), or --relation/--type to retype a link"
                .into(),
        );
    }
    Ok(())
}

/// `syn relate/support/contradict` — a bidirectional typed edge.
fn link_pair(ctx: &Context, args: LinkPairArgs, relation: domain::Relation) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    let target = resolve_target(ctx, &args.store)?;
    let workspace = match &target {
        Origin::Preference => return Err("links act on workspace memories".into()),
        Origin::Workspace(workspace) => workspace.to_string(),
    };
    client.link(&workspace, &args.a, &args.b, relation.as_str())?;
    println!("linked {} ↔ {} ({})", args.a, args.b, relation);
    Ok(())
}

/// `syn supersede --old <B> --new <A>` — directed; cycle-guarded, de-ranks the superseded memory.
fn supersede(ctx: &Context, args: SupersedeArgs) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    let target = resolve_target(ctx, &args.store)?;
    let workspace = match &target {
        Origin::Preference => return Err("links act on workspace memories".into()),
        Origin::Workspace(workspace) => workspace.to_string(),
    };
    client.link(&workspace, &args.new, &args.old, "supersession")?;
    println!("superseded {} by {}", args.old, args.new);
    Ok(())
}

/// `syn unlink <A> <B>` — remove whatever edge(s) exist between the pair.
fn unlink(ctx: &Context, args: LinkPairArgs) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    let target = resolve_target(ctx, &args.store)?;
    let workspace = match &target {
        Origin::Preference => return Err("links act on workspace memories".into()),
        Origin::Workspace(workspace) => workspace.to_string(),
    };
    client.unlink(&workspace, &args.a, &args.b)?;
    println!("unlinked {} ↔ {}", args.a, args.b);
    Ok(())
}

fn forget(ctx: &Context, args: IdArgs) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    let target = resolve_target(ctx, &args.store)?;
    match &target {
        Origin::Preference => client.forget_preference(&args.id),
        Origin::Workspace(workspace) => client.forget(workspace, &args.id),
    }?;
    println!("forgot {} ({})", args.id, output::store_label(&target));
    Ok(())
}

fn move_memory(ctx: &Context, args: MoveArgs) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    let from = resolve_target(ctx, &args.store)?;
    let to = match args.to.as_deref() {
        Some(SCOPE_EVERYWHERE) | None => Origin::Preference,
        Some(name) => Origin::Workspace(resolve::validate_workspace(name)?),
    };
    let body = MoveBody {
        from: from.clone(),
        to: to.clone(),
    };
    let response = client.move_memory(&args.id, &body)?;
    let source = output::place(&from, &response.from_scope);
    if !response.moved {
        println!(
            "nothing moved: {} is already in {source}",
            response.memory.id
        );
        return Ok(());
    }
    if response.from_scope != response.memory.scope {
        eprintln!(
            "note: scope widened from {} to workspace; it applies everywhere now",
            response.from_scope
        );
    }
    if response.links_dropped > 0 {
        eprintln!(
            "note: dropped {} link(s); links do not cross stores, so re-link it in {}",
            response.links_dropped,
            output::place(&to, &response.memory.scope)
        );
    }
    println!(
        "moved {} ({source} → {})",
        response.memory.id,
        output::place(&to, &response.memory.scope)
    );
    Ok(())
}

fn set_pinned(ctx: &Context, args: IdArgs, pinned: bool) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    let target = resolve_target(ctx, &args.store)?;
    let body = PatchMemoryBody {
        pinned: Some(pinned),
        ..PatchMemoryBody::default()
    };
    let memory = patch(&client, &target, &args.id, &body)?;
    let verb = if pinned { "pinned" } else { "unpinned" };
    println!("{verb} {} ({})", memory.id, output::store_label(&target));
    Ok(())
}

fn list(ctx: &Context, args: ListArgs) -> Result<(), String> {
    if args.pending {
        return pending(args);
    }
    let client = ctx.client(READ_TIMEOUT)?;
    flush_before_read(ctx);
    let target = resolve_target(ctx, &args.store)?;
    let memories = match &target {
        Origin::Preference => client.list_preferences(),
        Origin::Workspace(workspace) => client.list(workspace),
    }?;
    for memory in memories {
        println!(
            "{}",
            output::list_line(&target, &memory, args.detail.detail)
        );
    }
    Ok(())
}

fn pending(args: ListArgs) -> Result<(), String> {
    let outbox = Outbox::open()?;
    if let Some(workspace) = &args.reassign {
        let workspace = resolve::validate_workspace(workspace)?;
        let (moved, skipped) = outbox.reassign(&workspace, args.id.as_deref())?;
        println!("reassigned {moved} pending saves to {workspace}");
        if skipped > 0 {
            println!("left {skipped} everywhere saves alone; they belong to no workspace");
        }
        return Ok(());
    }
    if args.discard {
        let discarded = outbox.discard(args.id.as_deref())?;
        println!("discarded {discarded} pending saves");
        return Ok(());
    }
    let now = now_millis();
    for (_, item) in outbox.pending()? {
        println!(
            "[{}] ({}) queued {}",
            item.id,
            item.target.label(),
            age(now, item.queued_at)
        );
    }
    for (_, item) in outbox.dead_letters()? {
        println!(
            "[{}] ({}) dead-letter: {}",
            item.id,
            item.target.label(),
            item.failure.as_deref().unwrap_or("unknown failure")
        );
    }
    Ok(())
}

fn age(now: u64, queued_at: u64) -> String {
    let seconds = now.saturating_sub(queued_at) / 1000;
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn show(ctx: &Context, args: ShowArgs) -> Result<(), String> {
    let client = ctx.client(READ_TIMEOUT)?;
    flush_before_read(ctx);
    let target = resolve_target(ctx, &args.store)?;
    let memory = match &target {
        Origin::Preference => client.get_preference(&args.id),
        Origin::Workspace(workspace) => client.get(workspace, &args.id),
    }?;
    println!(
        "{}",
        output::memory_line(&target, &memory, args.detail.detail)
    );
    if !memory.tags.is_empty() {
        println!("tags: {}", memory.tags.join(", "));
    }
    println!(
        "kind: {}  pinned: {}  importance: {}  created: {}",
        cli_kind(&memory.kind),
        memory.pinned,
        memory.importance,
        memory.created_at
    );
    Ok(())
}

fn links(ctx: &Context, args: LinksArgs) -> Result<(), String> {
    let client = ctx.client(READ_TIMEOUT)?;
    flush_before_read(ctx);
    if args.store.everywhere() {
        return Err("syn links acts on a workspace memory; drop --scope everywhere".into());
    }
    let workspace = ctx.workspace(args.store.workspace.as_deref(), false)?;
    let graph = client.links(&workspace, &args.id, args.depth)?;
    let json = serde_json::to_string_pretty(&graph).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

fn workspace(ctx: &Context, command: WorkspaceCommand) -> Result<(), String> {
    match command {
        WorkspaceCommand::List => {
            let client = ctx.client(READ_TIMEOUT)?;
            let default = ctx.config.default_workspace.as_deref();
            for workspace in client.workspaces()? {
                let marker = if Some(workspace.as_str()) == default {
                    " (default)"
                } else {
                    ""
                };
                println!("{workspace}{marker}");
            }
            // Ladder order: path rules beat org rules, so they print first.
            for rule in &ctx.config.workspace_rules {
                println!("{} -> {} (path rule)", rule.path, rule.workspace);
            }
            for rule in &ctx.config.org_rules {
                println!("{} -> {} (org rule)", rule.org, rule.workspace);
            }
            Ok(())
        }
        WorkspaceCommand::Create { name } => {
            let name = resolve::validate_workspace(&name)?;
            let client = ctx.client(WRITE_TIMEOUT)?;
            let created = client.create_workspace(&name)?;
            println!("workspace {created} ready");
            Ok(())
        }
        WorkspaceCommand::Use { name } => {
            let name = resolve::validate_workspace(&name)?;
            let mut config = ctx.config.clone();
            config.default_workspace = Some(name.clone());
            config.save()?;
            println!("default workspace set to {name}");
            Ok(())
        }
        WorkspaceCommand::Map { path, name } => {
            let name = resolve::validate_workspace(&name)?;
            let path = path
                .canonicalize()
                .map_err(|e| format!("cannot resolve {}: {e}", path.display()))?
                .to_string_lossy()
                .to_string();
            let mut config = ctx.config.clone();
            config.workspace_rules.retain(|rule| rule.path != path);
            config.workspace_rules.push(WorkspaceRule {
                path: path.clone(),
                workspace: name.clone(),
            });
            config.save()?;
            println!("{path} now resolves to workspace {name}");
            Ok(())
        }
        WorkspaceCommand::MapOrg { org, name } => {
            let org = resolve::validate_org(&org)?;
            let name = resolve::validate_workspace(&name)?;
            let mut config = ctx.config.clone();
            config
                .org_rules
                .retain(|rule| !rule.org.eq_ignore_ascii_case(&org));
            config.org_rules.push(OrgRule {
                org: org.clone(),
                workspace: name.clone(),
            });
            config.save()?;
            println!("org {org} now resolves to workspace {name}");
            eprintln!(
                "note: existing memories do not move; see \"Routing migration\" in the README before deleting a path rule"
            );
            Ok(())
        }
    }
}

fn export(ctx: &Context, args: WorkspaceArgs) -> Result<(), String> {
    let client = ctx.client(BULK_TIMEOUT)?;
    drain_before_export(&client)?;
    let target = resolve_target(ctx, &args.store)?;
    let doc = match &target {
        Origin::Preference => client.export_preferences(),
        Origin::Workspace(workspace) => client.export(workspace),
    }?;
    let json = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

/// A dump is only a backup once this machine's queue is on the server, so an unflushable
/// backlog fails the export rather than writing a quietly incomplete file.
fn drain_before_export(client: &Client) -> Result<(), String> {
    let outbox = Outbox::open()?;
    let report = outbox.flush(client, None)?;
    report_backlog(&report);
    if report.still_queued > 0 {
        return Err(format!(
            "{} — the dump would omit them, so nothing was written (see: syn list --pending)",
            backlog_note(&report)
        ));
    }
    let dead_lettered = outbox.dead_letters()?.len();
    if dead_lettered > 0 {
        eprintln!(
            "note: {dead_lettered} dead-lettered saves never reached the server and are not in this dump (see: syn list --pending)"
        );
    }
    Ok(())
}

fn import(ctx: &Context, args: ImportArgs) -> Result<(), String> {
    let client = ctx.client(BULK_TIMEOUT)?;
    let target = resolve_target(ctx, &args.store)?;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("cannot read dump from stdin: {e}"))?;
    let doc: ExportDoc =
        serde_json::from_str(&input).map_err(|e| format!("invalid dump on stdin: {e}"))?;
    let report = match &target {
        Origin::Preference => client.import_preferences(args.merge, &doc),
        Origin::Workspace(workspace) => client.import(workspace, args.merge, &doc),
    }?;
    println!(
        "imported {} memories into {} ({} unchanged)",
        report.imported,
        output::store_label(&target),
        report.unchanged
    );
    Ok(())
}

fn set_config(mut config: Config, command: ConfigCommand) -> Result<(), String> {
    match command {
        ConfigCommand::SetToken { token } => {
            config.token = Some(token);
            config.save()?;
            println!("token stored in {}", crate::config::config_path().display());
        }
        ConfigCommand::SetUrl { url } => {
            config.url = Some(api_client::parse_base_url(&url)?);
            config.save()?;
            println!(
                "server url set to {} in {}",
                config.url(),
                crate::config::config_path().display()
            );
        }
        ConfigCommand::SetTransport { transport } => {
            config.transport = Some(match transport.as_str() {
                "daemon" => Transport::Daemon,
                _ => Transport::Http,
            });
            config.save()?;
            println!("transport set to {transport}");
        }
    }
    Ok(())
}

/// `syn setup` — configure both sides of a replicated installation: the Turso orgs
/// the daemon can reach and the machine-local routing that selects a workspace.
fn setup(ctx: &Context) -> Result<(), String> {
    let dir = daemon_client::state_dir()?;
    let path = daemon_client::config_path(&dir);
    if path.exists() {
        eprintln!("note: {} exists and will be replaced", path.display());
    }
    println!("Turso orgs this machine replicates. Finish with an empty org name.");
    let mut orgs = Vec::new();
    loop {
        let name = prompt(&format!("org #{} slug: ", orgs.len() + 1))?;
        if name.is_empty() {
            break;
        }
        let token = prompt_secret(&format!("platform API token for {name}: "))?;
        if token.is_empty() {
            return Err(format!("no token given for {name}; setup unchanged"));
        }
        let token = maybe_mint_machine_token(&name, token)?;
        orgs.push(ScopedOrg { name, token });
    }
    if orgs.is_empty() {
        return Err("no orgs given; setup unchanged".to_string());
    }

    let mut workspaces = crate::turso::list_workspaces(
        orgs.iter()
            .map(|org| (org.name.as_str(), org.token.as_str())),
    )?;
    let create_workspace = if workspaces.is_empty() {
        let name = prompt("no workspace databases found; first workspace [personal]: ")?;
        let name = if name.is_empty() {
            "personal".to_string()
        } else {
            resolve::validate_workspace(&name)?
        };
        workspaces.push(name.clone());
        Some(name)
    } else {
        println!("discovered workspaces: {}", workspaces.join(", "));
        None
    };

    let current_owner = ctx
        .facts()
        .ok()
        .flatten()
        .and_then(|facts| facts.owner.as_deref());
    println!(
        "Configure local workspace routing. Source-control org rules decide where repository saves go."
    );
    let cli_config =
        configure_workspace_routing(&ctx.config, &workspaces, current_owner, |label| {
            prompt(label)
        })?;
    let daemon_config = DaemonConfig {
        scoped_orgs: orgs,
        auto_update: None,
    };

    crate::config::private_dir(&dir)?;
    crate::config::write_private(&path, daemon_config.to_toml()?.as_bytes())?;
    cli_config.save()?;
    println!("wrote {}", path.display());
    println!(
        "CLI transport set to daemon; default workspace is {}",
        cli_config.default_workspace.as_deref().unwrap_or_default()
    );

    // The daemon reads its config once at boot; a live one keeps serving the old orgs.
    let probe = DaemonClient::new(daemon_client::socket_path(&dir), Duration::from_secs(1));
    let running = probe.ping().is_ok();
    if running {
        println!(
            "a daemon is running with the previous config; restart it to pick this up: \
             pkill synd (the next syn command starts it again)"
        );
    }
    if let Some(workspace) = create_workspace {
        if running {
            return Err(format!(
                "restart the daemon, then create the first workspace with: \
                 syn workspace create {workspace}"
            ));
        }
        daemon_client::ensure_running(&probe, &dir)?;
        let created = probe
            .create_workspace(&workspace)
            .map_err(|e| e.to_string())?;
        println!("workspace {} ready", created.workspace);
    }
    Ok(())
}

fn configure_workspace_routing(
    existing: &Config,
    workspaces: &[String],
    current_owner: Option<&str>,
    mut ask: impl FnMut(&str) -> Result<String, String>,
) -> Result<Config, String> {
    let mut config = existing.clone();
    config.transport = Some(Transport::Daemon);
    let default = match existing
        .default_workspace
        .as_deref()
        .filter(|name| workspaces.iter().any(|workspace| workspace == name))
    {
        Some(name) => name.to_string(),
        None if workspaces.len() == 1 => workspaces[0].clone(),
        None => {
            let answer = ask(&format!("default workspace ({}): ", workspaces.join(", ")))?;
            choose_workspace(&answer, None, workspaces)?
        }
    };
    config.default_workspace = Some(default.clone());

    if let Some(owner) = current_owner
        && !config
            .org_rules
            .iter()
            .any(|rule| rule.org.eq_ignore_ascii_case(owner))
    {
        let answer = ask(&format!(
            "workspace for current source-control org {owner} [{default}]: "
        ))?;
        let workspace = choose_workspace(&answer, Some(&default), workspaces)?;
        upsert_org_rule(&mut config, owner, &workspace);
    }

    loop {
        let org = ask("source-control org slug (empty to finish): ")?;
        if org.is_empty() {
            break;
        }
        let org = resolve::validate_org(&org)?;
        let suggested = config
            .org_rules
            .iter()
            .find(|rule| rule.org.eq_ignore_ascii_case(&org))
            .map(|rule| rule.workspace.as_str())
            .unwrap_or(&default);
        let answer = ask(&format!("workspace for {org} [{suggested}]: "))?;
        let workspace = choose_workspace(&answer, Some(suggested), workspaces)?;
        upsert_org_rule(&mut config, &org, &workspace);
    }
    Ok(config)
}

fn choose_workspace(
    answer: &str,
    default: Option<&str>,
    workspaces: &[String],
) -> Result<String, String> {
    let selected = if answer.is_empty() {
        default.ok_or_else(|| "a default workspace is required; setup unchanged".to_string())?
    } else {
        answer
    };
    let selected = resolve::validate_workspace(selected)?;
    if !workspaces.iter().any(|workspace| workspace == &selected) {
        return Err(format!(
            "workspace {selected} was not found; choose one of: {}",
            workspaces.join(", ")
        ));
    }
    Ok(selected)
}

fn upsert_org_rule(config: &mut Config, org: &str, workspace: &str) {
    config
        .org_rules
        .retain(|rule| !rule.org.eq_ignore_ascii_case(org));
    config.org_rules.push(OrgRule {
        org: org.to_string(),
        workspace: workspace.to_string(),
    });
}

/// Offer to trade the pasted token for a fresh machine-scoped one, so the long-lived
/// token the user keeps in their password manager never lands on disk. Defaults to
/// no: piped setups and users who pasted a per-machine token already keep what they gave.
fn maybe_mint_machine_token(org: &str, pasted: String) -> Result<String, String> {
    let answer = prompt("mint a fresh per-machine token from it and store that instead? [y/N]: ")?;
    if !matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(pasted);
    }
    let default_name = crate::turso::machine_token_name();
    let name = prompt(&format!("token name [{default_name}]: "))?;
    let name = if name.is_empty() { default_name } else { name };
    let minted = crate::turso::mint_token(&pasted, &name)?;
    println!("minted token {name} for {org}; the pasted token was not stored");
    Ok(minted)
}

fn prompt(label: &str) -> Result<String, String> {
    eprint!("{label}");
    std::io::stderr().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    Ok(line.trim().to_string())
}

/// Like `prompt`, but with terminal echo off so the secret stays out of the scrollback.
/// A non-tty stdin (piped setup) falls back to a plain read.
fn prompt_secret(label: &str) -> Result<String, String> {
    let stdin_fd = 0;
    if unsafe { libc::isatty(stdin_fd) } == 0 {
        return prompt(label);
    }
    eprint!("{label}");
    std::io::stderr().flush().map_err(|e| e.to_string())?;
    let mut term = std::mem::MaybeUninit::uninit();
    if unsafe { libc::tcgetattr(stdin_fd, term.as_mut_ptr()) } != 0 {
        return prompt("");
    }
    let saved = unsafe { term.assume_init() };
    let mut silent = saved;
    silent.c_lflag &= !libc::ECHO;
    if unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &silent) } != 0 {
        return prompt("");
    }
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &saved) };
    eprintln!();
    read.map_err(|e| format!("cannot read stdin: {e}"))?;
    Ok(line.trim().to_string())
}

/// The transport check comes before any client is built, so a missing HTTP token can
/// never hijack the error with a set-token hint for a transport the user is not on.
fn require_daemon(ctx: &Context, what: &str) -> Result<DaemonClient, String> {
    if ctx.config.transport() != Transport::Daemon {
        return Err(format!(
            "{what} acts on the replication daemon; run: syn config set-transport daemon"
        ));
    }
    ctx.daemon_client(WRITE_TIMEOUT, true)
}

fn sync(ctx: &Context, args: SyncArgs) -> Result<(), String> {
    let daemon = require_daemon(ctx, "syn sync")?;
    let only = args
        .workspace
        .as_deref()
        .map(resolve::validate_workspace)
        .transpose()?;
    let origin = only.clone().map(Origin::Workspace);
    let statuses = daemon.sync(origin).map_err(|e| e.to_string())?;
    let statuses: Vec<_> = statuses
        .into_iter()
        .filter(|ws| only.as_deref().is_none_or(|name| ws.name == name))
        .collect();
    if statuses.is_empty() {
        println!("no replicas open");
        return Ok(());
    }
    print_statuses(statuses);
    Ok(())
}

fn status(ctx: &Context) -> Result<(), String> {
    let daemon = require_daemon(ctx, "syn status")?;
    let statuses = daemon.status().map_err(|e| e.to_string())?;
    if statuses.is_empty() {
        println!("no replicas open");
        return Ok(());
    }
    print_statuses(statuses);
    Ok(())
}

fn print_statuses(statuses: Vec<api::rpc::WorkspaceStatus>) {
    let now = now_millis();
    for ws in statuses {
        let freshness = if ws.last_synced_at == 0 {
            "never synced".to_string()
        } else {
            format!("synced {}", age(now, ws.last_synced_at * 1000))
        };
        let connectivity = if ws.online { "online" } else { "offline" };
        let error = ws
            .error
            .map(|error| format!(" ({error})"))
            .unwrap_or_default();
        println!("{}: {connectivity}, {freshness}{error}", ws.name);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::{age, configure_workspace_routing};
    use crate::config::{Config, Transport};

    #[test]
    fn age_reads_in_the_largest_useful_unit() {
        assert_eq!(age(5_000, 5_000), "0s ago");
        assert_eq!(age(95_000, 5_000), "1m ago");
        assert_eq!(age(7_205_000, 5_000), "2h ago");
        assert_eq!(age(172_805_000, 5_000), "2d ago");
    }

    #[test]
    fn fresh_setup_selects_a_default_and_routes_source_control_orgs() {
        let mut answers: VecDeque<String> = [
            "work",
            "personal",
            "freshaengineering",
            "work",
            "surgeventures",
            "work",
            "",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let config = configure_workspace_routing(
            &Config::default(),
            &["personal".into(), "work".into()],
            Some("benediktms"),
            |_| Ok(answers.pop_front().expect("prompt has an answer")),
        )
        .unwrap();

        assert_eq!(config.transport, Some(Transport::Daemon));
        assert_eq!(config.default_workspace.as_deref(), Some("work"));
        let routes: Vec<_> = config
            .org_rules
            .iter()
            .map(|rule| (rule.org.as_str(), rule.workspace.as_str()))
            .collect();
        assert_eq!(
            routes,
            [
                ("benediktms", "personal"),
                ("freshaengineering", "work"),
                ("surgeventures", "work")
            ]
        );
    }

    #[test]
    fn setup_preserves_valid_existing_routing_without_reasking_for_it() {
        let existing: Config = toml::from_str(
            r#"
                transport = "http"
                default_workspace = "personal"
                [[org_rules]]
                org = "benediktms"
                workspace = "personal"
            "#,
        )
        .unwrap();
        let mut answers = VecDeque::from(["".to_string()]);
        let config = configure_workspace_routing(
            &existing,
            &["personal".into(), "work".into()],
            Some("benediktms"),
            |_| Ok(answers.pop_front().expect("only the additional-org prompt")),
        )
        .unwrap();

        assert_eq!(config.transport, Some(Transport::Daemon));
        assert_eq!(config.default_workspace.as_deref(), Some("personal"));
        assert_eq!(config.org_rules.len(), 1);
        assert_eq!(config.org_rules[0].org, "benediktms");
    }

    #[test]
    fn setup_rejects_a_default_that_turso_did_not_return() {
        let mut answers = VecDeque::from(["missing".to_string()]);
        let error = configure_workspace_routing(
            &Config::default(),
            &["personal".into(), "work".into()],
            None,
            |_| Ok(answers.pop_front().unwrap()),
        )
        .unwrap_err();

        assert_eq!(
            error,
            "workspace missing was not found; choose one of: personal, work"
        );
    }
}
