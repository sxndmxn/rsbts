// Precision loss when converting u64 to f64 for display/comparison is acceptable.
// Truncation is handled manually with clamp/max/round where needed.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use anyhow::Result;
use clap::{Parser, Subcommand};

mod cli;

#[derive(Parser)]
#[command(name = "rsbts")]
#[command(about = "A music library manager with MusicBrainz auto-tagging")]
struct Cli {
    /// Path to config file
    #[arg(short, long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Import music into library
    Import {
        /// Paths to import
        #[arg(required = true)]
        paths: Vec<std::path::PathBuf>,

        /// Copy files (don't move)
        #[arg(short = 'C', long)]
        copy: bool,

        /// Move files
        #[arg(short = 'M', long)]
        r#move: bool,
    },

    /// List items in library
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

    /// Update library (re-read tags)
    Update {
        /// Query to filter items
        query: Option<String>,
    },

    /// Remove items from library
    #[command(name = "rm", alias = "remove")]
    Remove {
        /// Query to match items
        query: String,

        /// Also delete files from disk
        #[arg(short, long)]
        delete: bool,
    },

    /// Modify item metadata
    Modify {
        /// Query to match items
        query: String,

        /// Field=value pairs
        #[arg(required = true)]
        fields: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    cli::run(cli.command, cli.config).await
}
