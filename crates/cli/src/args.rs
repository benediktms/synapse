use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use domain::Importance;

#[derive(Debug, Parser)]
#[command(name = "syn", version, about = "Synapse agent memory")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Save a durable memory — `--scope` says how far it reaches
    Save(SaveArgs),
    #[command(hide = true)]
    Remember(RetiredArgs),
    /// Hybrid search over the active workspace and the memories that apply everywhere
    Recall(RecallArgs),
    /// Session-start digest for the current project
    Context(ContextArgs),
    /// Replace a memory's content
    Edit(EditArgs),
    /// Delete a memory
    Forget(IdArgs),
    /// Relocate a memory to another workspace, or make it apply everywhere
    Move(MoveArgs),
    /// List memories, or queued saves with --pending
    List(ListArgs),
    /// Show a single memory
    Show(ShowArgs),
    /// Dump the linked-neighbors graph around a memory as JSON (JGF v2)
    Links(LinksArgs),
    /// Link two memories as generically related (bidirectional)
    Relate(LinkPairArgs),
    /// Link two memories, one supporting the other (bidirectional)
    Support(LinkPairArgs),
    /// Link two memories as contradicting each other (bidirectional)
    Contradict(LinkPairArgs),
    /// Mark one memory as superseding another (directed; de-ranks the superseded one)
    Supersede(SupersedeArgs),
    /// Remove whatever link(s) exist between two memories
    Unlink(LinkPairArgs),
    /// Pin a memory into the digest
    Pin(IdArgs),
    /// Remove a memory from the digest
    Unpin(IdArgs),
    /// Manage workspaces
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// Write a workspace dump to stdout
    Export(WorkspaceArgs),
    /// Restore a workspace dump from stdin
    Import(ImportArgs),
    /// Configure Turso replication and local workspace routing
    Setup,
    /// Force a replica sync (daemon transport)
    Sync(SyncArgs),
    /// Per-workspace replication status (daemon transport)
    Status,
    /// Manage local CLI configuration
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Manage the daemon's login-autostart unit (launchd on macOS, systemd --user on Linux)
    #[command(subcommand)]
    Daemon(DaemonCommand),
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Write the platform unit file and load it (idempotent)
    Install,
    /// Unload the unit and remove its file (idempotent)
    Uninstall,
    /// Load the unit and mark it persistent
    Start,
    /// Unload the unit and clear the persistent bit
    Stop,
    /// Stop, then start the unit
    Restart,
    /// Print recent daemon log lines
    Logs {
        /// Keep streaming new lines until interrupted
        #[arg(short, long)]
        follow: bool,
        /// Number of existing lines to print
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Sync only this workspace (default: every replica)
    #[arg(long)]
    pub workspace: Option<String>,
}

pub const SCOPE_EVERYWHERE: &str = "everywhere";

/// `project` is the store's own name for a `decision`, kept accepting but unadvertised so
/// existing scripts and queued outbox items keep working.
fn kinds() -> Vec<clap::builder::PossibleValue> {
    use clap::builder::PossibleValue;
    vec![
        PossibleValue::new("user"),
        PossibleValue::new("feedback"),
        PossibleValue::new("decision"),
        PossibleValue::new("reference"),
        PossibleValue::new("project").hide(true),
    ]
}

/// The importance tiers, interpolated from the domain map so help can never drift from it.
fn tiers() -> Vec<clap::builder::PossibleValue> {
    use clap::builder::PossibleValue;
    Importance::ALL
        .iter()
        .map(|tier| PossibleValue::new(tier.as_str()))
        .collect()
}

#[derive(Debug, Args)]
pub struct SaveArgs {
    /// The fact in full
    #[arg(long)]
    pub body: String,
    /// One line stating the fact — this is all the session-start digest shows of it
    #[arg(long)]
    pub title: String,
    /// What kind of fact this is
    #[arg(long = "kind", visible_alias = "type", value_name = "KIND", value_parser = clap::builder::PossibleValuesParser::new(kinds()))]
    pub kind: String,
    /// How far the fact reaches: omit for this repo, or `workspace`, `everywhere`, `owner/repo`
    #[arg(long)]
    pub scope: Option<String>,
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,
    /// How important this fact is
    #[arg(long, value_name = "TIER", value_parser = clap::builder::PossibleValuesParser::new(tiers()))]
    pub importance: Option<String>,
}

/// `syn remember` was folded into `syn save --scope everywhere`. It stays parseable so the
/// error can name both forms instead of clap printing "unrecognized subcommand".
#[derive(Debug, Args)]
pub struct RetiredArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<String>,
}

/// Which store an existing memory lives in.
#[derive(Debug, Args)]
pub struct StoreArgs {
    #[arg(long, conflicts_with_all = ["scope", "preference"])]
    pub workspace: Option<String>,
    /// `everywhere` targets the memories that apply in every workspace
    #[arg(long, value_parser = [SCOPE_EVERYWHERE])]
    pub scope: Option<String>,
    #[arg(long, hide = true)]
    pub preference: bool,
}

impl StoreArgs {
    pub fn everywhere(&self) -> bool {
        self.scope.is_some() || self.preference
    }
}

#[derive(Debug, Args)]
pub struct RecallArgs {
    pub query: String,
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(short = 'n', long, default_value_t = 10)]
    pub limit: usize,
    /// Search every workspace, grouped by workspace
    #[arg(long)]
    pub all_workspaces: bool,
    #[arg(long)]
    pub project: Option<String>,
    /// Only surface linked neighbors within the recall's scope (default: cross-scope links surface)
    #[arg(long)]
    pub links_in_scope: bool,
    #[command(flatten)]
    pub detail: DetailArgs,
}

