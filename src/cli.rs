use std::fmt::Display;
use std::io::IsTerminal;
use std::io::Write as _;
use std::path::PathBuf;

use chrono::Utc;
use dialoguer::{Confirm, Select};
use num_traits::ToPrimitive;

use rsbts::catalog::{EntityId, EntityKind};
use rsbts::config::Config;
use rsbts::db::{validate_modification_fields, AuditIssue, AuditMode, Library};
use rsbts::fixity::{FixityMode, FixityScheduleId};
use rsbts::import::{
    Action, AlbumPlan, ApprovalChoice, ApprovedAlbumPlan, ImportExecutor, ImportOptions,
    ImportPlanner,
};
use rsbts::musicbrainz::MusicBrainzProvider;
use rsbts::naming::NamingProfile;
use rsbts::operations::PlanId;
use rsbts::query::Query;
use rsbts::remove::{PurgeExecutor, PurgePlan, RemovalExecutor, RemovalPlan};
use rsbts::tag_projection::TagProjectionExecutor;
use rsbts::tags::{CanonicalTags, TagProfile};
use rsbts::{Error, Result};

use crate::{Commands, FixityCommand, OutputFormat, PlanCommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Partial,
}

#[allow(clippy::future_not_send)]
#[expect(
    clippy::too_many_lines,
    reason = "the top-level dispatcher centralizes preflight, read-only opening, recovery, and command routing"
)]
pub async fn run(
    command: Commands,
    config_path: Option<PathBuf>,
    output: OutputFormat,
) -> Result<Outcome> {
    let config = Config::load(config_path.as_deref())?;
    preflight(&command)?;
    if output != OutputFormat::Text
        && matches!(
            &command,
            Commands::Import {
                dry_run: false,
                yes: false,
                ..
            } | Commands::Remove {
                dry_run: false,
                yes: false,
                ..
            } | Commands::Purge {
                dry_run: false,
                yes: false,
                ..
            } | Commands::ProviderRefresh {
                dry_run: false,
                yes: false,
                ..
            } | Commands::TagProject {
                dry_run: false,
                yes: false,
                ..
            } | Commands::PathProject {
                dry_run: false,
                yes: false,
                ..
            }
        )
    {
        return Err(Error::Config(
            "machine-readable mutation requires --dry-run or --yes".into(),
        ));
    }
    match &command {
        Commands::Import { dry_run, yes, .. } => {
            require_confirmation_channel(*dry_run, *yes, "import")?;
        }
        Commands::Remove { dry_run, yes, .. } => {
            require_confirmation_channel(*dry_run, *yes, "remove")?;
        }
        Commands::Purge { dry_run, yes, .. } => {
            require_confirmation_channel(*dry_run, *yes, "purge")?;
        }
        Commands::ProviderRefresh { dry_run, yes, .. }
        | Commands::TagProject { dry_run, yes, .. }
        | Commands::PathProject { dry_run, yes, .. } => {
            require_confirmation_channel(*dry_run, *yes, "projection")?;
        }
        _ => {}
    }
    let command = match command {
        Commands::Migrate { source } => return migrate(source, &config, streams),
        command => command,
    };
    let dry_run = matches!(
        &command,
        Commands::Import { dry_run: true, .. }
            | Commands::Remove { dry_run: true, .. }
            | Commands::Purge { dry_run: true, .. }
            | Commands::ProviderRefresh { dry_run: true, .. }
            | Commands::TagProject { dry_run: true, .. }
            | Commands::PathProject { dry_run: true, .. }
    );
    let ordinary_read = matches!(
        &command,
        Commands::List { .. }
            | Commands::Stats
            | Commands::Integrity
            | Commands::Fixity {
                action: FixityCommand::Results { .. }
                    | FixityCommand::Schedules { .. }
                    | FixityCommand::History { .. },
            }
            | Commands::Plan {
                action: PlanCommand::Status { .. } | PlanCommand::Events { .. }
            }
    );
    let read_only_open = ordinary_read && config.library.database.exists();
    let mut library = if dry_run {
        Library::open_snapshot(&config.library.database)?
    } else if read_only_open {
        Library::open_read_only(&config.library.database)?
    } else {
        Library::open(&config.library.database)?
    };
    if !dry_run && !read_only_open {
        if output == OutputFormat::Text {
            report_migration(&library);
        }
        recover(&mut library, output)?;
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
            import(&mut library, &config, &paths, action, dry_run, yes, output).await
        }
        Commands::List {
            query,
            album,
            limit,
        } => list(&library, query.as_deref(), album, limit, output),
        Commands::Stats => stats(&library, output),
        Commands::Audit { deep } => audit(&library, deep, output),
        Commands::Integrity => integrity(&library, output),
        Commands::Fixity { action } => fixity_command(&library, &action, output),
        Commands::Verify { query } => {
            verify(&mut library, &config.library.directory, query.as_deref())
        }
        Commands::Update { query } => update(&library, query.as_deref()),
        Commands::Remove {
            query,
            delete,
            dry_run,
            yes,
        } => remove(&mut library, &query, delete, dry_run, yes, output),
        Commands::Purge {
            older_than_days,
            dry_run,
            yes,
        } => purge(&mut library, older_than_days, dry_run, yes),
        Commands::Modify { query, fields } => modify(&library, &query, &fields),
        Commands::ProviderRefresh {
            entity_kind,
            entity_id,
            dry_run,
            yes,
        } => provider_refresh(&library, &entity_kind, &entity_id, dry_run, yes, output),
        Commands::TagProject {
            item_id,
            title,
            artists,
            album,
            album_artists,
            profile,
            dry_run,
            yes,
        } => tag_project(
            &mut library,
            item_id,
            title,
            artists,
            album,
            album_artists,
            &profile,
            dry_run,
            yes,
            output,
        ),
        Commands::PathProject {
            asset_id,
            destination_relative,
            profile,
            dry_run,
            yes,
        } => path_project(
            &mut library,
            &asset_id,
            &destination_relative,
            &profile,
            dry_run,
            yes,
            output,
        ),
        Commands::Plan { action } => plan_command(&library, &action, output),
    }
}

