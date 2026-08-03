use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "syn", version, about = "Synapse agent memory")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Save a durable memory in the resolved workspace
    Save(SaveArgs),
    /// Save a memory that applies everywhere, in every workspace and project
    Remember(RememberArgs),
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
pub struct RememberArgs {
    pub content: String,
    #[arg(long = "type", value_name = "KIND", default_value = "user", value_parser = ["user", "feedback", "project", "reference"])]
    pub kind: String,
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
    #[arg(long, conflicts_with = "preference")]
    pub workspace: Option<String>,
    /// Target a memory that applies everywhere
    #[arg(long)]
    pub preference: bool,
}

#[derive(Debug, Args)]
pub struct IdArgs {
    pub id: String,
    #[arg(long, conflicts_with = "preference")]
    pub workspace: Option<String>,
    /// Target a memory that applies everywhere
    #[arg(long)]
    pub preference: bool,
}

#[derive(Debug, Args)]
pub struct MoveArgs {
    pub id: String,
    /// Workspace to move the memory into
    #[arg(
        long,
        value_name = "WORKSPACE",
        conflicts_with = "to_preference",
        required_unless_present = "to_preference"
    )]
    pub to: Option<String>,
    /// Make the memory apply everywhere, in every workspace and project
    #[arg(long)]
    pub to_preference: bool,
    /// Workspace the memory is in now (defaults to the resolved workspace)
    #[arg(long, conflicts_with = "preference")]
    pub workspace: Option<String>,
    /// The memory currently applies everywhere
    #[arg(long)]
    pub preference: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long, conflicts_with = "preference")]
    pub workspace: Option<String>,
    /// List memories that apply everywhere
    #[arg(long, conflicts_with = "pending")]
    pub preference: bool,
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
    #[arg(long, conflicts_with = "preference")]
    pub workspace: Option<String>,
    /// Dump the memories that apply everywhere
    #[arg(long)]
    pub preference: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    #[arg(long, conflicts_with = "preference")]
    pub workspace: Option<String>,
    /// Restore into the memories that apply everywhere
    #[arg(long)]
    pub preference: bool,
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

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Store the server bearer token
    SetToken { token: String },
    /// Store the server base URL
    SetUrl { url: String },
}