#[derive(Debug, Args)]
pub struct ContextArgs {
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    pub id: String,
    /// Replace the fact in full; may be combined with --relation/--type to also retype a link
    #[arg(long = "body", alias = "content")]
    pub body: Option<String>,
    /// Replace the one-line statement of the fact
    #[arg(long)]
    pub title: Option<String>,
    /// New importance tier for the memory
    #[arg(long, value_name = "TIER", value_parser = clap::builder::PossibleValuesParser::new(tiers()))]
    pub importance: Option<String>,
    /// Other endpoint of the link to retype (requires --type)
    #[arg(long, requires = "type_")]
    pub relation: Option<String>,
    /// Retype an existing link (with --relation) to this type: relation/support/contradiction/supersession
    #[arg(long, value_name = "TYPE", value_parser = clap::builder::PossibleValuesParser::new(relation_types()))]
    pub type_: Option<String>,
    #[command(flatten)]
    pub store: StoreArgs,
}

fn relation_types() -> Vec<clap::builder::PossibleValue> {
    ["relation", "support", "contradiction", "supersession"]
        .iter()
        .map(|s| clap::builder::PossibleValue::new(*s))
        .collect()
}

#[derive(Debug, Args)]
pub struct IdArgs {
    pub id: String,
    #[command(flatten)]
    pub store: StoreArgs,
}

/// How much of a memory to print. `short` is its title, or the one derived from its content;
/// `full` is the whole content. The session-start digest is always short.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Detail {
    Short,
    #[default]
    Full,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    pub id: String,
    #[command(flatten)]
    pub detail: DetailArgs,
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct DetailArgs {
    /// How much of each memory to print
    #[arg(long, value_enum, default_value_t = Detail::Full)]
    pub detail: Detail,
}

#[derive(Debug, Args)]
pub struct LinksArgs {
    pub id: String,
    /// How many hops of the graph to expand (default 2)
    #[arg(long, default_value_t = 2)]
    pub depth: usize,
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct LinkPairArgs {
    /// First memory id
    pub a: String,
    /// Second memory id
    pub b: String,
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct SupersedeArgs {
    /// The memory being superseded (the old one)
    #[arg(long)]
    pub old: String,
    /// The memory that supersedes it (the new one)
    #[arg(long)]
    pub new: String,
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct MoveArgs {
    pub id: String,
    /// Where it belongs: a workspace name, or `everywhere`
    #[arg(
        long,
        value_name = "WORKSPACE",
        conflicts_with = "to_preference",
        required_unless_present = "to_preference"
    )]
    pub to: Option<String>,
    #[arg(long, hide = true)]
    pub to_preference: bool,
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Show locally queued saves and dead-lettered items instead
    #[arg(long)]
    pub pending: bool,
    /// Move pending items to another workspace (requires --pending)
    #[arg(long, requires = "pending", conflicts_with = "discard")]
    pub reassign: Option<String>,
    /// Drop pending items without sending them (requires --pending)
    #[arg(long, requires = "pending")]
    pub discard: bool,
    /// Restrict --reassign/--discard to one memory id
    #[arg(long, requires = "pending")]
    pub id: Option<String>,
    #[command(flatten)]
    pub detail: DetailArgs,
}

#[derive(Debug, Args)]
pub struct WorkspaceArgs {
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Merge into a non-empty target instead of failing
    #[arg(long)]
    pub merge: bool,
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// List workspaces known to the server
    List,
    /// Create a workspace on the server
    Create { name: String },
    /// Set this machine's default workspace
    Use { name: String },
    /// Bind a directory tree to a workspace, so saves under it resolve without a flag
    Map { path: PathBuf, name: String },
    /// Bind every repo under a GitHub/GitLab org to a workspace; path rules still win
    MapOrg { org: String, name: String },
}

/// All three verbs configured the retired HTTP server. They stay parseable so a script that
/// still calls one is told what replaced it, rather than getting clap's "unrecognized
/// subcommand" and a non-zero exit it cannot interpret.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    #[command(hide = true)]
    SetToken { token: String },
    #[command(hide = true)]
    SetUrl { url: String },
    #[command(hide = true)]
    SetTransport {
        #[arg(value_parser = ["http", "daemon"])]
        transport: String,
    },
}

impl ConfigCommand {
    pub fn retired_flag(&self) -> &'static str {
        match self {
            Self::SetToken { .. } => "set-token",
            Self::SetUrl { .. } => "set-url",
            Self::SetTransport { .. } => "set-transport",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save_from(extra: &[&str]) -> Result<Cli, clap::Error> {
        let mut argv = vec!["syn", "save", "--body", "a fact", "--kind", "user"];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv)
    }

    #[test]
    fn importance_flag_accepts_only_domain_tiers() {
        let cli = save_from(&["--title", "A title", "--importance", "high"])
            .expect("a domain tier must parse");
        let Command::Save(args) = cli.command else {
            panic!("expected save")
        };
        assert_eq!(args.importance.as_deref(), Some("high"));

        assert!(
            save_from(&["--title", "A title", "--importance", "urgent"]).is_err(),
            "an unknown tier must be rejected by clap"
        );
    }

    /// A derived title is a fallback for memories written before titles existed, never a
    /// substitute for one on a new memory — so clap refuses the save rather than inferring.
    #[test]
    fn saving_without_a_title_is_refused_before_anything_is_sent() {
        let err = save_from(&[]).expect_err("a save with no title must not parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(err.to_string().contains("--title"), "{err}");

        let cli = save_from(&["--title", "Deploys use ArgoCD"]).expect("a titled save parses");
        let Command::Save(args) = cli.command else {
            panic!("expected save")
        };
        assert_eq!(args.title, "Deploys use ArgoCD");
    }
}