fn preflight(command: &Commands) -> Result<()> {
    match command {
        Commands::List {
            query,
            album: false,
            ..
        }
        | Commands::Update { query }
        | Commands::Verify { query } => {
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
        Commands::ProviderRefresh {
            entity_kind,
            entity_id,
            ..
        } => {
            parse_entity_kind(entity_kind)?;
            EntityId::parse(entity_id.clone())?;
        }
        Commands::TagProject {
            title,
            artists,
            album,
            album_artists,
            profile,
            ..
        } => {
            parse_tag_profile(profile)?;
            CanonicalTags::new(title, artists.clone(), album, album_artists.clone())?;
        }
        Commands::PathProject {
            asset_id, profile, ..
        } => {
            uuid::Uuid::parse_str(asset_id)
                .map_err(|error| Error::Operation(format!("invalid asset ID: {error}")))?;
            parse_naming_profile(profile)?;
        }
        Commands::Plan { action } => {
            let id = match action {
                PlanCommand::Status { id }
                | PlanCommand::Events { id }
                | PlanCommand::Cancel { id }
                | PlanCommand::Resume { id } => id,
            };
            PlanId::parse(id.clone())?;
        }
        Commands::Fixity { action } => preflight_fixity(action)?,
        Commands::Import { .. }
        | Commands::Purge { .. }
        | Commands::List { album: true, .. }
        | Commands::Stats
        | Commands::Audit { .. }
        | Commands::Integrity => {}
    }
    Ok(())
}

fn preflight_fixity(action: &FixityCommand) -> Result<()> {
    match action {
        FixityCommand::Plan { .. } => {}
        FixityCommand::Approve { id }
        | FixityCommand::Run { id, .. }
        | FixityCommand::Results { id, .. } => {
            PlanId::parse(id.clone())?;
        }
        FixityCommand::Schedule {
            interval_seconds, ..
        } => {
            if *interval_seconds == 0 {
                return Err(Error::Operation(
                    "fixity interval must be at least one second".into(),
                ));
            }
        }
        FixityCommand::Due { limit } => {
            if *limit == 0 || *limit > 256 {
                return Err(Error::Operation(
                    "due fixity schedule limit must be between 1 and 256".into(),
                ));
            }
        }
        FixityCommand::Schedules { after, limit } => {
            after.clone().map(FixityScheduleId::parse).transpose()?;
            if *limit == 0 || *limit > 4096 {
                return Err(Error::Operation(
                    "fixity schedule limit must be between 1 and 4096".into(),
                ));
            }
        }
        FixityCommand::Enable { id, .. } => {
            FixityScheduleId::parse(id.clone())?;
        }
        FixityCommand::History {
            schedule_id,
            after_plan_id,
            limit,
        } => {
            FixityScheduleId::parse(schedule_id.clone())?;
            after_plan_id.clone().map(PlanId::parse).transpose()?;
            if *limit == 0 || *limit > 4096 {
                return Err(Error::Operation(
                    "fixity history limit must be between 1 and 4096".into(),
                ));
            }
        }
    }
    if let FixityCommand::Run { page_size, .. } = action {
        if *page_size == 0 || *page_size > 4096 {
            return Err(Error::Operation(
                "fixity page size must be between 1 and 4096".into(),
            ));
        }
    }
    if let FixityCommand::Results { limit, .. } = action {
        if *limit == 0 || *limit > 4096 {
            return Err(Error::Operation(
                "fixity result limit must be between 1 and 4096".into(),
            ));
        }
    }
    Ok(())
}

fn report_migration(library: &Library, streams: &mut Streams<'_>) -> CliResult<()> {
    let report = library.migration_report();
    if let Some(path) = &report.backup_path {
        outln!(
            streams,
            "Migrated database from schema {} to {}; verified backup: {}",
            report.from_version,
            report.to_version,
            terminal_safe(path.display())
        );
    }
    Ok(())
}

fn recover(library: &mut Library, output: OutputFormat) -> Result<()> {
    let report = library.recover_pending()?;
    if output == OutputFormat::Text && !report.recovered_operations.is_empty() {
        eprintln!(
            "Recovered {} interrupted operation(s)",
            report.recovered_operations.len()
        );
    }
    if report.unresolved.is_empty() {
        Ok(())
    } else {
        Err(Error::Recovery(report.unresolved.join("; ")).into())
    }
}

#[allow(clippy::future_not_send)]
#[expect(
    clippy::too_many_lines,
    reason = "import keeps preview, match review, approval, execution, and machine output as one ordered protocol"
)]
async fn import(
    library: &mut Library,
    config: &Config,
    paths: &[PathBuf],
    action: Action,
    dry_run: bool,
    yes: bool,
    output: OutputFormat,
) -> Result<Outcome> {
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
    let preview = serde_json::to_value(&plan)?;
    let mut execution = Vec::new();
    let mut partial = !plan.scan_issues.is_empty();
    if output == OutputFormat::Text {
        for issue in &plan.scan_issues {
            eprintln!(
                "Scan warning: {}: {}",
                terminal_safe(issue.path.display()),
                terminal_safe(&issue.message)
            );
        }
    }
    if plan.albums.is_empty() {
        if output == OutputFormat::Text {
            println!("No readable audio files found");
        } else {
            emit(
                output,
                &serde_json::json!({"plan": preview, "execution": execution}),
            )?;
        }
        return Ok(if partial {
            Outcome::Partial
        } else {
            Outcome::Success
        });
    }

    for album_plan in plan.albums {
        if output == OutputFormat::Text {
            print_album_plan(&album_plan);
        }
        let choice = choose_import(&album_plan, dry_run, yes || output != OutputFormat::Text)?;
        if choice == ApprovalChoice::Skip {
            if output == OutputFormat::Text {
                println!("  Skipped");
            }
            execution
                .push(serde_json::json!({"album": album_plan.source_album, "state": "skipped"}));
            partial = true;
            continue;
        }
        let approved = ImportPlanner::new(library, &provider, options.clone())
            .approve(&album_plan, choice)
            .await;
        let Some(approved) = (match approved {
            Ok(approved) => approved,
            Err(error) => {
                if output == OutputFormat::Text {
                    eprintln!("  Cannot approve album: {}", terminal_safe(&error));
                }
                execution.push(serde_json::json!({
                    "album": album_plan.source_album,
                    "state": "approval-failed",
                    "error": error.to_string(),
                }));
                partial = true;
                continue;
            }
        }) else {
            partial = true;
            continue;
        };
        if output == OutputFormat::Text {
            print_approved(&approved);
        }
        if dry_run {
            if output == OutputFormat::Text {
                println!("  Dry run: no changes made");
            }
            execution
                .push(serde_json::json!({"album": album_plan.source_album, "state": "previewed"}));
            continue;
        }
        match ImportExecutor::new(library).execute(approved) {
            Ok(report) => {
                if output == OutputFormat::Text {
                    println!(
                        "  Imported {} track(s); {} already managed",
                        report.imported_tracks, report.already_managed_tracks
                    );
                    for warning in &report.warnings {
                        eprintln!("  Warning: {}", terminal_safe(warning));
                    }
                    if report.cleanup_recovered {
                        eprintln!("  Warning: post-commit cleanup required automatic recovery");
                    }
                }
                execution.push(serde_json::to_value(report)?);
            }
            Err(error) => {
                if output == OutputFormat::Text {
                    eprintln!("  Album failed: {}", terminal_safe(&error));
                }
                execution.push(serde_json::json!({
                    "album": album_plan.source_album,
                    "state": "failed",
                    "error": error.to_string(),
                }));
                partial = true;
            }
        }
    }
    if output != OutputFormat::Text {
        emit(
            output,
            &serde_json::json!({"plan": preview, "execution": execution}),
        )?;
    }
    Ok(if partial {
        Outcome::Partial
    } else {
        Outcome::Success
    })
}

