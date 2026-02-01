//! Import workflow

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Constants for track matching and scoring algorithms.
mod matching {
    /// Bonus score when track count matches release track count.
    pub const TRACK_COUNT_BONUS: f64 = 0.2;
    /// Multiplier for converting similarity scores to integer comparison values.
    pub const SCORE_MULTIPLIER: f64 = 100.0;

    /// Constants for track length comparison.
    pub mod length {
        /// Perfect match threshold in milliseconds.
        pub const PERFECT_THRESHOLD_MS: f64 = 3000.0;
        /// Good match threshold in milliseconds.
        pub const GOOD_THRESHOLD_MS: f64 = 10000.0;
        /// Score for perfect length match.
        pub const PERFECT_SCORE: f64 = 1.0;
        /// Score for good length match.
        pub const GOOD_SCORE: f64 = 0.7;
        /// Score for poor length match.
        pub const POOR_SCORE: f64 = 0.3;
        /// Score when length is unknown.
        pub const UNKNOWN_SCORE: f64 = 0.5;
    }

    /// Constants for cost matrix calculation.
    pub mod cost {
        /// Multiplier for similarity to cost conversion.
        pub const SIMILARITY_MULTIPLIER: f64 = -5000.0;
        /// Base offset for cost calculation.
        pub const BASE_OFFSET: f64 = 10000.0;
        /// Seconds to milliseconds conversion factor.
        pub const SECONDS_TO_MS: f64 = 1000.0;
    }
}

use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::db::Database;
use crate::musicbrainz::{Client as MbClient, Release};
use crate::pathformat::format_path;
use crate::tags::{is_audio_file, read_tags};
use crate::{Album, Item, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Copy,
    Move,
    Link,
}

pub struct ImportConfig {
    pub action: Action,
    pub fetch_art: bool,
    pub path_format: String,
    pub library_dir: PathBuf,
}

pub struct Importer<'a> {
    db: &'a Database,
    config: ImportConfig,
    mb: MbClient,
}

#[derive(Debug)]
struct AlbumCandidate {
    items: Vec<Item>,
    artist: String,
    album: String,
}

impl<'a> Importer<'a> {
    /// Create a new importer.
    ///
    /// # Errors
    /// Returns an error if the HTTP client cannot be created.
    pub fn new(db: &'a Database, config: ImportConfig) -> Result<Self> {
        Ok(Self {
            db,
            config,
            mb: MbClient::new()?,
        })
    }

    /// Import audio files from the given path.
    ///
    /// # Errors
    /// Returns an error if scanning or importing fails.
    // rusqlite::Connection is not Sync, so futures holding &Database aren't Send
    #[allow(clippy::future_not_send)]
    pub async fn import(&self, path: &Path) -> Result<()> {
        let items = scan(path);
        if items.is_empty() {
            println!("No audio files found in {}", path.display());
            return Ok(());
        }

        let candidates = group_into_albums(items);

        for candidate in candidates {
            self.process_candidate(candidate).await?;
        }

        Ok(())
    }

    // rusqlite::Connection is not Sync, so futures holding &Database aren't Send
    #[allow(clippy::future_not_send)]
    async fn process_candidate(&self, candidate: AlbumCandidate) -> Result<()> {
        println!(
            "\nImporting: {} - {} ({} tracks)",
            candidate.artist,
            candidate.album,
            candidate.items.len()
        );

        let release_info = self.lookup_release(&candidate).await?;
        let album = Self::create_album(&candidate, release_info.as_ref());
        let album_id = self.db.insert_album(&album)?;

        self.fetch_and_save_cover_art(&album, release_info.as_ref())
            .await;

        let matched_items = Self::match_items_to_release(candidate.items, release_info.as_ref());
        self.import_items(matched_items, album_id)?;

        println!("  Imported successfully");
        Ok(())
    }

