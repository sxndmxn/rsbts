use std::path::PathBuf;

use anyhow::{Context, Result};

use rsbts::config::Config;
use rsbts::db::Database;
use rsbts::import::Action;

use crate::Commands;

// rusqlite::Connection is not Sync, so futures holding &Database aren't Send
#[allow(clippy::future_not_send)]
pub async fn run(command: Commands, config_path: Option<PathBuf>) -> Result<()> {
    let config = Config::load(config_path.as_deref())?;
    let db = Database::open(&config.library.database)?;
    db.migrate()?;

    match command {
        Commands::Import { paths, copy, r#move } => {
            let action = if copy {
                Action::Copy
            } else if r#move {
                Action::Move
            } else {
                config.import.action
            };
            import(&db, &config, &paths, action).await?;
        }
        Commands::List { query, album } => {
            list(&db, query.as_deref(), album)?;
        }
        Commands::Stats => {
            stats(&db)?;
        }
        Commands::Update { query } => {
            update(&db, query.as_deref())?;
        }
        Commands::Remove { query, delete } => {
            remove(&db, &query, delete)?;
        }
        Commands::Modify { query, fields } => {
            modify(&db, &query, &fields)?;
        }
    }

    Ok(())
}

// rusqlite::Connection is not Sync, so futures holding &Database aren't Send
#[allow(clippy::future_not_send)]
async fn import(
    db: &Database,
    config: &Config,
    paths: &[PathBuf],
    action: Action,
) -> Result<()> {
    use rsbts::import::{ImportConfig, Importer};

    let import_config = ImportConfig {
        action,
        write_tags: config.import.write_tags,
        fetch_art: config.import.fetch_art,
        embed_art: config.import.embed_art,
        path_format: config.paths.format.clone(),
        library_dir: config.library.directory.clone(),
    };

    let importer = Importer::new(db, import_config);

    for path in paths {
        importer
            .import(path)
            .await
            .with_context(|| format!("Failed to import {}", path.display()))?;
    }

    Ok(())
}

fn list(db: &Database, query: Option<&str>, album: bool) -> Result<()> {
    if album {
        let albums = db.query_albums(query)?;
        for album in albums {
            let year = album.year.map_or_else(String::new, |y| format!(" ({y})"));
            println!("{} - {}{}", album.albumartist, album.album, year);
        }
    } else {
        let items = db.query_items(query)?;
        for item in items {
            let duration = format_duration(item.length);
            println!(
                "{} - {} - {} [{}]",
                item.artist, item.album, item.title, duration
            );
        }
    }
    Ok(())
}

fn stats(db: &Database) -> Result<()> {
    let stats = db.stats()?;
    println!("Tracks: {}", stats.tracks);
    println!("Albums: {}", stats.albums);
    println!("Artists: {}", stats.artists);
    println!("Total time: {}", format_duration(stats.total_length));
    println!("Total size: {}", format_size(stats.total_size));
    Ok(())
}

fn update(db: &Database, query: Option<&str>) -> Result<()> {
    let items = db.query_items(query)?;
    let count = items.len();

    for item in items {
        if let Some(id) = item.id {
            if let Ok(updated) = rsbts::tags::read_tags(&item.path) {
                db.update_item(id, &updated)?;
            }
        }
    }

    println!("Updated {count} items");
    Ok(())
}

fn remove(db: &Database, query: &str, delete: bool) -> Result<()> {
    let items = db.query_items(Some(query))?;
    let count = items.len();

    for item in &items {
        if let Some(id) = item.id {
            db.remove_item(id)?;
        }
        if delete {
            if let Err(e) = std::fs::remove_file(&item.path) {
                eprintln!("Warning: failed to delete {}: {e}", item.path.display());
            }
        }
    }

    println!("Removed {count} items");
    Ok(())
}

fn modify(db: &Database, query: &str, fields: &[String]) -> Result<()> {
    let items = db.query_items(Some(query))?;
    let count = items.len();

    for item in items {
        if let Some(id) = item.id {
            db.modify_item(id, fields)?;
        }
    }

    println!("Modified {count} items");
    Ok(())
}

fn format_duration(seconds: f64) -> String {
    // Negative durations are invalid; clamp to 0
    let total_secs = seconds.max(0.0) as u64;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
