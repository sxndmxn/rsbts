use std::fmt::Display;
use std::io::IsTerminal;
use std::path::PathBuf;

use dialoguer::{Confirm, Select};

use rsbts::config::Config;
use rsbts::db::{validate_modification_fields, AuditIssue, Library};
use rsbts::import::{
    Action, AlbumPlan, ApprovalChoice, ApprovedAlbumPlan, ImportExecutor, ImportOptions,
    ImportPlanner,
};
use rsbts::move_files::{MoveExecutor, MovePlan};
use rsbts::providers::ProviderSet;
use rsbts::query::Query;
use rsbts::remove::{RemovalExecutor, RemovalPlan};
use rsbts::write::{TagWriteExecutor, TagWritePlan};
use rsbts::{Error, Result};

use crate::{Commands, MigrateSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Partial,
}

#[allow(clippy::future_not_send)]
pub async fn run(command: Commands, config_path: Option<PathBuf>) -> Result<Outcome> {
    let config = Config::load(config_path.as_deref())?;
    preflight(&command)?;
    match &command {
        Commands::Import { dry_run, yes, .. } => {
            require_confirmation_channel(*dry_run, *yes, "import")?;
        }
        Commands::Remove { dry_run, yes, .. } => {
            require_confirmation_channel(*dry_run, *yes, "remove")?;
        }
        Commands::Write { dry_run, yes, .. } => {
            require_confirmation_channel(*dry_run, *yes, "write")?;
        }
        Commands::Move { dry_run, yes, .. } => {
            require_confirmation_channel(*dry_run, *yes, "move")?;
        }
        Commands::Migrate {
            source: MigrateSource::Beets { dry_run, yes, .. },
        } => {
            require_confirmation_channel(*dry_run, *yes, "migration")?;
        }
        _ => {}
    }
    let command = match command {
        Commands::Migrate { source } => return migrate(source, &config),
        command => command,
    };
    let dry_run = matches!(
        &command,
        Commands::Import { dry_run: true, .. }
            | Commands::Remove { dry_run: true, .. }
            | Commands::Write { dry_run: true, .. }
            | Commands::Move { dry_run: true, .. }
    );
    let mut library = if dry_run {
        Library::open_snapshot(&config.library.database)?
    } else {
        Library::open(&config.library.database)?
    };
    if !dry_run {
        report_migration(&library);
        recover(&mut library)?;
    }
    match command {
        Commands::Import {
            paths,
            copy,
            r#move,
            link,
            in_place,
            dry_run,
            yes,
        } => {
            let action = if copy {
                Action::Copy
            } else if r#move {
                Action::Move
            } else if link {
                Action::Link
            } else if in_place {
                Action::InPlace
            } else {
                config.import.action
            };
            import(&mut library, &config, &paths, action, dry_run, yes).await
        }
        Commands::List { query, album } => list(&library, query.as_deref(), album),
        Commands::Stats => stats(&library),
        Commands::Audit => audit(&library),
        Commands::Update { query } => update(&library, query.as_deref()),
        Commands::Write {
            query,
            all,
            dry_run,
            yes,
        } => write_tags(&mut library, query.as_deref(), all, dry_run, yes),
        Commands::Move {
            query,
            all,
            dry_run,
            yes,
        } => move_files(&mut library, &config, query.as_deref(), all, dry_run, yes),
        Commands::Remove {
            query,
            delete,
            dry_run,
            yes,
        } => remove(&mut library, &query, delete, dry_run, yes),
        Commands::Modify { query, fields } => modify(&library, &query, &fields),
        Commands::Migrate { .. } => Err(Error::Config(
            "migration command was not dispatched during preflight".into(),
        )),
    }
}