fn choose_import(plan: &AlbumPlan, dry_run: bool, yes: bool) -> Result<ApprovalChoice> {
    if dry_run || yes {
        // Fuzzy provider metadata is review-only until the calibrated hard-negative
        // release gate is met. Non-interactive and dry-run decisions therefore use
        // local tags and exercise the same safe acceptance path.
        return Ok(ApprovalChoice::AsIs);
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

fn print_album_plan(plan: &AlbumPlan, streams: &mut Streams<'_>) -> CliResult<()> {
    outln!(
        streams,
        "\n{} — {} ({} track(s))",
        terminal_safe(&plan.source_artist),
        terminal_safe(&plan.source_album),
        plan.items.len()
    );
    if let Some(error) = &plan.lookup_error {
        errln!(
            streams,
            "  Metadata lookup failed: {}",
            terminal_safe(error)
        );
    }
    if !plan.candidate_set_complete {
        eprintln!("  Provider candidate set is incomplete; fuzzy acceptance is disabled");
    }
    if plan.candidates.is_empty() {
        outln!(
            streams,
            "  No provider candidates; existing tags are available as an explicit choice"
        );
        return Ok(());
    }
    for (index, candidate) in plan.candidates.iter().enumerate() {
        let confidence = &candidate.confidence;
        let margin = confidence
            .runner_up_margin
            .map_or_else(|| "unknown".into(), |value| format!("{:.1}", value * 100.0));
        println!(
            "  [{}] {} — {} | total {:.1}% (recording {:.1}, release-group {:.1}, exact-release {:.1}; artist {:.1}, album {:.1}, tracks {:.1}, provider {:.1}; margin {margin}){}",
            index + 1,
            terminal_safe(&candidate.release.artist),
            terminal_safe(&candidate.release.title),
            confidence.composite * 100.0,
            confidence.recording_identity * 100.0,
            confidence.release_group_identity * 100.0,
            confidence.exact_release_identity * 100.0,
            confidence.artist * 100.0,
            confidence.album * 100.0,
            confidence.mean_track * 100.0,
            confidence.provider * 100.0,
            if confidence.high_confidence { " [passes score gates; review required]" } else { "" }
        );
        if index == 0 {
            for failure in &confidence.gate_failures {
                outln!(streams, "      gate: {}", terminal_safe(failure));
            }
        }
    }
    Ok(())
}

fn print_approved(plan: &ApprovedAlbumPlan, streams: &mut Streams<'_>) -> CliResult<()> {
    outln!(streams, "  Planned destinations:");
    for track in &plan.tracks {
        outln!(
            streams,
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
        outln!(
            streams,
            "    cover art -> {}",
            terminal_safe(artwork.destination.display())
        );
    }
    Ok(())
}

fn list(
    library: &Library,
    query: Option<&str>,
    album: bool,
    limit: Option<u32>,
    output: OutputFormat,
) -> Result<Outcome> {
    let limit = limit.unwrap_or(1_000);
    if limit == 0 || limit > 100_000 {
        return Err(Error::Query(
            "list limit must be between 1 and 100000".into(),
        ));
    }
    if album {
        let mut after = None;
        let mut remaining = limit;
        let mut json_albums = Vec::new();
        while remaining > 0 {
            let page_size = remaining.min(1_000);
            let page = library.query_albums_page(query, after, page_size)?;
            if page.is_empty() {
                break;
            }
            for album in &page {
                if output == OutputFormat::Json {
                    json_albums.push(album.clone());
                } else if output == OutputFormat::Jsonl {
                    emit(output, album)?;
                } else {
                    let year = album
                        .year
                        .map_or_else(String::new, |year| format!(" ({year})"));
                    println!(
                        "{} - {}{year}",
                        terminal_safe(&album.albumartist),
                        terminal_safe(&album.album)
                    );
                }
            }
            remaining = remaining.saturating_sub(page.len() as u32);
            after = page.last().and_then(|album| album.id);
            if page.len() < page_size as usize {
                break;
            }
        }
        if output == OutputFormat::Json {
            emit(output, &json_albums)?;
        }
    } else {
        let query = parse_query(query)?;
        let mut after = None;
        let mut remaining = limit;
        let mut json_items = Vec::new();
        while remaining > 0 {
            let page_size = remaining.min(1_000);
            let page = library.query_items_page(&query, after, page_size)?;
            if page.is_empty() {
                break;
            }
            for item in &page {
                if output == OutputFormat::Json {
                    json_items.push(item.clone());
                } else if output == OutputFormat::Jsonl {
                    emit(output, item)?;
                } else {
                    println!(
                        "{} - {} - {} [{}]",
                        terminal_safe(&item.artist),
                        terminal_safe(&item.album),
                        terminal_safe(&item.title),
                        format_duration(item.length)
                    );
                }
            }
            remaining = remaining.saturating_sub(page.len() as u32);
            after = page.last().and_then(|item| item.id);
            if page.len() < page_size as usize {
                break;
            }
        }
        if output == OutputFormat::Json {
            emit(output, &json_items)?;
        }
    }
    Ok(Outcome::Success)
}

fn stats(library: &Library, output: OutputFormat) -> Result<Outcome> {
    let stats = library.stats()?;
    if output != OutputFormat::Text {
        emit(output, &stats)?;
        return Ok(Outcome::Success);
    }
    println!("Tracks: {}", stats.tracks);
    println!("Albums: {}", stats.albums);
    println!("Artists: {}", stats.artists);
    println!("Total time: {}", format_duration(stats.total_length));
    println!("Total size: {}", format_size(stats.total_size));
    if stats.unknown_sizes > 0 {
        outln!(streams, "Unknown file sizes: {}", stats.unknown_sizes);
    }
    Ok(Outcome::Success)
}

#[allow(clippy::too_many_lines)]
fn audit(library: &Library, deep: bool, output: OutputFormat) -> Result<Outcome> {
    if deep {
        let plan = library.plan_fixity(FixityMode::Deep)?;
        if output == OutputFormat::Text {
            println!(
                "Deep fixity plan {} covers {} managed asset(s); review it, then run `rsbts fixity approve {}`",
                plan.id().as_str(),
                plan.asset_count(),
                plan.id().as_str()
            );
        } else {
            emit(output, &plan)?;
        }
        return Ok(Outcome::Success);
    }
    let mode = if deep {
        AuditMode::Deep
    } else {
        AuditMode::Quick
    };
    let report = library.audit_with_mode(mode)?;
    if output == OutputFormat::Json {
        emit(
            output,
            &serde_json::json!({
                "mode": if deep { "deep" } else { "quick" },
                "issues": report.issues(),
                "omitted": report.omitted(),
            }),
        )?;
        return Ok(if report.is_empty() {
            Outcome::Success
        } else {
            Outcome::Partial
        });
    }
    if output == OutputFormat::Jsonl {
        for issue in report.issues() {
            emit(output, issue)?;
        }
        if report.omitted() > 0 {
            emit(
                output,
                &serde_json::json!({"summary": {"omitted": report.omitted()}}),
            )?;
        }
        return Ok(if report.is_empty() {
            Outcome::Success
        } else {
            Outcome::Partial
        });
    }
    if report.is_empty() {
        println!("Audit: no issues found");
        return Ok(Outcome::Success);
    }
    for issue in report.issues() {
        match issue {
            AuditIssue::DatabaseIntegrity { detail } => {
                outln!(
                    streams,
                    "Database integrity check failed: {}",
                    terminal_safe(detail)
                );
            }
            AuditIssue::ForeignKeyViolations { count } => {
                outln!(streams, "Database has {count} foreign-key violation(s)");
            }
            AuditIssue::MissingFile { item_id, path } => {
                outln!(
                    streams,
                    "Missing file: item {item_id}: {}",
                    terminal_safe(path.display())
                );
            }
            AuditIssue::UnknownFileSize { item_id, path } => {
                outln!(
                    streams,
                    "Unknown file size: item {item_id}: {}",
                    terminal_safe(path.display())
                );
            }
            AuditIssue::OrphanedItem { item_id, album_id } => {
                outln!(
                    streams,
                    "Orphaned item: item {item_id}, missing album {album_id}"
                );
            }
            AuditIssue::SearchIndexInconsistent { detail } => {
                outln!(
                    streams,
                    "Search index is inconsistent: {}",
                    terminal_safe(detail)
                );
            }
            AuditIssue::InvalidTimestamp {
                table,
                row_id,
                field,
                value,
            } => {
                outln!(
                    streams,
                    "Invalid timestamp: {table} row {row_id}, {field}: {}",
                    terminal_safe(value)
                );
            }
            AuditIssue::MissingManagedAsset {
                asset_id,
                path,
                role,
            } => println!(
                "Missing managed {role}: asset {}: {}",
                terminal_safe(asset_id),
                terminal_safe(path.display())
            ),
            AuditIssue::MissingAssetRecord { item_id, path } => println!(
                "Missing managed asset record for item {item_id}: {}",
                terminal_safe(path.display())
            ),
            AuditIssue::UnverifiedAsset { asset_id, path } => println!(
                "Unverified asset {}: {}",
                terminal_safe(asset_id),
                terminal_safe(path.display())
            ),
            AuditIssue::AssetSizeMismatch {
                asset_id,
                path,
                expected,
                actual,
            } => println!(
                "Asset size mismatch {}: expected {expected}, found {actual}: {}",
                terminal_safe(asset_id),
                terminal_safe(path.display())
            ),
            AuditIssue::AssetMtimeMismatch {
                asset_id,
                path,
                expected,
                actual,
            } => println!(
                "Asset mtime mismatch {}: expected {}, found {}: {}",
                terminal_safe(asset_id),
                terminal_safe(expected),
                terminal_safe(actual),
                terminal_safe(path.display())
            ),
            AuditIssue::AssetEntryIdentityMismatch { asset_id, path } => println!(
                "Asset filesystem identity changed {}: {}",
                terminal_safe(asset_id),
                terminal_safe(path.display())
            ),
            AuditIssue::AssetDigestMismatch {
                asset_id,
                path,
                algorithm,
            } => println!(
                "Asset {algorithm} mismatch {}: {}",
                terminal_safe(asset_id),
                terminal_safe(path.display())
            ),
            AuditIssue::AssetUnreadable {
                asset_id,
                path,
                detail,
            } => println!(
                "Unreadable asset {}: {}: {}",
                terminal_safe(asset_id),
                terminal_safe(path.display()),
                terminal_safe(detail)
            ),
            AuditIssue::OrphanedManagedAsset { asset_id, path } => println!(
                "Orphaned managed asset {}: {}",
                terminal_safe(asset_id),
                terminal_safe(path.display())
            ),
            AuditIssue::ProjectionDiverged {
                asset_id,
                path,
                state,
            } => println!(
                "Asset projection {} is {}: {}",
                terminal_safe(asset_id),
                terminal_safe(state),
                terminal_safe(path.display())
            ),
            AuditIssue::MediaPropertiesMismatch { asset_id, path } => println!(
                "Asset media properties changed {}: {}",
                terminal_safe(asset_id),
                terminal_safe(path.display())
            ),
            AuditIssue::AudioEssenceMismatch { asset_id, path } => println!(
                "Asset decoded audio essence changed {}: {}",
                terminal_safe(asset_id),
                terminal_safe(path.display())
            ),
            _ => println!("Audit found an issue not recognized by this client"),
        }
    }
    println!(
        "Audit: {} issue(s) shown, {} omitted",
        report.issues().len(),
        report.omitted()
    );
    Ok(Outcome::Partial)
}

fn integrity(library: &Library, output: OutputFormat) -> Result<Outcome> {
    let report = library.integrity_check()?;
    if output == OutputFormat::Text {
        if report.is_ok() {
            println!("Database integrity: ok");
        } else {
            for message in report.messages() {
                println!("{}", terminal_safe(message));
            }
            if report.truncated() {
                println!("Additional integrity errors were omitted");
            }
        }
    } else if output == OutputFormat::Json {
        emit(output, &report)?;
    } else {
        for message in report.messages() {
            emit(output, &serde_json::json!({"integrity_error": message}))?;
        }
        emit(
            output,
            &serde_json::json!({"summary": {"ok": report.is_ok(), "truncated": report.truncated()}}),
        )?;
    }
    Ok(if report.is_ok() {
        Outcome::Success
    } else {
        Outcome::Partial
    })
}

fn update(library: &Library, query: Option<&str>) -> Result<Outcome> {
    let query = parse_query(query)?;
    let items = library.query_items_bounded(&query, 9_999)?;
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
                errln!(
                    streams,
                    "Could not update {}: {}",
                    terminal_safe(item.path.display()),
                    terminal_safe(error)
                );
                failed += 1;
            }
        }
    }
    let updated_count = library.update_items(&tag_updates)?;
    outln!(streams, "Updated {updated_count} item(s); {failed} failed");
    Ok(if failed == 0 {
        Outcome::Success
    } else {
        Outcome::Partial
    })
}

