use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

mod cli;

#[derive(Parser)]
#[command(name = "rsbts", version)]
#[command(about = "A music library manager in Rust, focused on the beets core")]
struct Cli {
    /// Path to config file
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Override the library database path
    #[arg(short = 'l', long = "library")]
    library: Option<PathBuf>,

    /// Override the music directory
    #[arg(short = 'd', long)]
    directory: Option<PathBuf>,

    /// Output contract for automation
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    output: OutputFormat,

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

        /// Import scanned embedded metadata without online lookups
        #[arg(long, visible_alias = "as-is")]
        existing_tags: bool,

        /// Confirm strict direct-ID matches or the --existing-tags selection
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// List tracks or albums
    #[command(name = "ls", alias = "list")]
    List {
        /// Search terms, field filters, and sort terms
        query: Vec<String>,

        /// Show albums instead of tracks
        #[arg(short, long)]
        album: bool,

        /// Print file paths or album directories
        #[arg(short = 'p', long, conflicts_with = "format")]
        path: bool,

        /// Format each row with $field or ${field} substitutions
        #[arg(short = 'f', long)]
        format: Option<String>,

        /// Maximum rows to emit
        #[arg(short = 'l', long)]
        limit: Option<u32>,
    },

    /// Show library statistics
    Stats {
        /// Search terms and field filters
        query: Vec<String>,
    },

    /// Show the effective configuration without opening the library
    Config {
        /// Print the configuration file path
        #[arg(short = 'p', long)]
        path: bool,
        /// Show built-in defaults instead of the loaded configuration
        #[arg(short = 'd', long = "default")]
        defaults: bool,
    },

    /// Show available item and album fields
    Fields,

    /// Check database and file consistency
    Audit {
        /// Read and compare full BLAKE3 and SHA-256 fixity for every managed asset
        #[arg(long)]
        deep: bool,
    },

    /// Run an explicit full `SQLite` integrity and foreign-key check
    Integrity,

    /// Plan, schedule, execute, and inspect bounded fixity work
    Fixity {
        #[command(subcommand)]
        action: FixityCommand,
    },

    /// Calculate and store persistent fixity for selected catalog items
    Verify {
        /// Query to filter items; omit to verify the complete catalog
        query: Vec<String>,
    },

    /// Re-read tags for matching library items
    Update {
        /// Query to filter items
        query: Vec<String>,

        /// Preview catalog changes without changing the database
        #[arg(long, alias = "pretend")]
        dry_run: bool,
    },

    /// Preview or write database metadata to audio files
    Write {
        /// Query selecting files to write
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        query: Vec<String>,

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
        query: Vec<String>,

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
        #[arg(required = true)]
        query: Vec<String>,

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

    /// Permanently purge retained removal quarantines
    Purge {
        /// Purge quarantines at least this many days old
        #[arg(long, default_value_t = 30)]
        older_than_days: u64,

        /// Preview the complete purge set without changing anything
        #[arg(long)]
        dry_run: bool,

        /// Confirm the complete purge set non-interactively
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Modify metadata stored in the library database
    Modify {
        /// Search terms followed by field=value assignments
        #[arg(required = true, value_name = "QUERY_OR_FIELD")]
        arguments: Vec<String>,

        /// Preview field changes without changing the database
        #[arg(long)]
        dry_run: bool,

        /// Confirm the complete modification set non-interactively
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Preview and apply a canonical provider-refresh diff
    ProviderRefresh {
        /// Entity family (release, release-group, recording, work, artist)
        entity_kind: String,
        /// Internal entity UUID
        entity_id: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Preview and apply a journaled tag projection
    TagProject {
        item_id: i64,
        #[arg(long)]
        title: String,
        #[arg(long = "artist", required = true)]
        artists: Vec<String>,
        #[arg(long)]
        album: String,
        #[arg(long = "album-artist", required = true)]
        album_artists: Vec<String>,
        #[arg(long, default_value = "archival-native-rich")]
        profile: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Preview and apply a journaled managed-asset rename
    PathProject {
        asset_id: String,
        destination_relative: PathBuf,
        #[arg(long, default_value = "portable")]
        profile: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Inspect or control a durable plan
    Plan {
        #[command(subcommand)]
        action: PlanCommand,
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
        #[arg(long)]
        beets_library: PathBuf,
        #[arg(long)]
        beets_config: Option<PathBuf>,
        #[arg(long)]
        music_directory: Option<PathBuf>,
        #[arg(long)]
        output_database: Option<PathBuf>,
        #[arg(long)]
        output_config: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Jsonl,
}

#[derive(Subcommand)]
enum PlanCommand {
    Status {
        id: String,
    },
    Events {
        id: String,
        /// Return events after this sequence number
        #[arg(long)]
        after_sequence: Option<u64>,
        /// Maximum events to return, from 1 through 4096
        #[arg(long, default_value_t = 512)]
        limit: u32,
    },
    Cancel {
        id: String,
    },
    Resume {
        id: String,
    },
}

#[derive(Subcommand)]
enum FixityCommand {
    /// Persist a reviewable fixity preview
    Plan {
        /// Use the quick metadata/identity policy instead of full content hashing
        #[arg(long)]
        quick: bool,
    },
    /// Approve a planned fixity run without executing it
    Approve { id: String },
    /// Execute one bounded page; repeat until complete
    Run {
        id: String,
        #[arg(long, default_value_t = 512)]
        page_size: u32,
    },
    /// Stream one keyset page of retained results
    Results {
        id: String,
        #[arg(long)]
        after_asset_id: Option<String>,
        #[arg(long, default_value_t = 512)]
        limit: u32,
    },
    /// Create a persistent schedule whose occurrences receive standing approval
    Schedule {
        #[arg(long)]
        interval_seconds: u64,
        #[arg(long)]
        quick: bool,
    },
    /// Materialize due schedule occurrences as approved plans
    Due {
        #[arg(long, default_value_t = 32)]
        limit: u32,
    },
    /// List schedules in keyset order
    Schedules {
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value_t = 256)]
        limit: u32,
    },
    /// Enable or disable a schedule
    Enable {
        id: String,
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
    },
    /// List retained run history for a schedule
    History {
        schedule_id: String,
        #[arg(long)]
        after_plan_id: Option<String>,
        #[arg(long, default_value_t = 256)]
        limit: u32,
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
        let result = cli::run(
            arguments.command,
            arguments.config,
            arguments.library,
            arguments.directory,
            arguments.output,
            &mut streams,
        )
        .await;
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands};

    #[test]
    fn import_accepts_existing_tags_and_as_is_alias() -> Result<(), clap::Error> {
        for flag in ["--existing-tags", "--as-is"] {
            let arguments = Cli::try_parse_from(["rsbts", "import", flag, "album"])?;
            assert!(matches!(
                arguments.command,
                Commands::Import {
                    existing_tags: true,
                    ..
                }
            ));
        }
        Ok(())
    }
}
