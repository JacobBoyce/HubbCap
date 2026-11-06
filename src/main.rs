#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hubbcap::ops;

/// HubbCap: version control that gets out of your way.
#[derive(Parser)]
#[command(name = "cap", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start tracking this folder.
    Init {
        /// Folder to use as the shared hub (created if it doesn't exist).
        #[arg(long)]
        hub: Option<PathBuf>,
        /// Your name, as teammates will see it.
        #[arg(long)]
        user: Option<String>,
        /// This computer's name, e.g. desktop or laptop.
        #[arg(long)]
        machine: Option<String>,
    },
    /// Save and share everything you changed.
    Done {
        /// Optional message; makes this save a named checkpoint.
        message: Option<String>,
    },
    /// Show what's changed here and what's waiting on the hub.
    Status,
    /// Check that everything stored here and on the hub is intact.
    Verify,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let here = PathBuf::from(".");
    let result = match cli.command {
        Command::Init { hub, user, machine } => {
            ops::init(&here, ops::InitOptions { hub, user, machine }).map(|_| true)
        }
        Command::Done { message } => ops::done(&here, message).map(|_| true),
        Command::Status => ops::status(&here).map(|_| true),
        Command::Verify => ops::verify(&here),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("cap: {e:#}");
            ExitCode::FAILURE
        }
    }
}
