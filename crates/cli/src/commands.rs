use std::cell::OnceCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use api::{
    ExportDoc, MemoryDto, MoveBody, Origin, PatchMemoryBody, PutMemoryBody, PutPreferenceBody,
    SearchResponse,
};
use api_client::SynapseApiClient;
use domain::MemoryId;

use crate::args::{
    Cli, Command, ConfigCommand, ContextArgs, EditArgs, IdArgs, ImportArgs, LinkPairArgs,
    LinksArgs, ListArgs, MoveArgs, RecallArgs, RetiredArgs, SCOPE_EVERYWHERE, SaveArgs, StoreArgs,
    SupersedeArgs, WorkspaceArgs, WorkspaceCommand,
};
use crate::config::{Config, OrgRule, WorkspaceRule};
use crate::git::GitFacts;
use crate::outbox::{FlushReport, Outbox, PendingSave, SaveTarget, now_millis};
use crate::output;
use crate::resolve;

const READ_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const BULK_TIMEOUT: Duration = Duration::from_secs(600);
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
    fn client(&self, timeout: Duration) -> Result<SynapseApiClient, String> {
        SynapseApiClient::new(self.config.url(), self.config.token()?, timeout)
            .map_err(|e| e.to_string())
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
fn flush_before_read(ctx: &Context) {
    let Ok(outbox) = Outbox::open() else {
        return;
    };
    let flushed = ctx
        .client(FLUSH_SEND_TIMEOUT)
        .and_then(|client| outbox.flush(&client, Some(FLUSH_BUDGET)));
    let Ok(report) = flushed else {
        return;
    };
    for (id, failure) in &report.dead_lettered {
        eprintln!("note: {id} moved to dead-letter: {failure}");
    }
    if report.still_queued > 0 {
        let reason = report.deferred.as_deref().unwrap_or("still unsent");
        eprintln!(
            "note: {} saves are queued locally ({reason}); this read may predate them — see: syn list --pending",
            report.still_queued
        );
    }
}

fn save(ctx: &Context, args: SaveArgs) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    let kind = wire_kind(&args.kind);
    if args.scope.as_deref() == Some(SCOPE_EVERYWHERE) {
        if args.workspace.is_some() {
            return Err(
                "--scope everywhere applies in every workspace, so it cannot take --workspace"
                    .to_string(),
            );
        }
        return queue_and_flush(
            &client,
            SaveTarget::Preference {
                body: PutPreferenceBody {
                    content: args.content,
                    kind,
                    tags: args.tags,
                    importance: args.importance,
                },
            },
        );
    }
    let workspace = ctx.workspace(args.workspace.as_deref(), true)?;
    let scope = ctx.scope(args.scope.as_deref())?;
    if let Some(note) = &scope.note {
        eprintln!("note: {note}");
    }
    queue_and_flush(
        &client,
        SaveTarget::Memory {
            workspace,
            body: PutMemoryBody {
                content: args.content,
                kind,
                scope: scope.scope,
                tags: args.tags,
                importance: args.importance,
            },
        },
    )
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
fn queue_and_flush(client: &SynapseApiClient, target: SaveTarget) -> Result<(), String> {
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
    let report = outbox.flush(client, None)?;
    if report.sent.contains(&id) {
        println!("saved {id} ({where_to})");
        return Ok(());
    }
    if let Some((_, failure)) = report.dead_lettered.iter().find(|(dead, _)| *dead == id) {
        return Err(format!("{failure} (see: syn list --pending)"));
    }
    report_backlog(&report);
    println!("queued {id} ({where_to}) — queued locally, not yet recallable");
    Ok(())
}

fn report_backlog(report: &FlushReport) {
    if let Some(reason) = &report.deferred {
        eprintln!("note: {reason}");
    }
    for (id, failure) in &report.dead_lettered {
        eprintln!("note: {id} moved to dead-letter: {failure}");
    }
}

fn recall(ctx: &Context, args: RecallArgs) -> Result<(), String> {
    let client = ctx.client(READ_TIMEOUT)?;
    flush_before_read(ctx);
    let workspace = ctx.workspace(args.workspace.as_deref(), false)?;
    let scope = ctx.scope(args.project.as_deref())?;
    let started = Instant::now();
    let response = client
        .search(
            &workspace,
            &args.query,
            scope.project(),
            args.limit,
            args.all_workspaces,
            args.links_in_scope,
        )
        .map_err(|e| e.to_string())?;
    let elapsed = started.elapsed().as_millis();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let count = match &response {
        SearchResponse::Flat { hits } => {
            for hit in hits {
                let _ = writeln!(out, "{}", output::hit_line(hit));
            }
            hits.len()
        }
        SearchResponse::Grouped { groups } => {
            for group in groups {
                let _ = writeln!(out, "## {}", output::store_label(&group.origin));
                for hit in &group.hits {
                    let _ = writeln!(out, "{}", output::hit_line(hit));
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
    let digest = client
        .context(&workspace, scope.project())
        .map_err(|e| e.to_string())?;
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
    client: &SynapseApiClient,
    target: &Origin,
    id: &str,
    body: &PatchMemoryBody,
) -> Result<MemoryDto, String> {
    match target {
        Origin::Preference => client.edit_preference(id, body),
        Origin::Workspace(workspace) => client.edit(workspace, id, body),
    }
    .map_err(|e| e.to_string())
}

fn edit(ctx: &Context, args: EditArgs) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    // Retype mode: --relation <other> --type <t> changes an existing edge's type.
    if let Some(relation) = args.relation {
        let target = resolve_target(ctx, &args.store)?;
        let b = relation;
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
        client
            .retype_link(&workspace, &args.id, &b, ty)
            .map_err(|e| e.to_string())?;
        println!("retyped {} ↔ {b} ({})", args.id, ty);
        return Ok(());
    }
    let content = args
        .content
        .ok_or("syn edit needs a new content, or --relation/--type to retype a link")?;
    let target = resolve_target(ctx, &args.store)?;
    let body = PatchMemoryBody {
        content: Some(content),
        importance: args.importance,
        ..PatchMemoryBody::default()
    };
    let memory = patch(&client, &target, &args.id, &body)?;
    println!("updated {} ({})", memory.id, output::store_label(&target));
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
    client
        .link(&workspace, &args.a, &args.b, relation.as_str())
        .map_err(|e| e.to_string())?;
    println!("linked {} → {} ({})", args.a, args.b, relation);
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
    client
        .link(&workspace, &args.new, &args.old, "supersession")
        .map_err(|e| e.to_string())?;
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
    client
        .unlink(&workspace, &args.a, &args.b)
        .map_err(|e| e.to_string())?;
    println!("unlinked {} ↔ {}", args.a, args.b);
    Ok(())
}

fn forget(ctx: &Context, args: IdArgs) -> Result<(), String> {
    let client = ctx.client(WRITE_TIMEOUT)?;
    let target = resolve_target(ctx, &args.store)?;
    match &target {
        Origin::Preference => client.forget_preference(&args.id),
        Origin::Workspace(workspace) => client.forget(workspace, &args.id),
    }
    .map_err(|e| e.to_string())?;
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
    let response = client
        .move_memory(&args.id, &body)
        .map_err(|e| e.to_string())?;
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
    }
    .map_err(|e| e.to_string())?;
    for memory in memories {
        println!("{}", output::list_line(&target, &memory));
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

fn show(ctx: &Context, args: IdArgs) -> Result<(), String> {
    let client = ctx.client(READ_TIMEOUT)?;
    flush_before_read(ctx);
    let target = resolve_target(ctx, &args.store)?;
    let memory = match &target {
        Origin::Preference => client.get_preference(&args.id),
        Origin::Workspace(workspace) => client.get(workspace, &args.id),
    }
    .map_err(|e| e.to_string())?;
    println!("{}", output::memory_line(&target, &memory));
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
    let graph = client
        .links(&workspace, &args.id, args.depth)
        .map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&graph).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

fn workspace(ctx: &Context, command: WorkspaceCommand) -> Result<(), String> {
    match command {
        WorkspaceCommand::List => {
            let client = ctx.client(READ_TIMEOUT)?;
            let default = ctx.config.default_workspace.as_deref();
            for workspace in client.workspaces().map_err(|e| e.to_string())? {
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
            let created = client.create_workspace(&name).map_err(|e| e.to_string())?;
            println!("workspace {} ready", created.workspace);
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
    }
    .map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

/// A dump is only a backup once this machine's queue is on the server, so an unflushable
/// backlog fails the export rather than writing a quietly incomplete file.
fn drain_before_export(client: &SynapseApiClient) -> Result<(), String> {
    let outbox = Outbox::open()?;
    let report = outbox.flush(client, None)?;
    report_backlog(&report);
    if report.still_queued > 0 {
        return Err(format!(
            "{} saves are still queued locally and would be missing from this dump (see: syn list --pending)",
            report.still_queued
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
    }
    .map_err(|e| e.to_string())?;
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
            config.url = Some(url.trim_end_matches('/').to_string());
            config.save()?;
            println!("server url set to {}", config.url());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::age;

    #[test]
    fn age_reads_in_the_largest_useful_unit() {
        assert_eq!(age(5_000, 5_000), "0s ago");
        assert_eq!(age(95_000, 5_000), "1m ago");
        assert_eq!(age(7_205_000, 5_000), "2h ago");
        assert_eq!(age(172_805_000, 5_000), "2d ago");
    }
}
