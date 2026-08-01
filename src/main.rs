#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod cli;

#[derive(Parser)]
#[command(name = "rsbts", version)]
#[command(about = "A safe, plan-first music library manager")]
struct Cli {
    /// Path to config file
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Preview and import albums into the library
    Import {
        /// Paths to scan
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Copy files into the library
        #[arg(short = 'C', long, conflicts_with_all = ["move", "link"])]
        copy: bool,

        /// Move files into the library after committing metadata
        #[arg(short = 'M', long, conflicts_with_all = ["copy", "link"])]
        r#move: bool,

        /// Create symbolic links in the library
        #[arg(short = 'L', long, conflicts_with_all = ["copy", "move"])]
        link: bool,

        /// Preview decisions and destinations without changing files or the database
        #[arg(long)]
        dry_run: bool,

        /// Accept only candidates that pass every strict confidence gate
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// List tracks or albums
    #[command(name = "ls", alias = "list")]
    List {
        /// Query string
        query: Option<String>,

        /// Show albums instead of tracks
        #[arg(short, long)]
        album: bool,
    },

    /// Show library statistics
    Stats,

    /// Re-read tags for matching library items
    Update {
        /// Query to filter items
        query: Option<String>,
    },

    /// Remove matching items from the library
    #[command(name = "rm", alias = "remove")]
    Remove {
        /// Query to match items
        query: String,

        /// Also delete files from disk after quarantining them
        #[arg(short, long)]
        delete: bool,

        /// Preview the complete removal set without changing anything
        #[arg(long)]
        dry_run: bool,

        /// Confirm the complete removal set non-interactively
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Modify metadata stored in the library database
    Modify {
        /// Query to match items
        query: String,

        /// Field=value pairs
        #[arg(required = true)]
        fields: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = Cli::parse();
    match cli::run(arguments.command, arguments.config).await {
        Ok(cli::Outcome::Success) => ExitCode::SUCCESS,
        Ok(cli::Outcome::Partial) => ExitCode::from(2),
        Err(error) => {
            eprintln!("rsbts: {error}");
            ExitCode::FAILURE
        }
    }
}