fn preflight(command: &Commands) -> Result<()> {
    match command {
        Commands::List {
            query,
            album: false,
        }
        | Commands::Update { query } => {
            parse_query(query.as_deref())?;
        }
        Commands::Remove { query, .. } => {
            require_selection(query, "removal")?;
            Query::parse(query)?;
        }
        Commands::Modify { query, fields } => {
            require_selection(query, "modify")?;
            Query::parse(query)?;
            validate_modification_fields(fields)?;
        }
        Commands::Write { query, all, .. } | Commands::Move { query, all, .. } => {
            if *all {
                if query.is_some() {
                    return Err(Error::Query(
                        "operation accepts either --all or a query, not both".into(),
                    ));
                }
            } else {
                let query = query.as_deref().ok_or_else(|| {
                    Error::Query("operation requires --all or an explicit query".into())
                })?;
                require_selection(query, "operation")?;
                Query::parse(query)?;
            }
        }
        Commands::Import { .. }
        | Commands::List { album: true, .. }
        | Commands::Stats
        | Commands::Audit
        | Commands::Migrate { .. } => {}
    }
    Ok(())
}

fn report_migration(library: &Library) {
    let report = library.migration_report();
    if let Some(path) = &report.backup_path {
        println!(
            "Migrated database from schema {} to {}; verified backup: {}",
            report.from_version,
            report.to_version,
            terminal_safe(path.display())
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

#[allow(clippy::future_not_send)]
async fn import(
    library: &mut Library,
    config: &Config,
    paths: &[PathBuf],
    action: Action,
    dry_run: bool,
    yes: bool,
) -> Result<Outcome> {
    let search_limit = config
        .providers
        .enabled
        .iter()
        .map(|provider| match provider.as_str() {
            "discogs" => config.discogs.search_limit,
            _ => config.musicbrainz.search_limit,
        })
        .max()
        .unwrap_or(config.musicbrainz.search_limit);
    let options = ImportOptions {
        action,
        fetch_art: config.import.fetch_art,
        follow_symlinks: config.import.follow_symlinks,
        path_format: config.paths.format.clone(),
        library_dir: config.library.directory.clone(),
        search_limit,
        auto_accept_threshold: config.matching.auto_accept_threshold,
        runner_up_margin: config.matching.runner_up_margin,
    };
    let provider = ProviderSet::from_config(config)?;
    let plan = ImportPlanner::new(library, &provider, options.clone())
        .plan(paths)
        .await;
    for warning in &plan.provider_warnings {
        eprintln!("Provider warning: {}", terminal_safe(warning));
    }
    let mut partial = !plan.scan_issues.is_empty();
    for issue in &plan.scan_issues {
        eprintln!(
            "Scan warning: {}: {}",
            terminal_safe(issue.path.display()),
            terminal_safe(&issue.message)
        );
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
                eprintln!("  Cannot approve album: {}", terminal_safe(error));
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
                    eprintln!("  Warning: {}", terminal_safe(warning));
                }
                if report.cleanup_recovered {
                    eprintln!("  Warning: post-commit cleanup required automatic recovery");
                }
            }
            Err(error) => {
                eprintln!("  Album failed: {}", terminal_safe(error));
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
                terminal_safe(&candidate.release.artist),
                terminal_safe(&candidate.release.title),
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
        terminal_safe(&plan.source_artist),
        terminal_safe(&plan.source_album),
        plan.items.len()
    );
    if let Some(error) = &plan.lookup_error {
        eprintln!("  Metadata lookup failed: {}", terminal_safe(error));
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
            terminal_safe(&candidate.release.artist),
            terminal_safe(&candidate.release.title),
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
                println!("      gate: {}", terminal_safe(failure));
            }
        }
    }
}

fn print_approved(plan: &ApprovedAlbumPlan) {
    println!("  Planned destinations:");
    for track in &plan.tracks {
        println!(
            "    {} -> {}{}",
            terminal_safe(track.source.display()),
            terminal_safe(track.destination.display()),
            if track.already_managed {
                " (already managed)"
            } else {
                ""
            }
        );
    }
    if let Some(artwork) = &plan.artwork {
        println!(
            "    cover art -> {}",
            terminal_safe(artwork.destination.display())
        );
    }
}

