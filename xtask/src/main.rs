mod install;

use clap::{Parser, Subcommand};

use install::{Harness, InstallOptions};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Synapse repository tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate and install the synapse skill and session-start hooks per harness
    InstallAgents(InstallArgs),
}

#[derive(Debug, clap::Args)]
struct InstallArgs {
    /// Harness to install into; repeat the flag, or omit for all of them
    #[arg(long = "harness", value_enum)]
    harnesses: Vec<Harness>,
    /// Report what would change without touching anything
    #[arg(long)]
    dry_run: bool,
    /// Overwrite files edited by hand or not written by this installer
    #[arg(long)]
    force: bool,
}

fn main() -> std::process::ExitCode {
    let Command::InstallAgents(args) = Cli::parse().command;
    let harnesses = if args.harnesses.is_empty() {
        Harness::all().to_vec()
    } else {
        args.harnesses
    };
    let options = InstallOptions {
        dry_run: args.dry_run,
        force: args.force,
    };
    match install::run(&harnesses, &options) {
        Ok(report) => {
            print!("{report}");
            if report.blocked() {
                std::process::ExitCode::FAILURE
            } else {
                std::process::ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
