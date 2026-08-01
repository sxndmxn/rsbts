//! Plan-first, album-atomic import workflow.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::Utc;
use indicatif::{ProgressBar, ProgressStyle};
use pathfinding::matrix::Matrix;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::db::{hash_path, JournalFile, Library, OperationKind};
use crate::pathformat::format_relative_path;
use crate::provider::{MetadataProvider, ProviderTrack, ReleaseCandidate, ReleaseQuery};
use crate::tags::{is_audio_file, read_tags};
use crate::{Album, Error, ExternalId, Item, Result};

const ARTIST_WEIGHT: f64 = 0.25;
const ALBUM_WEIGHT: f64 = 0.25;
const TRACK_WEIGHT: f64 = 0.40;
const PROVIDER_WEIGHT: f64 = 0.10;
const ARTIST_GATE: f64 = 0.95;
const ALBUM_GATE: f64 = 0.92;
const TRACK_TITLE_GATE: f64 = 0.90;
const DURATION_GATE_SECONDS: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Copy,
    Move,
    Link,
}

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub action: Action,
    pub fetch_art: bool,
    pub follow_symlinks: bool,
    pub path_format: String,
    pub library_dir: PathBuf,
    pub search_limit: u32,
    pub auto_accept_threshold: f64,
    pub runner_up_margin: f64,
}

#[derive(Debug, Clone)]
pub struct ScanIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct ImportPlan {
    pub albums: Vec<AlbumPlan>,
    pub scan_issues: Vec<ScanIssue>,
}

#[derive(Debug, Clone)]
pub struct AlbumPlan {
    pub source_artist: String,
    pub source_album: String,
    pub items: Vec<Item>,
    pub candidates: Vec<ScoredCandidate>,
    pub lookup_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub release: ReleaseCandidate,
    pub confidence: ConfidenceBreakdown,
    pub track_matches: Vec<TrackMatch>,
}