fn list(library: &Library, query: Option<&str>, album: bool) -> Result<Outcome> {
    if album {
        for album in library.query_albums(query)? {
            let year = album
                .year
                .map_or_else(String::new, |year| format!(" ({year})"));
            println!(
                "{} - {}{year}",
                terminal_safe(&album.albumartist),
                terminal_safe(&album.album)
            );
        }
    } else {
        let query = parse_query(query)?;
        for item in library.query_items(&query)? {
            println!(
                "{} - {} - {} [{}]",
                terminal_safe(&item.artist),
                terminal_safe(&item.album),
                terminal_safe(&item.title),
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

fn audit(library: &Library) -> Result<Outcome> {
    let report = library.audit()?;
    if report.issues.is_empty() {
        println!("Audit: no issues found");
        return Ok(Outcome::Success);
    }
    for issue in &report.issues {
        match issue {
            AuditIssue::MissingFile { item_id, path } => {
                println!(
                    "Missing file: item {item_id}: {}",
                    terminal_safe(path.display())
                );
            }
            AuditIssue::UnknownFileSize { item_id, path } => {
                println!(
                    "Unknown file size: item {item_id}: {}",
                    terminal_safe(path.display())
                );
            }
            AuditIssue::OrphanedItem { item_id, album_id } => {
                println!("Orphaned item: item {item_id}, missing album {album_id}");
            }
            AuditIssue::SearchIndexInconsistent { detail } => {
                println!("Search index is inconsistent: {}", terminal_safe(detail));
            }
            AuditIssue::InvalidTimestamp {
                table,
                row_id,
                field,
                value,
            } => {
                println!(
                    "Invalid timestamp: {table} row {row_id}, {field}: {}",
                    terminal_safe(value)
                );
            }
        }
    }
    println!("Audit: {} issue(s) found", report.issues.len());
    Ok(Outcome::Partial)
}

fn update(library: &Library, query: Option<&str>) -> Result<Outcome> {
    let query = parse_query(query)?;
    let items = library.query_items(&query)?;
    let mut tag_updates = Vec::new();
    let mut failed = 0;
    for item in items {
        let Some(id) = item.id else {
            failed += 1;
            continue;
        };
        match rsbts::tags::read_tags(&item.path) {
            Ok(value) => tag_updates.push((id, value)),
            Err(error) => {
                eprintln!(
                    "Could not update {}: {}",
                    terminal_safe(item.path.display()),
                    terminal_safe(error)
                );
                failed += 1;
            }
        }
    }
    let updated_count = library.update_items(&tag_updates)?;
    println!("Updated {updated_count} item(s); {failed} failed");
    Ok(if failed == 0 {
        Outcome::Success
    } else {
        Outcome::Partial
    })
}

fn write_tags(
    library: &mut Library,
    raw_query: Option<&str>,
    all: bool,
    dry_run: bool,
    yes: bool,
) -> Result<Outcome> {
    let query = if all {
        Query::all()
    } else {
        let raw_query = raw_query
            .ok_or_else(|| Error::Query("write requires --all or an explicit query".into()))?;
        require_selection(raw_query, "write")?;
        Query::parse(raw_query)?
    };
    let plan = TagWritePlan::build(library, &query)?;
    println!("Tag-write plan: {} file(s)", plan.files.len());
    for file in &plan.files {
        println!("  {}", terminal_safe(file.item.path.display()));
    }
    if dry_run || plan.files.is_empty() {
        println!("No changes made");
        return Ok(Outcome::Success);
    }
    if !yes {
        let confirmed = Confirm::new()
            .with_prompt("Write database metadata to every listed file?")
            .default(false)
            .interact()
            .map_err(|error| Error::Import(format!("cannot read confirmation: {error}")))?;
        if !confirmed {
            println!("Cancelled; no changes made");
            return Ok(Outcome::Partial);
        }
    }
    let report = TagWriteExecutor::new(library).execute(plan);
    println!(
        "Wrote {} file(s); {} failed",
        report.written,
        report.failures.len()
    );
    for failure in &report.failures {
        eprintln!(
            "Could not write {}: {}",
            terminal_safe(failure.path.display()),
            terminal_safe(&failure.error)
        );
    }
    Ok(if report.failures.is_empty() {
        Outcome::Success
    } else {
        Outcome::Partial
    })
}

fn move_files(
    library: &mut Library,
    config: &Config,
    raw_query: Option<&str>,
    all: bool,
    dry_run: bool,
    yes: bool,
) -> Result<Outcome> {
    let query = if all {
        Query::all()
    } else {
        let raw_query = raw_query
            .ok_or_else(|| Error::Query("move requires --all or an explicit query".into()))?;
        require_selection(raw_query, "move")?;
        Query::parse(raw_query)?
    };
    let plan = MovePlan::build(
        library,
        &query,
        &config.library.directory,
        &config.paths.format,
    )?;
    println!("Move plan: {} file(s)", plan.tracks.len());
    for track in &plan.tracks {
        println!(
            "  {} -> {}",
            terminal_safe(track.source.display()),
            terminal_safe(track.destination.display())
        );
    }
    if dry_run || plan.tracks.is_empty() {
        println!("No changes made");
        return Ok(Outcome::Success);
    }
    if !yes {
        let confirmed = Confirm::new()
            .with_prompt("Move every listed file?")
            .default(false)
            .interact()
            .map_err(|error| Error::Import(format!("cannot read confirmation: {error}")))?;
        if !confirmed {
            println!("Cancelled; no changes made");
            return Ok(Outcome::Partial);
        }
    }
    let report = MoveExecutor::new(library, config.library.directory.clone()).execute(plan);
    println!(
        "Moved {} file(s); {} failed",
        report.moved,
        report.failures.len()
    );
    for failure in &report.failures {
        eprintln!(
            "Could not move {}: {}",
            terminal_safe(failure.source.display()),
            terminal_safe(&failure.error)
        );
    }
    Ok(if report.failures.is_empty() {
        Outcome::Success
    } else {
        Outcome::Partial
    })
}

fn migrate(source: MigrateSource, config: &Config) -> Result<Outcome> {
    match source {
        MigrateSource::Beets {
            beets_library,
            beets_config,
            music_directory,
            output_database,
            output_config,
            dry_run,
            yes,
        } => migrate_beets(
            config,
            &beets_library,
            beets_config.as_deref(),
            music_directory.as_deref(),
            output_database.as_deref(),
            output_config.as_deref(),
            dry_run,
            yes,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn migrate_beets(
    config: &Config,
    beets_library: &std::path::Path,
    beets_config: Option<&std::path::Path>,
    music_directory: Option<&std::path::Path>,
    output_database: Option<&std::path::Path>,
    output_config: Option<&std::path::Path>,
    dry_run: bool,
    yes: bool,
) -> Result<Outcome> {
    let output_database = output_database.map_or_else(
        || config.library.database.clone(),
        std::path::Path::to_path_buf,
    );
    if output_database.exists() || output_database.is_symlink() {
        return Err(Error::Config(format!(
            "migration output database already exists: {}",
            output_database.display()
        )));
    }
    if output_config.is_some_and(|path| path.exists() || path.is_symlink()) {
        return Err(Error::Config(
            "migration output config already exists".into(),
        ));
    }
    let migration = rsbts::beets::BeetsMigration::read(
        beets_library,
        beets_config,
        music_directory,
        output_database.clone(),
    )?;
    migration.validate_in_memory()?;
    println!(
        "Beets migration plan: {} album(s), {} album track(s), {} singleton(s), {} external ID(s), {} flexible field(s), {} missing file(s)",
        migration.report.albums,
        migration.report.album_tracks,
        migration.report.singletons,
        migration.report.external_ids,
        migration.report.flexible_fields,
        migration.report.missing_files,
    );
    for warning in &migration.report.warnings {
        eprintln!("Migration warning: {}", terminal_safe(warning));
    }
    if dry_run {
        println!("Dry run: no output files created");
        return Ok(if migration.report.warnings.is_empty() {
            Outcome::Success
        } else {
            Outcome::Partial
        });
    }
    if !yes {
        let confirmed = Confirm::new()
            .with_prompt("Create the new rsbts database from this Beets library?")
            .default(false)
            .interact()
            .map_err(|error| Error::Import(format!("cannot read confirmation: {error}")))?;
        if !confirmed {
            println!("Cancelled; no output files created");
            return Ok(Outcome::Partial);
        }
    }
    let parent = output_database
        .parent()
        .ok_or_else(|| Error::Config("migration output database has no parent directory".into()))?;
    std::fs::create_dir_all(parent)?;
    let name = output_database
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Config("migration database filename is not valid UTF-8".into()))?;
    let temporary = parent.join(format!(
        ".{name}.rsbts-migrate-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let import_result = (|| {
        let mut library = Library::open(&temporary)?;
        library.import_migrated_groups(&migration.groups)?;
        let expected = migration.report.album_tracks + migration.report.singletons;
        if library.stats()?.tracks as usize != expected {
            return Err(Error::Recovery(
                "migrated track count changed before finalization".into(),
            ));
        }
        drop(library);
        std::fs::hard_link(&temporary, &output_database)?;
        rsbts::db::sync_directory(parent)?;
        std::fs::remove_file(&temporary)?;
        if let Some(output_config) = output_config {
            rsbts::beets::write_config_create_new(&migration.translated_config, output_config)?;
        }
        Ok(())
    })();
    if import_result.is_err() && temporary.exists() {
        let _ = std::fs::remove_file(&temporary);
    }
    import_result?;
    println!(
        "Created migrated rsbts database: {}",
        terminal_safe(output_database.display())
    );
    Ok(if migration.report.warnings.is_empty() {
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
    require_selection(raw_query, "removal")?;
    let query = Query::parse(raw_query)?;
    let plan = RemovalPlan::build(library, &query, delete)?;
    println!(
        "Removal plan: {} database row(s), {} existing file(s) to delete, {} missing file(s)",
        plan.items.len(),
        plan.items.len().saturating_sub(plan.missing_files.len()) * usize::from(delete),
        plan.missing_files.len()
    );
    for item in &plan.items {
        println!("  {}", terminal_safe(item.path.display()));
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
        eprintln!(
            "Missing file row removed: {}",
            terminal_safe(path.display())
        );
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
    require_selection(raw_query, "modify")?;
    let query = Query::parse(raw_query)?;
    let items = library.query_items(&query)?;
    let ids = items
        .iter()
        .map(|item| {
            item.id
                .ok_or_else(|| Error::Query("a matched item has no database ID".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let count = library.modify_items(&ids, fields)?;
    println!("Modified {count} item(s)");
    Ok(Outcome::Success)
}

fn parse_query(query: Option<&str>) -> Result<Query> {
    match query {
        None => Ok(Query::all()),
        Some(query) if query.trim().is_empty() => Err(Error::Query(
            "an explicit query cannot be empty; omit it to select all items".into(),
        )),
        Some(query) => Query::parse(query),
    }
}

fn require_selection(query: &str, operation: &str) -> Result<()> {
    if query.trim().is_empty() {
        Err(Error::Query(format!(
            "{operation} query cannot be empty; provide an explicit filter"
        )))
    } else {
        Ok(())
    }
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

pub fn terminal_safe(value: impl Display) -> String {
    let mut output = String::new();
    for character in value.to_string().chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::terminal_safe;

    #[test]
    fn terminal_output_escapes_control_characters() {
        assert_eq!(terminal_safe("title\n\u{1b}[2J"), "title\\n\\u{1b}[2J");
    }
}
