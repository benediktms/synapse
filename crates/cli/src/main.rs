use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    match cli::run(cli::Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