fn verify(library: &mut Library, root: &std::path::Path, query: Option<&str>) -> Result<Outcome> {
    let query = parse_query(query)?;
    let report = library.verify_items(&query, root)?;
    println!("Verified {} asset(s)", report.verified);
    for (item_id, path, detail) in &report.skipped {
        println!(
            "Skipped item {item_id}: {}: {}",
            terminal_safe(path.display()),
            terminal_safe(detail)
        );
    }
    if report.skipped.is_empty() {
        Ok(Outcome::Success)
    } else {
        Ok(Outcome::Partial)
    }
}

fn remove(
    library: &mut Library,
    raw_query: &str,
    delete: bool,
    dry_run: bool,
    yes: bool,
    output: OutputFormat,
) -> Result<Outcome> {
    require_selection(raw_query, "removal")?;
    let query = Query::parse(raw_query)?;
    let plan = RemovalPlan::build(library, &query, delete)?;
    if output == OutputFormat::Text {
        println!(
            "Removal plan: {} database row(s), {} existing file(s) to quarantine, {} missing file(s)",
            plan.items.len(),
            plan.items.len().saturating_sub(plan.missing_files.len()) * usize::from(delete),
            plan.missing_files.len()
        );
        for item in &plan.items {
            println!("  {}", terminal_safe(item.path.display()));
        }
    }
    if dry_run || plan.items.is_empty() {
        if output == OutputFormat::Text {
            println!("No changes made");
        } else {
            emit(
                output,
                &serde_json::json!({"plan": plan, "executed": false}),
            )?;
        }
        return Ok(Outcome::Success);
    }
    if !yes {
        let confirmed = Confirm::new()
            .with_prompt(if delete {
                "Remove all rows and quarantine all listed existing files?"
            } else {
                "Remove all listed rows from the library?"
            })
            .default(false)
            .interact()
            .map_err(|error| Error::Import(format!("cannot read confirmation: {error}")))?;
        if !confirmed {
            outln!(streams, "Cancelled; no changes made");
            return Ok(Outcome::Partial);
        }
    }
    let plan = plan.approve(library)?;
    let plan_id = plan.id().cloned();
    let report = RemovalExecutor::new(library).execute(plan)?;
    if output == OutputFormat::Text {
        println!(
            "Removed {} row(s); quarantined {} file(s)",
            report.removed_rows, report.quarantined_files
        );
        for path in &report.missing_files {
            eprintln!(
                "Missing file row removed: {}",
                terminal_safe(path.display())
            );
        }
    } else {
        emit(
            output,
            &serde_json::json!({"plan_id": plan_id, "report": report}),
        )?;
    }
    Ok(if report.missing_files.is_empty() {
        Outcome::Success
    } else {
        Outcome::Partial
    })
}

