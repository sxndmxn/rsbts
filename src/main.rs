use std::io::{self, Write as _};
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
        #[arg(short = 'C', long, conflicts_with_all = ["move", "link", "in_place"])]
        copy: bool,

        /// Move files into the library after committing metadata
        #[arg(short = 'M', long, conflicts_with_all = ["copy", "link", "in_place"])]
        r#move: bool,

        /// Create symbolic links in the library
        #[arg(short = 'L', long, conflicts_with_all = ["copy", "move", "in_place"])]
        link: bool,

        /// Catalog files at their existing paths without copying or moving them
        #[arg(long, conflicts_with_all = ["copy", "move", "link"])]
        in_place: bool,

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
        /// Track query, or album/album-artist substring with --album
        query: Option<String>,

        /// Show albums instead of tracks
        #[arg(short, long)]
        album: bool,
    },

    /// Show library statistics
    Stats,

    /// Check database and file consistency
    Audit,

    /// Re-read tags for matching library items
    Update {
        /// Query to filter items
        query: Option<String>,
    },

    /// Preview or write database metadata to audio files
    Write {
        /// Query selecting files to write
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        query: Option<String>,

        /// Explicitly select every item
        #[arg(long)]
        all: bool,

        /// Preview tag changes without touching files or the database
        #[arg(long)]
        dry_run: bool,

        /// Confirm the complete write set non-interactively
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Reorganize managed files using the configured path format
    Move {
        /// Query selecting files to move
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        query: Option<String>,

        /// Explicitly select every item
        #[arg(long)]
        all: bool,

        /// Preview destination paths without changing anything
        #[arg(long)]
        dry_run: bool,

        /// Confirm the complete move set non-interactively
        #[arg(short = 'y', long)]
        yes: bool,
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

    /// Migrate an external music library into a new rsbts database
    Migrate {
        #[command(subcommand)]
        source: MigrateSource,
    },
}

#[derive(Subcommand)]
enum MigrateSource {
    /// Read a Beets library database and optional YAML configuration
    Beets {
        /// Source Beets library database
        #[arg(long)]
        beets_library: PathBuf,

        /// Source Beets config.yaml
        #[arg(long)]
        beets_config: Option<PathBuf>,

        /// Beets music directory, required when relative paths cannot be derived from config
        #[arg(long)]
        music_directory: Option<PathBuf>,

        /// New rsbts database; defaults to library.database in the rsbts config
        #[arg(long)]
        output_database: Option<PathBuf>,

        /// Optional new rsbts TOML config to create
        #[arg(long)]
        output_config: Option<PathBuf>,

        /// Validate and report without creating output files
        #[arg(long)]
        dry_run: bool,

        /// Confirm migration non-interactively
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = Cli::parse();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut stdout = io::BufWriter::new(stdout.lock());
    let mut stderr = io::BufWriter::new(stderr.lock());
    let (result, flush_result) = {
        let mut streams = cli::Streams::new(&mut stdout, &mut stderr);
        let result = cli::run(arguments.command, arguments.config, &mut streams).await;
        let flush_result = streams.finish();
        (result, flush_result)
    };
    let result = match result {
        Ok(outcome) => flush_result.map(|()| outcome),
        Err(error) => Err(error),
    };
    match result {
        Ok(cli::Outcome::Success) => ExitCode::SUCCESS,
        Ok(cli::Outcome::Partial) => ExitCode::from(2),
        Err(error) if error.is_stdout_broken_pipe() => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(stderr, "rsbts: {}", cli::terminal_safe(error));
            let _ = stderr.flush();
            ExitCode::FAILURE
        }
    }
}