    /// Look up release information from `MusicBrainz`.
    #[allow(clippy::future_not_send)]
    async fn lookup_release(&self, candidate: &AlbumCandidate) -> Result<Option<Release>> {
        let releases = self
            .mb
            .search_release(&candidate.artist, &candidate.album, 5)
            .await?;

        if releases.is_empty() {
            println!("  No MusicBrainz matches found, importing as-is");
            return Ok(None);
        }

        let Some(best) = pick_best_match(candidate, &releases) else {
            return Ok(None);
        };

        println!(
            "  Matched: {} - {} ({})",
            best.artist_name(),
            best.title,
            best.year().map_or_else(|| "????".into(), |y| y.to_string())
        );

        let release = self.mb.lookup_release(&best.id).await?;
        Ok(Some(release))
    }

    /// Create an Album struct from candidate and optional release info.
    fn create_album(candidate: &AlbumCandidate, release: Option<&Release>) -> Album {
        Album {
            id: None,
            album: release.map_or_else(|| candidate.album.clone(), |r| r.title.clone()),
            albumartist: release.map_or_else(|| candidate.artist.clone(), Release::artist_name),
            year: release.and_then(Release::year),
            artpath: None,
            mb_albumid: release.map(|r| r.id.clone()),
            added: chrono::Utc::now(),
        }
    }

    /// Fetch and save cover art if configured and available.
    #[allow(clippy::future_not_send)]
    async fn fetch_and_save_cover_art(&self, album: &Album, release: Option<&Release>) {
        if !self.config.fetch_art {
            return;
        }

        let Some(release) = release else {
            return;
        };

        let Ok(Some(art)) = self.mb.fetch_cover_art(&release.id).await else {
            return;
        };

        let art_path = self.config.library_dir.join(format!(
            "{}/{}/cover.jpg",
            album.albumartist, album.album
        ));

        if let Some(parent) = art_path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }

        if std::fs::write(&art_path, art).is_ok() {
            println!("  Downloaded cover art");
        }
    }

    /// Match items to release tracks if release info is available.
    fn match_items_to_release(items: Vec<Item>, release: Option<&Release>) -> Vec<Item> {
        match release {
            Some(rel) => match_tracks(items, rel),
            None => items,
        }
    }

    /// Import matched items into the database.
    fn import_items(&self, items: Vec<Item>, album_id: i64) -> Result<()> {
        for mut item in items {
            if self.db.item_exists(&item.path)? {
                continue;
            }

            item.album_id = Some(album_id);

            let dest = self.destination_path(&item)?;

            self.transfer_file(&item.path, &dest)?;
            item.path = dest;

            self.db.insert_item(&item)?;
        }
        Ok(())
    }

    fn destination_path(&self, item: &Item) -> Result<PathBuf> {
        let relative = format_path(&self.config.path_format, item)?;
        let ext = item
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp3");
        Ok(self.config.library_dir.join(format!("{relative}.{ext}")))
    }

    fn transfer_file(&self, src: &Path, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        match self.config.action {
            Action::Copy => {
                std::fs::copy(src, dest)?;
            }
            Action::Move => {
                if std::fs::rename(src, dest).is_err() {
                    std::fs::copy(src, dest)?;
                    std::fs::remove_file(src)?;
                }
            }
            Action::Link => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(src, dest)?;
                #[cfg(not(unix))]
                std::fs::copy(src, dest)?;
            }
        }

        Ok(())
    }
}

/// Trait for reporting scan progress.
pub trait ScanProgress: Sync {
    /// Called when files have been found.
    fn on_files_found(&self, count: usize);
    /// Called periodically during scanning.
    fn tick(&self);
    /// Called when scanning is complete.
    fn finish(&self, track_count: usize);
}

/// Console-based progress reporter using indicatif.
pub struct ConsoleProgress {
    bar: ProgressBar,
}

impl ConsoleProgress {
    /// Create a new console progress reporter.
    #[must_use]
    pub fn new() -> Self {
        let bar = ProgressBar::new_spinner();
        if let Ok(style) =
            ProgressStyle::default_spinner().template("{spinner:.green} Scanning: {msg}")
        {
            bar.set_style(style);
        }
        Self { bar }
    }
}

impl Default for ConsoleProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanProgress for ConsoleProgress {
    fn on_files_found(&self, count: usize) {
        self.bar.set_message(format!("Found {count} files"));
    }

    fn tick(&self) {
        self.bar.tick();
    }

    fn finish(&self, track_count: usize) {
        self.bar
            .finish_with_message(format!("Scanned {track_count} tracks"));
    }
}

