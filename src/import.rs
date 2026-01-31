//! Import workflow

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    pub write_tags: bool,
    pub fetch_art: bool,
    pub embed_art: bool,
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
    #[must_use]
    pub fn new(db: &'a Database, config: ImportConfig) -> Self {
        Self {
            db,
            config,
            mb: MbClient::new(),
        }
    }

    /// Import audio files from the given path.
    ///
    /// # Errors
    /// Returns an error if scanning or importing fails.
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

    #[allow(clippy::future_not_send)]
    async fn process_candidate(&self, candidate: AlbumCandidate) -> Result<()> {
        println!(
            "\nImporting: {} - {} ({} tracks)",
            candidate.artist,
            candidate.album,
            candidate.items.len()
        );

        let releases = self
            .mb
            .search_release(&candidate.artist, &candidate.album, 5)
            .await?;

        let matched_release = if releases.is_empty() {
            println!("  No MusicBrainz matches found, importing as-is");
            None
        } else {
            let best = pick_best_match(&candidate, &releases);
            best.inspect(|release| {
                println!(
                    "  Matched: {} - {} ({})",
                    release.artist_name(),
                    release.title,
                    release.year().map_or_else(|| "????".into(), |y| y.to_string())
                );
            })
        };

        let release_info = if let Some(release) = matched_release {
            Some(self.mb.lookup_release(&release.id).await?)
        } else {
            None
        };

        let album = Album {
            id: None,
            album: release_info
                .as_ref()
                .map_or_else(|| candidate.album.clone(), |r| r.title.clone()),
            albumartist: release_info
                .as_ref()
                .map_or_else(|| candidate.artist.clone(), Release::artist_name),
            year: release_info.as_ref().and_then(Release::year),
            artpath: None,
            mb_albumid: release_info.as_ref().map(|r| r.id.clone()),
            added: chrono::Utc::now(),
        };
        let album_id = self.db.insert_album(&album)?;

        if self.config.fetch_art {
            if let Some(ref release) = release_info {
                if let Ok(Some(art)) = self.mb.fetch_cover_art(&release.id).await {
                    let art_path = self.config.library_dir.join(format!(
                        "{}/{}/cover.jpg",
                        album.albumartist, album.album
                    ));
                    if let Some(parent) = art_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&art_path, art)?;
                    println!("  Downloaded cover art");
                }
            }
        }

        let matched_items = release_info
            .as_ref()
            .map_or_else(
                || candidate.items.clone(),
                |release| match_tracks(candidate.items.clone(), release),
            );

        for mut item in matched_items {
            if self.db.item_exists(&item.path)? {
                continue;
            }

            item.album_id = Some(album_id);

            let dest = self.destination_path(&item)?;

            self.transfer_file(&item.path, &dest)?;
            item.path = dest;

            self.db.insert_item(&item)?;
        }

        println!("  Imported successfully");
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

fn scan(path: &Path) -> Vec<Item> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} Scanning: {msg}")
            .unwrap(),
    );

    let files: Vec<PathBuf> = WalkDir::new(path)
        .follow_links(true)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| is_audio_file(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();

    spinner.set_message(format!("Found {} files", files.len()));

    let items: Vec<Item> = files
        .par_iter()
        .filter_map(|p| {
            spinner.tick();
            read_tags(p).ok()
        })
        .collect();

    spinner.finish_with_message(format!("Scanned {} tracks", items.len()));
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
            0.2
        } else {
            0.0
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let score = ((artist_sim + album_sim + track_count_match) * 100.0) as u32;
        score
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
            #[allow(clippy::cast_precision_loss)]
            let length_dist = track.length.map_or(0.5, |tl| {
                let diff = item.length.mul_add(1000.0, -(tl as f64)).abs();
                if diff < 3000.0 {
                    1.0
                } else if diff < 10000.0 {
                    0.7
                } else {
                    0.3
                }
            });
            #[allow(clippy::cast_possible_truncation)]
            let cost = (title_dist + length_dist).mul_add(-5000.0, 10000.0) as i64;
            matrix[i][j] = cost;
        }
    }

    let assignment = pathfinding::kuhn_munkres::kuhn_munkres_min(
        &pathfinding::matrix::Matrix::from_rows(matrix).unwrap(),
    );

    for (item_idx, track_idx) in assignment.1.iter().enumerate() {
        if item_idx < items.len() && *track_idx < tracks.len() {
            let track = &tracks[*track_idx];
            items[item_idx].title.clone_from(&track.title);
            items[item_idx].mb_trackid = Some(track.recording.id.clone());
        }
    }

    items
}
