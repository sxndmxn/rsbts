use std::io::IsTerminal;
use std::path::PathBuf;

use dialoguer::{Confirm, Select};

use rsbts::config::Config;
use rsbts::db::{AuditIssue, Library};
use rsbts::import::{
    Action, AlbumPlan, ApprovalChoice, ApprovedAlbumPlan, ImportExecutor, ImportOptions,
    ImportPlanner,
};
use rsbts::musicbrainz::MusicBrainzProvider;
use rsbts::query::Query;
use rsbts::remove::{RemovalExecutor, RemovalPlan};
use rsbts::{Error, Result};

use crate::Commands;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Partial,
}

#[allow(clippy::future_not_send)]
pub async fn run(command: Commands, config_path: Option<PathBuf>) -> Result<Outcome> {
    let config = Config::load(config_path.as_deref())?;
    let mut library = Library::open(&config.library.database)?;
    report_migration(&library);
    recover(&mut library)?;
    report_audit(&library)?;

    match command {
        Commands::Import {
            paths,
            copy,
            r#move,
            link,
            dry_run,
            yes,
        } => {
            let action = if copy {
                Action::Copy
            } else if r#move {
                Action::Move
            } else if link {
                Action::Link
            } else {
                config.import.action
            };
            import(&mut library, &config, &paths, action, dry_run, yes).await
        }
        Commands::List { query, album } => list(&library, query.as_deref(), album),
        Commands::Stats => stats(&library),
        Commands::Update { query } => update(&library, query.as_deref()),
        Commands::Remove {
            query,
            delete,
            dry_run,
            yes,
        } => remove(&mut library, &query, delete, dry_run, yes),
        Commands::Modify { query, fields } => modify(&library, &query, &fields),
    }
}

fn report_migration(library: &Library) {
    let report = library.migration_report();
    if let Some(path) = &report.backup_path {
        println!(
            "Migrated database from schema {} to {}; verified backup: {}",
            report.from_version,
            report.to_version,
            path.display()
        );
    }
}

fn recover(library: &mut Library) -> Result<()> {
    let report = library.recover_pending()?;
    if !report.recovered_operations.is_empty() {
        eprintln!(
            "Recovered {} interrupted operation(s)",
            report.recovered_operations.len()
        );
    }
    if report.unresolved.is_empty() {
        Ok(())
    } else {
        Err(Error::Recovery(report.unresolved.join("; ")))
    }
}

fn report_audit(library: &Library) -> Result<()> {
    let report = library.audit()?;
    let missing = report
        .issues
        .iter()
        .filter(|issue| matches!(issue, AuditIssue::MissingFile { .. }))
        .count();
    if missing > 0 {
        eprintln!("Warning: {missing} library row(s) refer to missing files; rows were preserved");
    }
    let structural = report.issues.len().saturating_sub(missing);
    if structural > 0 {
        eprintln!("Warning: library audit found {structural} additional issue(s)");
    }
    Ok(())
}

#[allow(clippy::future_not_send)]
async fn import(
    library: &mut Library,
    config: &Config,
    paths: &[PathBuf],
    action: Action,
    dry_run: bool,
    yes: bool,
) -> Result<Outcome> {
    require_confirmation_channel(dry_run, yes, "import")?;
    let options = ImportOptions {
        action,
        fetch_art: config.import.fetch_art,
        follow_symlinks: config.import.follow_symlinks,
        path_format: config.paths.format.clone(),
        library_dir: config.library.directory.clone(),
        search_limit: config.musicbrainz.search_limit,
        auto_accept_threshold: config.matching.auto_accept_threshold,
        runner_up_margin: config.matching.runner_up_margin,
    };
    let provider = MusicBrainzProvider::new(&config.musicbrainz)?;
    let plan = ImportPlanner::new(library, &provider, options.clone())
        .plan(paths)
        .await;
    let mut partial = !plan.scan_issues.is_empty();
    for issue in &plan.scan_issues {
        eprintln!("Scan warning: {}: {}", issue.path.display(), issue.message);
    }
    if plan.albums.is_empty() {
        println!("No readable audio files found");
        return Ok(if partial {
            Outcome::Partial
        } else {
            Outcome::Success
        });
    }

    for album_plan in plan.albums {
        print_album_plan(&album_plan);
        let choice = choose_import(&album_plan, dry_run, yes)?;
        if choice == ApprovalChoice::Skip {
            println!("  Skipped");
            partial = true;
            continue;
        }
        let approved = ImportPlanner::new(library, &provider, options.clone())
            .approve(&album_plan, choice)
            .await;
        let Some(approved) = (match approved {
            Ok(approved) => approved,
            Err(error) => {
                eprintln!("  Cannot approve album: {error}");
                partial = true;
                continue;
            }
        }) else {
            partial = true;
            continue;
        };
        print_approved(&approved);
        if dry_run {
            println!("  Dry run: no changes made");
            continue;
        }
        match ImportExecutor::new(library).execute(approved) {
            Ok(report) => {
                println!(
                    "  Imported {} track(s); {} already managed",
                    report.imported_tracks, report.already_managed_tracks
                );
                for warning in report.warnings {
                    eprintln!("  Warning: {warning}");
                }
                if report.cleanup_recovered {
                    eprintln!("  Warning: post-commit cleanup required automatic recovery");
                }
            }
            Err(error) => {
                eprintln!("  Album failed: {error}");
                partial = true;
            }
        }
    }
    Ok(if partial {
        Outcome::Partial
    } else {
        Outcome::Success
    })
}