/// No-op progress reporter for testing or silent operation.
pub struct NoProgress;

impl ScanProgress for NoProgress {
    fn on_files_found(&self, _count: usize) {}
    fn tick(&self) {}
    fn finish(&self, _track_count: usize) {}
}

fn scan(path: &Path) -> Vec<Item> {
    let progress = ConsoleProgress::new();
    scan_with_progress(path, &progress)
}

fn scan_with_progress<P: ScanProgress>(path: &Path, progress: &P) -> Vec<Item> {
    let files: Vec<PathBuf> = WalkDir::new(path)
        .follow_links(true)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| is_audio_file(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();

    progress.on_files_found(files.len());

    let items: Vec<Item> = files
        .par_iter()
        .filter_map(|p| {
            progress.tick();
            read_tags(p).ok()
        })
        .collect();

    progress.finish(items.len());
    items
}

fn group_into_albums(items: Vec<Item>) -> Vec<AlbumCandidate> {
    let mut groups: HashMap<(String, String), Vec<Item>> = HashMap::new();

    for item in items {
        let key = (
            item.effective_albumartist().to_lowercase(),
            item.album.to_lowercase(),
        );
        groups.entry(key).or_default().push(item);
    }

    groups
        .into_iter()
        .map(|((artist, album), items)| AlbumCandidate {
            artist: items
                .first()
                .map_or(artist, |i| i.effective_albumartist().to_string()),
            album: items.first().map_or(album, |i| i.album.clone()),
            items,
        })
        .collect()
}

fn pick_best_match<'b>(candidate: &AlbumCandidate, releases: &'b [Release]) -> Option<&'b Release> {
    releases.iter().max_by_key(|r| {
        let artist_sim = strsim::jaro_winkler(&candidate.artist, &r.artist_name());
        let album_sim = strsim::jaro_winkler(&candidate.album, &r.title);
        let track_count_match = if r.tracks().len() == candidate.items.len() {
            matching::TRACK_COUNT_BONUS
        } else {
            0.0
        };
        (artist_sim + album_sim + track_count_match)
            .mul_add(matching::SCORE_MULTIPLIER, 0.0)
            .clamp(0.0, f64::from(u32::MAX)) as u32
    })
}

fn match_tracks(mut items: Vec<Item>, release: &Release) -> Vec<Item> {
    let tracks = release.tracks();
    if tracks.is_empty() {
        return items;
    }

    let n = items.len().max(tracks.len());
    let mut matrix = vec![vec![0i64; n]; n];

    for (i, item) in items.iter().enumerate() {
        for (j, track) in tracks.iter().enumerate() {
            let title_dist = strsim::jaro_winkler(&item.title, &track.title);
            let length_dist = track.length.map_or(matching::length::UNKNOWN_SCORE, |tl| {
                // tl is track length in ms (u64→f64 precision loss acceptable for comparison)
                let diff = item
                    .length
                    .mul_add(matching::cost::SECONDS_TO_MS, -(tl as f64))
                    .abs();
                if diff < matching::length::PERFECT_THRESHOLD_MS {
                    matching::length::PERFECT_SCORE
                } else if diff < matching::length::GOOD_THRESHOLD_MS {
                    matching::length::GOOD_SCORE
                } else {
                    matching::length::POOR_SCORE
                }
            });
            let cost = (title_dist + length_dist)
                .mul_add(
                    matching::cost::SIMILARITY_MULTIPLIER,
                    matching::cost::BASE_OFFSET,
                )
                .round() as i64;
            matrix[i][j] = cost;
        }
    }

    let Ok(matrix_obj) = pathfinding::matrix::Matrix::from_rows(matrix) else {
        return items; // Return unmatched if matrix construction fails
    };
    let assignment = pathfinding::kuhn_munkres::kuhn_munkres_min(&matrix_obj);

    for (item_idx, track_idx) in assignment.1.iter().enumerate() {
        if item_idx < items.len() && *track_idx < tracks.len() {
            let track = &tracks[*track_idx];
            items[item_idx].title.clone_from(&track.title);
            items[item_idx].mb_trackid = Some(track.recording.id.clone());
        }
    }

    items
}
