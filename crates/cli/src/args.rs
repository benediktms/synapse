use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "syn", version, about = "Synapse agent memory")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Save a durable memory
    Save(SaveArgs),
    /// Hybrid search over the active workspace and `shared`
    Recall(RecallArgs),
    /// Session-start digest for the current project
    Context(ContextArgs),
    /// Replace a memory's content
    Edit(EditArgs),
    /// Delete a memory
    Forget(IdArgs),
    /// List memories, or queued saves with --pending
    List(ListArgs),
    /// Show a single memory
    Show(IdArgs),
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
    /// Manage local CLI configuration
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Debug, Args)]
pub struct SaveArgs {
    pub content: String,
    /// Memory kind
    #[arg(long = "type", value_name = "KIND", value_parser = ["user", "feedback", "project", "reference"])]
    pub kind: String,
    /// `workspace`, `project` (infer from git origin), or an explicit `owner/repo`
    #[arg(long)]
    pub scope: Option<String>,
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,
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
    pub content: String,
    #[arg(long)]
    pub workspace: Option<String>,
}

#[derive(Debug, Args)]
pub struct IdArgs {
    pub id: String,
    #[arg(long)]
    pub workspace: Option<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long)]
    pub workspace: Option<String>,
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
}

#[derive(Debug, Args)]
pub struct WorkspaceArgs {
    #[arg(long)]
    pub workspace: Option<String>,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    #[arg(long)]
    pub workspace: Option<String>,
    /// Merge into a non-empty workspace instead of failing
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
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Store the server bearer token
    SetToken { token: String },
    /// Store the server base URL
    SetUrl { url: String },
}