fn purge(library: &mut Library, older_than_days: u64, dry_run: bool, yes: bool) -> Result<Outcome> {
    let plan = PurgePlan::build(library, older_than_days)?;
    println!(
        "Purge plan: {} quarantined file(s) at least {older_than_days} day(s) old",
        plan.len()
    );
    for path in plan.paths() {
        println!("  {}", terminal_safe(path.display()));
    }
    if dry_run || plan.is_empty() {
        println!("No changes made");
        return Ok(Outcome::Success);
    }
    if !yes {
        let confirmed = Confirm::new()
            .with_prompt("Permanently delete every listed quarantine?")
            .default(false)
            .interact()
            .map_err(|error| Error::Import(format!("cannot read confirmation: {error}")))?;
        if !confirmed {
            println!("Cancelled; no changes made");
            return Ok(Outcome::Partial);
        }
    }
    let plan = plan.approve(library)?;
    let report = PurgeExecutor::new(library).execute(&plan)?;
    println!(
        "Purged {} file(s); {} were already missing",
        report.purged_files, report.already_missing
    );
    Ok(if report.already_missing == 0 {
        Outcome::Success
    } else {
        Outcome::Partial
    })
}

fn modify(library: &Library, raw_query: &str, fields: &[String]) -> Result<Outcome> {
    require_selection(raw_query, "modify")?;
    let query = Query::parse(raw_query)?;
    let items = library.query_items_bounded(&query, 9_999)?;
    let ids = items
        .iter()
        .map(|item| {
            item.id
                .ok_or_else(|| Error::Query("a matched item has no database ID".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let count = library.modify_items(&ids, fields)?;
    outln!(streams, "Modified {count} item(s)");
    Ok(Outcome::Success)
}

fn provider_refresh(
    library: &Library,
    raw_kind: &str,
    raw_id: &str,
    dry_run: bool,
    yes: bool,
    output: OutputFormat,
) -> Result<Outcome> {
    let kind = parse_entity_kind(raw_kind)?;
    let id = EntityId::parse(raw_id.to_owned())?;
    let plan = if dry_run {
        library.preview_provider_refresh(kind, &id)?
    } else {
        library.plan_provider_refresh(kind, &id)?
    };
    if dry_run {
        if output == OutputFormat::Text {
            println!(
                "Provider refresh: {} field-level change(s)",
                plan.diffs().len()
            );
            for diff in plan.diffs() {
                println!(
                    "  {}: {:?} -> {:?}",
                    diff.field(),
                    diff.before(),
                    diff.after()
                );
            }
            println!("Dry run: canonical values and media tags are unchanged");
        } else {
            emit(
                output,
                &serde_json::json!({"plan": plan, "executed": false}),
            )?;
        }
        return Ok(Outcome::Success);
    }
    if !yes && !confirm("Apply every reviewed canonical field change (media tags stay unchanged)?")?
    {
        return Ok(Outcome::Partial);
    }
    library.approve_provider_refresh(&plan)?;
    library.execute_provider_refresh(&plan)?;
    if output == OutputFormat::Text {
        println!(
            "Applied {} canonical field change(s); media tags unchanged; plan {}",
            plan.diffs().len(),
            plan.id().as_str()
        );
    } else {
        emit(
            output,
            &serde_json::json!({"plan": plan, "state": "complete"}),
        )?;
    }
    Ok(Outcome::Success)
}

#[allow(clippy::too_many_arguments)]
fn tag_project(
    library: &mut Library,
    item_id: i64,
    title: String,
    artists: Vec<String>,
    album: String,
    album_artists: Vec<String>,
    raw_profile: &str,
    dry_run: bool,
    yes: bool,
    output: OutputFormat,
) -> Result<Outcome> {
    let profile = parse_tag_profile(raw_profile)?;
    let tags = CanonicalTags::new(title, artists, album, album_artists)?;
    let plan = if dry_run {
        library.preview_tag_projection(item_id, tags, profile)?
    } else {
        library.plan_tag_projection(item_id, tags, profile)?
    };
    if dry_run {
        if output == OutputFormat::Text {
            println!(
                "Tag projection {}: {} (original retained)",
                plan.id().as_str(),
                terminal_safe(plan.path().display())
            );
        } else {
            emit(
                output,
                &serde_json::json!({"plan": plan, "executed": false}),
            )?;
        }
        return Ok(Outcome::Success);
    }
    if !yes && !confirm("Write this tag projection and retain the original file?")? {
        return Ok(Outcome::Partial);
    }
    library.approve_tag_projection(&plan)?;
    let receipt = TagProjectionExecutor::new(library).execute(&plan)?;
    if output == OutputFormat::Text {
        println!(
            "Tag projection complete; original retained at {}",
            terminal_safe(receipt.retained_original().display())
        );
    } else {
        emit(output, &receipt)?;
    }
    Ok(Outcome::Success)
}

#[allow(clippy::too_many_arguments)]
fn path_project(
    library: &mut Library,
    asset_id: &str,
    destination: &std::path::Path,
    raw_profile: &str,
    dry_run: bool,
    yes: bool,
    output: OutputFormat,
) -> Result<Outcome> {
    let profile = parse_naming_profile(raw_profile)?;
    let plan = if dry_run {
        library.preview_path_projection(asset_id, destination, profile)?
    } else {
        library.plan_path_projection(asset_id, destination, profile)?
    };
    if dry_run {
        if output == OutputFormat::Text {
            println!(
                "Path projection {}: {} -> {}",
                plan.id().as_str(),
                terminal_safe(plan.source().display()),
                terminal_safe(plan.destination().display())
            );
        } else {
            emit(
                output,
                &serde_json::json!({"plan": plan, "executed": false}),
            )?;
        }
        return Ok(Outcome::Success);
    }
    if !yes && !confirm("Apply this journaled no-clobber path projection?")? {
        return Ok(Outcome::Partial);
    }
    library.approve_path_projection(&plan)?;
    let receipt = library.execute_path_projection(&plan)?;
    if output == OutputFormat::Text {
        println!(
            "Path projection complete: {}",
            terminal_safe(plan.destination().display())
        );
    } else {
        emit(output, &receipt)?;
    }
    Ok(Outcome::Success)
}

#[expect(
    clippy::too_many_lines,
    reason = "each fixity subcommand is a small view over one durable state-machine transition"
)]
fn fixity_command(
    library: &Library,
    action: &FixityCommand,
    output: OutputFormat,
) -> Result<Outcome> {
    match action {
        FixityCommand::Plan { quick } => {
            let mode = if *quick {
                FixityMode::Quick
            } else {
                FixityMode::Deep
            };
            let plan = library.plan_fixity(mode)?;
            if output == OutputFormat::Text {
                println!(
                    "Fixity plan {} ({mode:?}, {} managed assets)",
                    plan.id().as_str(),
                    plan.asset_count()
                );
            } else {
                emit(output, &plan)?;
            }
        }
        FixityCommand::Approve { id } => {
            let plan = library.fixity_plan(&PlanId::parse(id.clone())?)?;
            library.approve_fixity(&plan)?;
            if output == OutputFormat::Text {
                println!("Approved fixity plan {}", plan.id().as_str());
            } else {
                emit(output, &library.durable_plan(plan.id())?)?;
            }
        }
        FixityCommand::Run { id, page_size } => {
            let plan = library.fixity_plan(&PlanId::parse(id.clone())?)?;
            let progress = library.run_fixity_page(&plan, *page_size)?;
            if output == OutputFormat::Text {
                for result in progress.results() {
                    println!(
                        "{} {:?}: {}{}",
                        terminal_safe(result.asset_id()),
                        result.state(),
                        terminal_safe(result.path().display()),
                        result.detail().map_or_else(String::new, |detail| format!(
                            ": {}",
                            terminal_safe(detail)
                        ))
                    );
                }
                println!(
                    "Fixity {}: {}/{} checked, {} failure(s), complete={}",
                    plan.id().as_str(),
                    progress.checked(),
                    progress.total(),
                    progress.failures(),
                    progress.complete()
                );
            } else if output == OutputFormat::Json {
                emit(output, &progress)?;
            } else {
                for result in progress.results() {
                    emit(output, result)?;
                }
                emit(
                    output,
                    &serde_json::json!({
                        "summary": {
                            "plan_id": plan.id(),
                            "checked": progress.checked(),
                            "total": progress.total(),
                            "failures": progress.failures(),
                            "complete": progress.complete(),
                            "cursor": progress.cursor(),
                        }
                    }),
                )?;
            }
            return Ok(if progress.failures() == 0 {
                Outcome::Success
            } else {
                Outcome::Partial
            });
        }
        FixityCommand::Results {
            id,
            after_asset_id,
            limit,
        } => {
            let id = PlanId::parse(id.clone())?;
            let results = library.fixity_results_page(&id, after_asset_id.as_deref(), *limit)?;
            if output == OutputFormat::Json {
                emit(output, &results)?;
            } else if output == OutputFormat::Jsonl {
                for result in &results {
                    emit(output, result)?;
                }
            } else {
                for result in &results {
                    println!(
                        "{} {:?}: {}",
                        terminal_safe(result.asset_id()),
                        result.state(),
                        terminal_safe(result.path().display())
                    );
                }
            }
            return Ok(
                if results
                    .iter()
                    .all(|result| result.state() == rsbts::fixity::FixityResultState::Ok)
                {
                    Outcome::Success
                } else {
                    Outcome::Partial
                },
            );
        }
        FixityCommand::Schedule {
            interval_seconds,
            quick,
        } => {
            let mode = if *quick {
                FixityMode::Quick
            } else {
                FixityMode::Deep
            };
            let id = library.schedule_fixity(
                mode,
                std::time::Duration::from_secs(*interval_seconds),
                Utc::now(),
            )?;
            if output == OutputFormat::Text {
                println!("Created {mode:?} fixity schedule {}", id.as_str());
            } else {
                emit(
                    output,
                    &serde_json::json!({"schedule_id": id, "mode": mode}),
                )?;
            }
        }
        FixityCommand::Due { limit } => {
            let plans = library.plan_due_fixity(Utc::now(), *limit)?;
            if output == OutputFormat::Json {
                emit(output, &plans)?;
            } else if output == OutputFormat::Jsonl {
                for plan in &plans {
                    emit(output, plan)?;
                }
            } else {
                for plan in &plans {
                    println!(
                        "Due fixity plan {} ({:?}, {} assets)",
                        plan.id().as_str(),
                        plan.mode(),
                        plan.asset_count()
                    );
                }
            }
        }
        FixityCommand::Schedules { after, limit } => {
            let after = after.clone().map(FixityScheduleId::parse).transpose()?;
            let schedules = library.fixity_schedules_page(after.as_ref(), *limit)?;
            if output == OutputFormat::Json {
                emit(output, &schedules)?;
            } else if output == OutputFormat::Jsonl {
                for schedule in &schedules {
                    emit(output, schedule)?;
                }
            } else {
                for schedule in &schedules {
                    println!(
                        "{} {:?} every {}s enabled={} next={}",
                        schedule.id().as_str(),
                        schedule.mode(),
                        schedule.interval_seconds(),
                        schedule.enabled(),
                        schedule.next_run_at()
                    );
                }
            }
        }
        FixityCommand::Enable {
            id,
            disable,
            enable: _enable,
        } => {
            let id = FixityScheduleId::parse(id.clone())?;
            let enabled = !disable;
            library.set_fixity_schedule_enabled(&id, enabled)?;
            if output == OutputFormat::Text {
                println!(
                    "Fixity schedule {} {}",
                    id.as_str(),
                    if enabled { "enabled" } else { "disabled" }
                );
            } else {
                emit(
                    output,
                    &serde_json::json!({"schedule_id": id, "enabled": enabled}),
                )?;
            }
        }
        FixityCommand::History {
            schedule_id,
            after_plan_id,
            limit,
        } => {
            let schedule_id = FixityScheduleId::parse(schedule_id.clone())?;
            let after = after_plan_id.clone().map(PlanId::parse).transpose()?;
            let history = library.scheduled_fixity_history(&schedule_id, after.as_ref(), *limit)?;
            if output == OutputFormat::Json {
                emit(output, &history)?;
            } else if output == OutputFormat::Jsonl {
                for run in &history {
                    emit(output, run)?;
                }
            } else {
                for run in &history {
                    println!(
                        "{} {:?}: {} checked, {} failure(s)",
                        run.plan_id().as_str(),
                        run.state(),
                        run.checked(),
                        run.failures()
                    );
                }
            }
        }
    }
    Ok(Outcome::Success)
}

fn plan_command(library: &Library, action: &PlanCommand, output: OutputFormat) -> Result<Outcome> {
    let id = match &action {
        PlanCommand::Status { id }
        | PlanCommand::Events { id }
        | PlanCommand::Cancel { id }
        | PlanCommand::Resume { id } => PlanId::parse(id.clone())?,
    };
    match action {
        PlanCommand::Status { .. } => {
            let plan = library.durable_plan(&id)?;
            if output == OutputFormat::Text {
                println!(
                    "{} {} {:?} progress {:?} cursor {:?}",
                    plan.id().as_str(),
                    plan.kind(),
                    plan.state(),
                    plan.progress(),
                    plan.resume_cursor()
                );
            } else {
                emit(output, &plan)?;
            }
        }
        PlanCommand::Events { .. } => {
            let events = library.plan_events(&id)?;
            if output == OutputFormat::Json {
                emit(output, &events)?;
            } else if output == OutputFormat::Jsonl {
                for event in &events {
                    emit(output, event)?;
                }
            } else {
                for event in events {
                    println!(
                        "{} {} {}",
                        event.sequence(),
                        terminal_safe(event.event_type()),
                        terminal_safe(event.detail())
                    );
                }
            }
        }
        PlanCommand::Cancel { .. } => {
            library.request_plan_cancellation(&id)?;
            if output == OutputFormat::Text {
                println!("Cancellation requested for {}", id.as_str());
            } else {
                emit(output, &library.durable_plan(&id)?)?;
            }
        }
        PlanCommand::Resume { .. } => {
            library.resume_durable_plan(&id)?;
            if output == OutputFormat::Text {
                println!(
                    "Plan {} marked running for its executor to resume",
                    id.as_str()
                );
            } else {
                emit(output, &library.durable_plan(&id)?)?;
            }
        }
    }
    Ok(Outcome::Success)
}

fn parse_entity_kind(value: &str) -> Result<EntityKind> {
    match value {
        "release-group" => Ok(EntityKind::ReleaseGroup),
        "release" => Ok(EntityKind::Release),
        "medium" => Ok(EntityKind::Medium),
        "release-track" => Ok(EntityKind::ReleaseTrack),
        "recording" => Ok(EntityKind::Recording),
        "work" => Ok(EntityKind::Work),
        "artist" => Ok(EntityKind::Artist),
        "label" => Ok(EntityKind::Label),
        "asset" => Ok(EntityKind::Asset),
        _ => Err(Error::Catalog(format!("unknown entity kind: {value}"))),
    }
}

fn parse_tag_profile(value: &str) -> Result<TagProfile> {
    match value {
        "archival-native-rich" => Ok(TagProfile::ArchivalNativeRich),
        "picard-navidrome" => Ok(TagProfile::PicardNavidrome),
        "id3v2.3-legacy" => Ok(TagProfile::Id3v23Legacy),
        "portable-player" => Ok(TagProfile::PortablePlayer),
        _ => Err(Error::Operation(format!("unknown tag profile: {value}"))),
    }
}

fn parse_naming_profile(value: &str) -> Result<NamingProfile> {
    match value {
        "portable" => Ok(NamingProfile::Portable),
        "native-filesystem" => Ok(NamingProfile::NativeFilesystem),
        "archival" => Ok(NamingProfile::Archival),
        _ => Err(Error::PathFormat(format!(
            "unknown naming profile: {value}"
        ))),
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()
        .map_err(|error| Error::Operation(format!("cannot read confirmation: {error}")))
}

fn emit<T: serde::Serialize>(output: OutputFormat, value: &T) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match output {
        OutputFormat::Text => {
            return Err(Error::Operation(
                "internal error: structured emitter used for text output".into(),
            ));
        }
        OutputFormat::Json => serde_json::to_writer_pretty(&mut lock, value)?,
        OutputFormat::Jsonl => serde_json::to_writer(&mut lock, value)?,
    }
    lock.write_all(b"\n")?;
    Ok(())
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
    let total_seconds = seconds.max(0.0).to_u64().unwrap_or(u64::MAX);
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
        format!(
            "{:.1} GiB",
            bytes.to_f64().unwrap_or(f64::MAX) / 1_073_741_824.0
        )
    } else if bytes >= MIB {
        format!(
            "{:.1} MiB",
            bytes.to_f64().unwrap_or(f64::MAX) / 1_048_576.0
        )
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes.to_f64().unwrap_or(f64::MAX) / 1_024.0)
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
    use std::io::{self, Write};

    use super::{terminal_safe, CliError, Streams};

    struct BrokenWriter;

    impl Write for BrokenWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn terminal_output_escapes_control_characters() {
        assert_eq!(terminal_safe("title\n\u{1b}[2J"), "title\\n\\u{1b}[2J");
    }

    #[test]
    fn stdout_broken_pipe_is_distinct_from_other_io_errors(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut stdout = BrokenWriter;
        let mut stderr = Vec::new();
        let error = match Streams::new(&mut stdout, &mut stderr).output(format_args!("record")) {
            Ok(()) => return Err("the synthetic writer unexpectedly accepted output".into()),
            Err(error) => error,
        };
        assert!(error.is_stdout_broken_pipe());

        let application = CliError::Application(rsbts::Error::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "application pipe",
        )));
        assert!(!application.is_stdout_broken_pipe());
        Ok(())
    }

    #[test]
    fn output_and_diagnostics_use_separate_streams() -> Result<(), CliError> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut streams = Streams::new(&mut stdout, &mut stderr);
        streams.output(format_args!("record"))?;
        streams.diagnostic(format_args!("warning"))?;
        assert_eq!(stdout, b"record\n");
        assert_eq!(stderr, b"warning\n");
        Ok(())
    }
}