fn choose_import(plan: &AlbumPlan, dry_run: bool, yes: bool) -> Result<ApprovalChoice> {
    if dry_run {
        return Ok(if plan.candidates.is_empty() {
            ApprovalChoice::AsIs
        } else {
            ApprovalChoice::Candidate(0)
        });
    }
    if yes {
        return Ok(match plan.candidates.first() {
            Some(candidate) if candidate.confidence.high_confidence => ApprovalChoice::Candidate(0),
            _ => ApprovalChoice::Skip,
        });
    }

    let mut choices = plan
        .candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            format!(
                "{}. {} — {} ({}, {:.1}%)",
                index + 1,
                candidate.release.artist,
                candidate.release.title,
                candidate
                    .release
                    .year
                    .map_or_else(|| "year unknown".into(), |year| year.to_string()),
                candidate.confidence.composite * 100.0
            )
        })
        .collect::<Vec<_>>();
    choices.push("Import with existing tags".into());
    choices.push("Skip album".into());
    let selected = Select::new()
        .with_prompt("Choose metadata")
        .items(&choices)
        .default(0)
        .interact()
        .map_err(|error| Error::Import(format!("cannot read selection: {error}")))?;
    match selected.cmp(&plan.candidates.len()) {
        std::cmp::Ordering::Less => Ok(ApprovalChoice::Candidate(selected)),
        std::cmp::Ordering::Equal => Ok(ApprovalChoice::AsIs),
        std::cmp::Ordering::Greater => Ok(ApprovalChoice::Skip),
    }
}

fn print_album_plan(plan: &AlbumPlan) {
    println!(
        "\n{} — {} ({} track(s))",
        plan.source_artist,
        plan.source_album,
        plan.items.len()
    );
    if let Some(error) = &plan.lookup_error {
        eprintln!("  Metadata lookup failed: {error}");
    }
    if plan.candidates.is_empty() {
        println!("  No provider candidates; existing tags are available as an explicit choice");
        return;
    }
    for (index, candidate) in plan.candidates.iter().enumerate() {
        let confidence = &candidate.confidence;
        println!(
            "  [{}] {} — {} | total {:.1}% (artist {:.1}, album {:.1}, tracks {:.1}, provider {:.1}; margin {:.1}){}",
            index + 1,
            candidate.release.artist,
            candidate.release.title,
            confidence.composite * 100.0,
            confidence.artist * 100.0,
            confidence.album * 100.0,
            confidence.mean_track * 100.0,
            confidence.provider * 100.0,
            confidence.runner_up_margin * 100.0,
            if confidence.high_confidence { " [strict auto-accept]" } else { "" }
        );
        if index == 0 {
            for failure in &confidence.gate_failures {
                println!("      gate: {failure}");
            }
        }
    }
}

fn print_approved(plan: &ApprovedAlbumPlan) {
    println!("  Planned destinations:");
    for track in &plan.tracks {
        println!(
            "    {} -> {}{}",
            track.source.display(),
            track.destination.display(),
            if track.already_managed {
                " (already managed)"
            } else {
                ""
            }
        );
    }
    if let Some(artwork) = &plan.artwork {
        println!("    cover art -> {}", artwork.destination.display());
    }
}