#[derive(Debug, Clone)]
pub struct ConfidenceBreakdown {
    pub artist: f64,
    pub album: f64,
    pub mean_track: f64,
    pub provider: f64,
    pub composite: f64,
    pub runner_up_margin: f64,
    pub high_confidence: bool,
    pub gate_failures: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TrackMatch {
    pub item_index: usize,
    pub track_index: usize,
    pub title_similarity: f64,
    pub duration_delta_seconds: Option<f64>,
    pub number_and_disc_match: bool,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChoice {
    Candidate(usize),
    AsIs,
    Skip,
}

#[derive(Debug, Clone)]
pub struct SourceFingerprint {
    pub size: u64,
    pub modified: SystemTime,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct PlannedTrack {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub fingerprint: SourceFingerprint,
    pub item: Item,
    pub already_managed: bool,
}

#[derive(Debug, Clone)]
pub struct PlannedArtwork {
    pub destination: PathBuf,
    pub bytes: Vec<u8>,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct ApprovedAlbumPlan {
    pub album: Album,
    pub tracks: Vec<PlannedTrack>,
    pub artwork: Option<PlannedArtwork>,
    pub action: Action,
    pub library_dir: PathBuf,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub imported_tracks: usize,
    pub already_managed_tracks: usize,
    pub cleanup_recovered: bool,
    pub warnings: Vec<String>,
}

pub struct ImportPlanner<'a> {
    library: &'a Library,
    provider: &'a dyn MetadataProvider,
    options: ImportOptions,
}

impl<'a> ImportPlanner<'a> {
    #[must_use]
    pub const fn new(
        library: &'a Library,
        provider: &'a dyn MetadataProvider,
        options: ImportOptions,
    ) -> Self {
        Self {
            library,
            provider,
            options,
        }
    }

    /// Scan paths and query the metadata provider without mutating the library.
    #[allow(clippy::future_not_send)]
    pub async fn plan(&self, paths: &[PathBuf]) -> ImportPlan {
        let progress = ConsoleProgress::new();
        let (items, scan_issues) = scan_paths(paths, self.options.follow_symlinks, &progress);
        let groups = group_into_albums(items);
        let mut albums = Vec::with_capacity(groups.len());

        for group in groups {
            let query = ReleaseQuery {
                artist: group.artist.clone(),
                album: group.album.clone(),
                track_count: group.items.len(),
            };
            match self
                .provider
                .search_releases(&query, self.options.search_limit)
                .await
            {
                Ok(releases) => albums.push(AlbumPlan {
                    source_artist: group.artist,
                    source_album: group.album,
                    candidates: score_candidates(
                        &group.items,
                        releases,
                        self.options.auto_accept_threshold,
                        self.options.runner_up_margin,
                    ),
                    items: group.items,
                    lookup_error: None,
                }),
                Err(error) => albums.push(AlbumPlan {
                    source_artist: group.artist,
                    source_album: group.album,
                    items: group.items,
                    candidates: Vec::new(),
                    lookup_error: Some(error.to_string()),
                }),
            }
        }

        ImportPlan {
            albums,
            scan_issues,
        }
    }

    /// Materialize an album decision into a fully validated, non-mutating plan.
    #[allow(clippy::future_not_send)]
    pub async fn approve(
        &self,
        plan: &AlbumPlan,
        choice: ApprovalChoice,
    ) -> Result<Option<ApprovedAlbumPlan>> {
        if choice == ApprovalChoice::Skip {
            return Ok(None);
        }

        let selected = selected_candidate(plan, choice)?;
        let (mut album, items) = album_and_items(plan, selected);
        let tracks = self.plan_tracks(items)?;
        let mut warnings = Vec::new();
        let artwork = self
            .plan_artwork(selected, &tracks, &mut album, &mut warnings)
            .await?;

        Ok(Some(ApprovedAlbumPlan {
            album,
            tracks,
            artwork,
            action: self.options.action,
            library_dir: self.options.library_dir.clone(),
            warnings,
        }))
    }

    fn plan_tracks(&self, items: Vec<Item>) -> Result<Vec<PlannedTrack>> {
        let mut destinations = HashMap::<PathBuf, PathBuf>::new();
        let mut tracks = Vec::with_capacity(items.len());
        for mut item in items {
            let source = std::fs::canonicalize(&item.path).map_err(|error| {
                Error::Import(format!("cannot resolve {}: {error}", item.path.display()))
            })?;
            let metadata = std::fs::metadata(&source)?;
            let relative = format_relative_path(&self.options.path_format, &item)?;
            let extension = source
                .extension()
                .ok_or_else(|| Error::Import(format!("missing extension: {}", source.display())))?;
            let mut destination = self.options.library_dir.join(relative);
            destination.set_extension(extension);
            validate_destination(&self.options.library_dir, &destination)?;
            reject_duplicate_destination(&mut destinations, &source, &destination)?;

            let content_hash = hash_path(&source)?;
            let destination_exists = destination.exists() || destination.is_symlink();
            let managed = self.library.item_exists(&destination)?;
            let already_managed =
                destination_exists && managed && hash_path(&destination)? == content_hash;
            if destination_exists && !already_managed {
                return Err(Error::Import(format!(
                    "album destination already exists: {}",
                    destination.display()
                )));
            }
            if managed && !destination_exists {
                return Err(Error::Import(format!(
                    "database already manages the missing destination {}; repair or remove that row first",
                    destination.display()
                )));
            }

            item.path.clone_from(&destination);
            item.file_size = Some(metadata.len());
            tracks.push(PlannedTrack {
                source,
                destination,
                fingerprint: SourceFingerprint {
                    size: metadata.len(),
                    modified: metadata.modified()?,
                    content_hash,
                },
                item,
                already_managed,
            });
        }
        Ok(tracks)
    }

    #[allow(clippy::future_not_send)]
    async fn plan_artwork(
        &self,
        selected: Option<&ScoredCandidate>,
        tracks: &[PlannedTrack],
        album: &mut Album,
        warnings: &mut Vec<String>,
    ) -> Result<Option<PlannedArtwork>> {
        let Some(candidate) = selected.filter(|_| self.options.fetch_art) else {
            return Ok(None);
        };
        match self
            .provider
            .fetch_cover_art(&candidate.release.external_id)
            .await
        {
            Ok(Some(bytes)) => {
                let destination = common_track_parent(tracks)
                    .unwrap_or_else(|| self.options.library_dir.clone())
                    .join("cover.jpg");
                validate_destination(&self.options.library_dir, &destination)?;
                album.artpath = Some(destination.clone());
                if destination.exists() || destination.is_symlink() {
                    warnings.push("existing cover art will not be overwritten".into());
                    Ok(None)
                } else {
                    let content_hash = blake3::hash(&bytes).to_hex().to_string();
                    Ok(Some(PlannedArtwork {
                        destination,
                        bytes,
                        content_hash,
                    }))
                }
            }
            Ok(None) => Ok(None),
            Err(error) => {
                warnings.push(format!("cover art lookup failed: {error}"));
                Ok(None)
            }
        }
    }
}

pub struct ImportExecutor<'a> {
    library: &'a mut Library,
}

impl<'a> ImportExecutor<'a> {
    pub const fn new(library: &'a mut Library) -> Self {
        Self { library }
    }

    /// Execute one approved album atomically with respect to the database.
    pub fn execute(&mut self, mut plan: ApprovedAlbumPlan) -> Result<ImportReport> {
        let active_tracks = plan
            .tracks
            .iter()
            .filter(|track| !track.already_managed)
            .collect::<Vec<_>>();
        let already_managed_tracks = plan.tracks.len() - active_tracks.len();
        if active_tracks.is_empty() {
            return Ok(ImportReport {
                already_managed_tracks,
                warnings: plan.warnings,
                ..ImportReport::default()
            });
        }

        let transfer_id = uuid::Uuid::new_v4();
        let mut journal_files =
            Vec::with_capacity(active_tracks.len() + usize::from(plan.artwork.is_some()));
        for track in &active_tracks {
            journal_files.push(JournalFile {
                source: track.source.clone(),
                staged: staging_path(&track.destination, transfer_id, "stage")?,
                destination: track.destination.clone(),
                content_hash: Some(track.fingerprint.content_hash.clone()),
                role: "track".into(),
                state: "prepared".into(),
            });
        }
        if let Some(artwork) = &plan.artwork {
            journal_files.push(JournalFile {
                source: PathBuf::new(),
                staged: staging_path(&artwork.destination, transfer_id, "art")?,
                destination: artwork.destination.clone(),
                content_hash: Some(artwork.content_hash.clone()),
                role: "artwork".into(),
                state: "prepared".into(),
            });
        }

        let kind = match plan.action {
            Action::Copy => OperationKind::ImportCopy,
            Action::Move => OperationKind::ImportMove,
            Action::Link => OperationKind::ImportLink,
        };
        let operation_id = self.library.create_operation(kind, &journal_files)?;
        if let Err(error) =
            self.stage_and_finalize(&operation_id, &plan, &active_tracks, &journal_files)
        {
            return Err(self.rollback_failed(&operation_id, error));
        }

        let items = active_tracks
            .iter()
            .map(|track| track.item.clone())
            .collect::<Vec<_>>();
        if let Err(error) = self
            .library
            .commit_import(&operation_id, &plan.album, &items)
        {
            return Err(self.rollback_failed(&operation_id, error));
        }

        let mut cleanup_recovered = false;
        if plan.action == Action::Move {
            self.library
                .set_operation_state(&operation_id, "cleanup-pending", None)?;
            if let Err(error) = remove_move_sources(&active_tracks) {
                self.library.set_operation_state(
                    &operation_id,
                    "cleanup-pending",
                    Some(&error.to_string()),
                )?;
                let recovery = self.library.recover_pending()?;
                if !recovery.unresolved.is_empty() {
                    return Err(Error::Recovery(recovery.unresolved.join("; ")));
                }
                cleanup_recovered = true;
            } else {
                self.library.complete_operation(&operation_id)?;
            }
        } else {
            self.library.complete_operation(&operation_id)?;
        }

        Ok(ImportReport {
            imported_tracks: active_tracks.len(),
            already_managed_tracks,
            cleanup_recovered,
            warnings: std::mem::take(&mut plan.warnings),
        })
    }

    fn stage_and_finalize(
        &self,
        operation_id: &str,
        plan: &ApprovedAlbumPlan,
        tracks: &[&PlannedTrack],
        files: &[JournalFile],
    ) -> Result<()> {
        self.library
            .set_operation_state(operation_id, "staging", None)?;
        for (ordinal, track) in tracks.iter().enumerate() {
            validate_source(track)?;
            validate_destination(&plan.library_dir, &track.destination)?;
            validate_destination_for_execution(&track.destination)?;
            create_parent(&track.destination)?;
            validate_destination(&plan.library_dir, &track.destination)?;
            match plan.action {
                Action::Copy | Action::Move => copy_new(&track.source, &files[ordinal].staged)?,
                Action::Link => create_symlink(&track.source, &files[ordinal].staged)?,
            }
            verify_hash(&files[ordinal].staged, &track.fingerprint.content_hash)?;
            self.library
                .set_file_state(operation_id, ordinal, "staged")?;
        }

        if let Some(artwork) = &plan.artwork {
            let ordinal = tracks.len();
            validate_destination(&plan.library_dir, &artwork.destination)?;
            validate_destination_for_execution(&artwork.destination)?;
            create_parent(&artwork.destination)?;
            validate_destination(&plan.library_dir, &artwork.destination)?;
            write_new(&files[ordinal].staged, &artwork.bytes)?;
            verify_hash(&files[ordinal].staged, &artwork.content_hash)?;
            self.library
                .set_file_state(operation_id, ordinal, "staged")?;
        }

        self.library
            .set_operation_state(operation_id, "finalizing", None)?;
        for (ordinal, track) in tracks.iter().enumerate() {
            validate_destination(&plan.library_dir, &track.destination)?;
            validate_destination_for_execution(&track.destination)?;
            if plan.action == Action::Link {
                finalize_link(&files[ordinal].staged, &track.destination)?;
            } else {
                finalize_regular_file(&files[ordinal].staged, &track.destination)?;
            }
            self.library
                .set_file_state(operation_id, ordinal, "finalized")?;
        }
        if let Some(artwork) = &plan.artwork {
            let ordinal = tracks.len();
            validate_destination(&plan.library_dir, &artwork.destination)?;
            validate_destination_for_execution(&artwork.destination)?;
            finalize_regular_file(&files[ordinal].staged, &artwork.destination)?;
            self.library
                .set_file_state(operation_id, ordinal, "finalized")?;
        }
        Ok(())
    }

    fn rollback_failed(&mut self, operation_id: &str, error: Error) -> Error {
        let _ = self
            .library
            .set_operation_state(operation_id, "failed", Some(&error.to_string()));
        match self.library.recover_pending() {
            Ok(report) if report.unresolved.is_empty() => error,
            Ok(report) => Error::Recovery(format!(
                "{error}; automatic rollback needs attention: {}",
                report.unresolved.join("; ")
            )),
            Err(recovery_error) => Error::Recovery(format!(
                "{error}; automatic rollback failed: {recovery_error}"
            )),
        }
    }
}

#[derive(Debug)]
struct AlbumGroup {
    items: Vec<Item>,
    artist: String,
    album: String,
}

fn selected_candidate(
    plan: &AlbumPlan,
    choice: ApprovalChoice,
) -> Result<Option<&ScoredCandidate>> {
    match choice {
        ApprovalChoice::Candidate(index) => plan
            .candidates
            .get(index)
            .map(Some)
            .ok_or_else(|| Error::Import(format!("candidate {index} does not exist"))),
        ApprovalChoice::AsIs | ApprovalChoice::Skip => Ok(None),
    }
}

fn album_and_items(plan: &AlbumPlan, selected: Option<&ScoredCandidate>) -> (Album, Vec<Item>) {
    let mut items = plan.items.clone();
    if let Some(candidate) = selected {
        apply_release_metadata(&mut items, candidate);
    }
    let album = Album {
        id: None,
        album: selected.map_or_else(
            || plan.source_album.clone(),
            |candidate| candidate.release.title.clone(),
        ),
        albumartist: selected.map_or_else(
            || plan.source_artist.clone(),
            |candidate| candidate.release.artist.clone(),
        ),
        year: selected
            .and_then(|candidate| candidate.release.year)
            .or_else(|| plan.items.first().and_then(|item| item.year)),
        artpath: None,
        external_id: selected.map(|candidate| ExternalId {
            provider: candidate.release.provider.clone(),
            value: candidate.release.external_id.clone(),
        }),
        added: Utc::now(),
    };
    (album, items)
}

fn reject_duplicate_destination(
    destinations: &mut HashMap<PathBuf, PathBuf>,
    source: &Path,
    destination: &Path,
) -> Result<()> {
    destinations
        .insert(destination.to_path_buf(), source.to_path_buf())
        .map_or_else(
            || Ok(()),
            |other| {
                Err(Error::Import(format!(
                    "album has a destination collision: {} and {} both map to {}",
                    other.display(),
                    source.display(),
                    destination.display()
                )))
            },
        )
}

/// Trait for reporting scan progress.
pub trait ScanProgress: Sync {
    fn on_files_found(&self, count: usize);
    fn tick(&self);
    fn finish(&self, track_count: usize);
}

pub struct ConsoleProgress {
    bar: ProgressBar,
}

impl ConsoleProgress {
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

pub struct NoProgress;

impl ScanProgress for NoProgress {
    fn on_files_found(&self, _count: usize) {}
    fn tick(&self) {}
    fn finish(&self, _track_count: usize) {}
}

fn scan_paths<P: ScanProgress>(
    paths: &[PathBuf],
    follow_symlinks: bool,
    progress: &P,
) -> (Vec<Item>, Vec<ScanIssue>) {
    let mut files = Vec::new();
    let mut issues = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        for entry in WalkDir::new(path).follow_links(follow_symlinks) {
            match entry {
                Ok(entry) if entry.file_type().is_file() && is_audio_file(entry.path()) => {
                    match std::fs::canonicalize(entry.path()) {
                        Ok(canonical) if seen.insert(canonical.clone()) => files.push(canonical),
                        Ok(_) => {}
                        Err(error) => issues.push(ScanIssue {
                            path: entry.path().to_path_buf(),
                            message: error.to_string(),
                        }),
                    }
                }
                Ok(_) => {}
                Err(error) => issues.push(ScanIssue {
                    path: error.path().map_or_else(|| path.clone(), Path::to_path_buf),
                    message: error.to_string(),
                }),
            }
        }
    }
    files.sort();
    progress.on_files_found(files.len());
    let mut items = Vec::with_capacity(files.len());
    for path in files {
        progress.tick();
        match read_tags(&path) {
            Ok(item) => items.push(item),
            Err(error) => issues.push(ScanIssue {
                path,
                message: error.to_string(),
            }),
        }
    }
    progress.finish(items.len());
    (items, issues)
}

fn group_into_albums(items: Vec<Item>) -> Vec<AlbumGroup> {
    let mut groups: HashMap<String, Vec<Item>> = HashMap::new();
    for item in items {
        let artist = item.effective_albumartist();
        let known = !is_placeholder(artist) && !is_placeholder(&item.album);
        let key = if known {
            format!("tag:{}\0{}", normalize(artist), normalize(&item.album))
        } else {
            format!(
                "dir:{}",
                item.path
                    .parent()
                    .map_or_else(String::new, |path| path.to_string_lossy().into_owned())
            )
        };
        groups.entry(key).or_default().push(item);
    }
    let mut output = groups
        .into_values()
        .map(|items| AlbumGroup {
            artist: items.first().map_or_else(
                || "Unknown Artist".into(),
                |item| item.effective_albumartist().into(),
            ),
            album: items
                .first()
                .map_or_else(|| "Unknown Album".into(), |item| item.album.clone()),
            items,
        })
        .collect::<Vec<_>>();
    output.sort_by(|left, right| (&left.artist, &left.album).cmp(&(&right.artist, &right.album)));
    output
}

fn score_candidates(
    items: &[Item],
    releases: Vec<ReleaseCandidate>,
    threshold: f64,
    required_margin: f64,
) -> Vec<ScoredCandidate> {
    let source_artist = items
        .first()
        .map_or("Unknown Artist", Item::effective_albumartist);
    let source_album = items
        .first()
        .map_or("Unknown Album", |item| item.album.as_str());
    let mut candidates = releases
        .into_iter()
        .map(|release| {
            let track_matches = match_tracks(items, &release.tracks);
            let artist = similarity(source_artist, &release.artist);
            let album = similarity(source_album, &release.title);
            let mean_track = if track_matches.is_empty() {
                0.0
            } else {
                track_matches
                    .iter()
                    .map(|matched| matched.score)
                    .sum::<f64>()
                    / track_matches.len() as f64
            };
            let provider = release.provider_score.clamp(0.0, 1.0);
            let composite = artist.mul_add(
                ARTIST_WEIGHT,
                album.mul_add(
                    ALBUM_WEIGHT,
                    mean_track.mul_add(TRACK_WEIGHT, provider * PROVIDER_WEIGHT),
                ),
            );
            ScoredCandidate {
                release,
                confidence: ConfidenceBreakdown {
                    artist,
                    album,
                    mean_track,
                    provider,
                    composite,
                    runner_up_margin: 0.0,
                    high_confidence: false,
                    gate_failures: Vec::new(),
                },
                track_matches,
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .confidence
            .composite
            .partial_cmp(&left.confidence.composite)
            .unwrap_or(Ordering::Equal)
    });

    let runner_up = candidates
        .get(1)
        .map_or(0.0, |candidate| candidate.confidence.composite);
    if let Some(best) = candidates.first_mut() {
        best.confidence.runner_up_margin = best.confidence.composite - runner_up;
        best.confidence.gate_failures =
            confidence_gate_failures(items, best, threshold, required_margin);
        best.confidence.high_confidence = best.confidence.gate_failures.is_empty();
    }
    candidates
}

fn confidence_gate_failures(
    items: &[Item],
    candidate: &ScoredCandidate,
    threshold: f64,
    required_margin: f64,
) -> Vec<String> {
    let mut failures = Vec::new();
    if items.iter().any(|item| {
        is_placeholder(item.effective_albumartist())
            || is_placeholder(&item.album)
            || is_placeholder(&item.title)
    }) {
        failures.push("source tags contain placeholder artist, album, or title values".into());
    }
    if items.len() != candidate.release.tracks.len() {
        failures.push(format!(
            "track count differs (source {}, candidate {})",
            items.len(),
            candidate.release.tracks.len()
        ));
    }
    if candidate.confidence.artist < ARTIST_GATE {
        failures.push(format!(
            "artist similarity {:.1}% is below {:.0}%",
            candidate.confidence.artist * 100.0,
            ARTIST_GATE * 100.0
        ));
    }
    if candidate.confidence.album < ALBUM_GATE {
        failures.push(format!(
            "album similarity {:.1}% is below {:.0}%",
            candidate.confidence.album * 100.0,
            ALBUM_GATE * 100.0
        ));
    }
    for matched in &candidate.track_matches {
        let duration_and_number = matched.number_and_disc_match
            && matched
                .duration_delta_seconds
                .is_some_and(|delta| delta <= DURATION_GATE_SECONDS);
        if matched.title_similarity < TRACK_TITLE_GATE && !duration_and_number {
            failures.push(format!(
                "track {} lacks a strong title or number/disc/duration match",
                matched.item_index + 1
            ));
        }
    }
    if candidate.track_matches.len() != items.len() {
        failures.push("not every source track could be assigned".into());
    }
    if candidate.confidence.composite < threshold {
        failures.push(format!(
            "composite {:.1}% is below {:.1}%",
            candidate.confidence.composite * 100.0,
            threshold * 100.0
        ));
    }
    if candidate.confidence.runner_up_margin < required_margin {
        failures.push(format!(
            "runner-up margin {:.1} points is below {:.1}",
            candidate.confidence.runner_up_margin * 100.0,
            required_margin * 100.0
        ));
    }
    failures
}

// `Item::track` intentionally maps to provider `number`; their field names differ by contract.
#[allow(clippy::suspicious_operation_groupings)]
fn match_tracks(items: &[Item], tracks: &[ProviderTrack]) -> Vec<TrackMatch> {
    if items.is_empty() || tracks.is_empty() {
        return Vec::new();
    }
    let size = items.len().max(tracks.len());
    let mut costs = vec![vec![1_000_000_i64; size]; size];
    let mut scores = vec![vec![None; tracks.len()]; items.len()];
    for (item_index, item) in items.iter().enumerate() {
        for (track_index, provider_track) in tracks.iter().enumerate() {
            let title_similarity = similarity(&item.title, &provider_track.title);
            let duration_delta_seconds = provider_track
                .length_ms
                .map(|milliseconds| (item.length - milliseconds as f64 / 1000.0).abs());
            let duration_score = duration_delta_seconds.map_or(title_similarity, |delta| {
                if delta <= DURATION_GATE_SECONDS {
                    1.0
                } else {
                    (1.0 - (delta - DURATION_GATE_SECONDS) / 12.0).clamp(0.0, 1.0)
                }
            });
            let score = if duration_delta_seconds.is_some() {
                title_similarity.mul_add(0.7, duration_score * 0.3)
            } else {
                title_similarity
            };
            let number_and_disc_match = item.track.is_some()
                && provider_track.number.is_some()
                && (item.track == provider_track.number)
                && (item.disc.unwrap_or(1) == provider_track.disc.unwrap_or(1));
            costs[item_index][track_index] = (-(score * 100_000.0)).round() as i64;
            scores[item_index][track_index] = Some(TrackMatch {
                item_index,
                track_index,
                title_similarity,
                duration_delta_seconds,
                number_and_disc_match,
                score,
            });
        }
    }
    let Ok(matrix) = Matrix::from_rows(costs) else {
        return Vec::new();
    };
    let (_, assignment) = pathfinding::kuhn_munkres::kuhn_munkres_min(&matrix);
    assignment
        .iter()
        .enumerate()
        .filter_map(|(item_index, track_index)| {
            scores
                .get(item_index)
                .and_then(|row| row.get(*track_index))
                .and_then(Clone::clone)
        })
        .collect()
}

fn apply_release_metadata(items: &mut [Item], candidate: &ScoredCandidate) {
    let release_id = ExternalId {
        provider: candidate.release.provider.clone(),
        value: candidate.release.external_id.clone(),
    };
    for item in &mut *items {
        item.album.clone_from(&candidate.release.title);
        item.albumartist = Some(candidate.release.artist.clone());
        item.year = candidate.release.year;
        item.release_external_id = Some(release_id.clone());
    }
    for matched in &candidate.track_matches {
        let Some(item) = items.get_mut(matched.item_index) else {
            continue;
        };
        let Some(track) = candidate.release.tracks.get(matched.track_index) else {
            continue;
        };
        item.title.clone_from(&track.title);
        item.artist.clone_from(&track.artist);
        item.track = track.number;
        item.disc = track.disc;
        item.track_external_id = Some(ExternalId {
            provider: candidate.release.provider.clone(),
            value: track.external_id.clone(),
        });
    }
}

fn normalize(value: &str) -> String {
    value
        .nfkc()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn similarity(left: &str, right: &str) -> f64 {
    strsim::jaro_winkler(&normalize(left), &normalize(right))
}

fn is_placeholder(value: &str) -> bool {
    matches!(
        normalize(value).as_str(),
        "" | "unknown" | "unknown artist" | "unknown album" | "untitled" | "various"
    )
}

fn validate_destination(root: &Path, destination: &Path) -> Result<()> {
    if !root.is_absolute() {
        return Err(Error::Import(format!(
            "library directory must be absolute: {}",
            root.display()
        )));
    }
    if !destination.starts_with(root) || destination == root {
        return Err(Error::Import(format!(
            "destination escapes the library directory: {}",
            destination.display()
        )));
    }
    let mut current = root.to_path_buf();
    if current
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(Error::Import(format!(
            "library directory may not be a symlink: {}",
            root.display()
        )));
    }
    let relative = destination
        .strip_prefix(root)
        .map_err(|error| Error::Import(error.to_string()))?;
    for component in relative
        .components()
        .take(relative.components().count().saturating_sub(1))
    {
        current.push(component);
        if current
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(Error::Import(format!(
                "destination traverses a symlink: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn validate_destination_for_execution(destination: &Path) -> Result<()> {
    if destination.exists() || destination.is_symlink() {
        Err(Error::Import(format!(
            "destination appeared after planning: {}",
            destination.display()
        )))
    } else {
        Ok(())
    }
}

fn common_track_parent(tracks: &[PlannedTrack]) -> Option<PathBuf> {
    let mut common = tracks.first()?.destination.parent()?.to_path_buf();
    for track in tracks.iter().skip(1) {
        let parent = track.destination.parent()?;
        while !parent.starts_with(&common) {
            common.pop();
            if common.as_os_str().is_empty() {
                return None;
            }
        }
    }
    Some(common)
}

fn staging_path(destination: &Path, operation: uuid::Uuid, suffix: &str) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .ok_or_else(|| Error::Import(format!("invalid destination: {}", destination.display())))?
        .to_string_lossy();
    Ok(destination.with_file_name(format!(".{name}.rsbts-{operation}.{suffix}")))
}

fn validate_source(track: &PlannedTrack) -> Result<()> {
    let metadata = std::fs::metadata(&track.source)?;
    if metadata.len() != track.fingerprint.size
        || metadata.modified()? != track.fingerprint.modified
        || hash_path(&track.source)? != track.fingerprint.content_hash
    {
        return Err(Error::Import(format!(
            "source changed after planning: {}",
            track.source.display()
        )));
    }
    Ok(())
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn copy_new(source: &Path, destination: &Path) -> Result<()> {
    let mut input = std::fs::File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn write_new(destination: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn create_symlink(source: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink(source: &Path, destination: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(source, destination)?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_source: &Path, _destination: &Path) -> Result<()> {
    Err(Error::Import(
        "symbolic links are unsupported on this platform".into(),
    ))
}

fn verify_hash(path: &Path, expected: &str) -> Result<()> {
    if hash_path(path)? == expected {
        Ok(())
    } else {
        Err(Error::Import(format!(
            "staged content verification failed: {}",
            path.display()
        )))
    }
}

fn finalize_regular_file(staged: &Path, destination: &Path) -> Result<()> {
    std::fs::hard_link(staged, destination)?;
    std::fs::remove_file(staged)?;
    if let Some(parent) = destination.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn finalize_link(staged: &Path, destination: &Path) -> Result<()> {
    std::fs::hard_link(staged, destination)?;
    std::fs::remove_file(staged)?;
    if let Some(parent) = destination.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    match std::fs::File::open(path).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_move_sources(tracks: &[&PlannedTrack]) -> Result<()> {
    for track in tracks {
        verify_hash(&track.source, &track.fingerprint.content_hash)?;
        std::fs::remove_file(&track.source)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioFormat;
    use std::io::Read;

    fn item(path: PathBuf, title: &str, track: u32) -> Item {
        Item {
            id: None,
            album_id: None,
            path,
            title: title.into(),
            artist: "Black Sabbath".into(),
            album: "Paranoid".into(),
            albumartist: Some("Black Sabbath".into()),
            genre: None,
            year: Some(1970),
            track: Some(track),
            disc: Some(1),
            format: AudioFormat::Flac,
            bitrate: 1,
            length: 180.0,
            file_size: Some(4),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: Utc::now(),
        }
    }

    fn release(title: &str, second_track: &str) -> ReleaseCandidate {
        ReleaseCandidate {
            provider: "test".into(),
            external_id: title.into(),
            title: "Paranoid".into(),
            artist: "Black Sabbath".into(),
            year: Some(1970),
            provider_score: 1.0,
            tracks: vec![
                ProviderTrack {
                    external_id: "one".into(),
                    title: "War Pigs".into(),
                    artist: "Black Sabbath".into(),
                    number: Some(1),
                    disc: Some(1),
                    length_ms: Some(180_000),
                },
                ProviderTrack {
                    external_id: "two".into(),
                    title: second_track.into(),
                    artist: "Black Sabbath".into(),
                    number: Some(2),
                    disc: Some(1),
                    length_ms: Some(180_000),
                },
            ],
        }
    }

    #[test]
    fn strict_confidence_requires_runner_up_margin() {
        let items = vec![
            item("/tmp/one.flac".into(), "War Pigs", 1),
            item("/tmp/two.flac".into(), "Paranoid", 2),
        ];
        let candidates = score_candidates(
            &items,
            vec![release("a", "Paranoid"), release("b", "Paranoid")],
            0.92,
            0.05,
        );
        assert!(!candidates[0].confidence.high_confidence);
        assert!(candidates[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("runner-up")));
    }

    #[test]
    fn high_confidence_exposes_each_gate() {
        let items = vec![
            item("/tmp/one.flac".into(), "War Pigs", 1),
            item("/tmp/two.flac".into(), "Paranoid", 2),
        ];
        let candidates = score_candidates(
            &items,
            vec![release("best", "Paranoid"), release("other", "Wrong")],
            0.92,
            0.05,
        );
        assert!(candidates[0].confidence.high_confidence);
        assert!(candidates[0].confidence.composite >= 0.92);
    }

    #[test]
    fn regular_file_finalization_never_overwrites() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let staged = temporary.path().join("stage");
        let destination = temporary.path().join("destination");
        std::fs::write(&staged, b"new")?;
        std::fs::write(&destination, b"old")?;
        assert!(finalize_regular_file(&staged, &destination).is_err());
        let mut value = String::new();
        std::fs::File::open(destination)?.read_to_string(&mut value)?;
        assert_eq!(value, "old");
        Ok(())
    }

    fn approved_plan(
        source: &Path,
        library_dir: &Path,
        action: Action,
    ) -> Result<ApprovedAlbumPlan> {
        let destination = library_dir.join("Artist/Album/01 - Track.flac");
        let metadata = std::fs::metadata(source)?;
        let mut planned_item = item(destination.clone(), "Track", 1);
        planned_item.file_size = Some(metadata.len());
        Ok(ApprovedAlbumPlan {
            album: Album {
                id: None,
                album: "Album".into(),
                albumartist: "Artist".into(),
                year: None,
                artpath: None,
                external_id: None,
                added: Utc::now(),
            },
            tracks: vec![PlannedTrack {
                source: source.to_path_buf(),
                destination,
                fingerprint: SourceFingerprint {
                    size: metadata.len(),
                    modified: metadata.modified()?,
                    content_hash: hash_path(source)?,
                },
                item: planned_item,
                already_managed: false,
            }],
            artwork: None,
            action,
            library_dir: library_dir.to_path_buf(),
            warnings: Vec::new(),
        })
    }

    #[test]
    fn executor_supports_copy_move_and_link() -> Result<()> {
        for action in [Action::Copy, Action::Move, Action::Link] {
            let temporary = tempfile::tempdir()?;
            let source = temporary.path().join("source.flac");
            let library_dir = temporary.path().join("library");
            std::fs::write(&source, b"audio")?;
            let plan = approved_plan(&source, &library_dir, action)?;
            let destination = plan.tracks[0].destination.clone();
            let mut library = Library::open_in_memory()?;
            let report = ImportExecutor::new(&mut library).execute(plan)?;
            assert_eq!(report.imported_tracks, 1);
            assert!(destination.exists());
            assert_eq!(destination.is_symlink(), action == Action::Link);
            assert_eq!(source.exists(), action != Action::Move);
            assert_eq!(library.query_items(&crate::query::Query::all())?.len(), 1);
        }
        Ok(())
    }

    #[test]
    fn executor_preserves_a_late_destination_collision() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        let library_dir = temporary.path().join("library");
        std::fs::write(&source, b"audio")?;
        let plan = approved_plan(&source, &library_dir, Action::Copy)?;
        let destination = plan.tracks[0].destination.clone();
        create_parent(&destination)?;
        std::fs::write(&destination, b"collision")?;
        let mut library = Library::open_in_memory()?;
        assert!(ImportExecutor::new(&mut library).execute(plan).is_err());
        let mut value = String::new();
        std::fs::File::open(destination)?.read_to_string(&mut value)?;
        assert_eq!(value, "collision");
        assert!(library.query_items(&crate::query::Query::all())?.is_empty());
        Ok(())
    }
}