fn list(library: &Library, query: Option<&str>, album: bool) -> Result<Outcome> {
    if album {
        for album in library.query_albums(query)? {
            let year = album
                .year
                .map_or_else(String::new, |year| format!(" ({year})"));
            println!("{} - {}{year}", album.albumartist, album.album);
        }
    } else {
        let query = parse_query(query)?;
        for item in library.query_items(&query)? {
            println!(
                "{} - {} - {} [{}]",
                item.artist,
                item.album,
                item.title,
                format_duration(item.length)
            );
        }
    }
    Ok(Outcome::Success)
}

fn stats(library: &Library) -> Result<Outcome> {
    let stats = library.stats()?;
    println!("Tracks: {}", stats.tracks);
    println!("Albums: {}", stats.albums);
    println!("Artists: {}", stats.artists);
    println!("Total time: {}", format_duration(stats.total_length));
    println!("Total size: {}", format_size(stats.total_size));
    if stats.unknown_sizes > 0 {
        println!("Unknown file sizes: {}", stats.unknown_sizes);
    }
    Ok(Outcome::Success)
}

fn update(library: &Library, query: Option<&str>) -> Result<Outcome> {
    let query = parse_query(query)?;
    let items = library.query_items(&query)?;
    let mut updated = 0;
    let mut failed = 0;
    for item in items {
        let Some(id) = item.id else {
            failed += 1;
            continue;
        };
        match rsbts::tags::read_tags(&item.path) {
            Ok(value) => {
                library.update_item(id, &value)?;
                updated += 1;
            }
            Err(error) => {
                eprintln!("Could not update {}: {error}", item.path.display());
                failed += 1;
            }
        }
    }
    println!("Updated {updated} item(s); {failed} failed");
    Ok(if failed == 0 {
        Outcome::Success
    } else {
        Outcome::Partial
    })
}

fn remove(
    library: &mut Library,
    raw_query: &str,
    delete: bool,
    dry_run: bool,
    yes: bool,
) -> Result<Outcome> {
    require_confirmation_channel(dry_run, yes, "remove")?;
    let query = Query::parse(raw_query)?;
    let plan = RemovalPlan::build(library, &query, delete)?;
    println!(
        "Removal plan: {} database row(s), {} existing file(s) to delete, {} missing file(s)",
        plan.items.len(),
        plan.items.len().saturating_sub(plan.missing_files.len()) * usize::from(delete),
        plan.missing_files.len()
    );
    for item in &plan.items {
        println!("  {}", item.path.display());
    }
    if dry_run || plan.items.is_empty() {
        println!("No changes made");
        return Ok(Outcome::Success);
    }
    if !yes {
        let confirmed = Confirm::new()
            .with_prompt(if delete {
                "Remove all rows and delete all listed existing files?"
            } else {
                "Remove all listed rows from the library?"
            })
            .default(false)
            .interact()
            .map_err(|error| Error::Import(format!("cannot read confirmation: {error}")))?;
        if !confirmed {
            println!("Cancelled; no changes made");
            return Ok(Outcome::Partial);
        }
    }
    let report = RemovalExecutor::new(library).execute(plan)?;
    println!(
        "Removed {} row(s); deleted {} file(s)",
        report.removed_rows, report.deleted_files
    );
    for path in &report.missing_files {
        eprintln!("Missing file row removed: {}", path.display());
    }
    if report.cleanup_recovered {
        eprintln!("Warning: post-commit cleanup required automatic recovery");
    }
    Ok(if report.missing_files.is_empty() {
        Outcome::Success
    } else {
        Outcome::Partial
    })
}

fn modify(library: &Library, raw_query: &str, fields: &[String]) -> Result<Outcome> {
    let query = Query::parse(raw_query)?;
    let items = library.query_items(&query)?;
    let count = items.len();
    for item in items {
        if let Some(id) = item.id {
            library.modify_item(id, fields)?;
        }
    }
    println!("Modified {count} item(s)");
    Ok(Outcome::Success)
}

fn parse_query(query: Option<&str>) -> Result<Query> {
    query.map_or_else(|| Ok(Query::all()), Query::parse)
}

fn require_confirmation_channel(dry_run: bool, yes: bool, operation: &str) -> Result<()> {
    if !dry_run && !yes && !std::io::stdin().is_terminal() {
        Err(Error::Config(format!(
            "{operation} requires an interactive terminal, --dry-run, or --yes"
        )))
    } else {
        Ok(())
    }
}

fn format_duration(seconds: f64) -> String {
    let total_seconds = seconds.max(0.0) as u64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}
