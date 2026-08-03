//! Plan-first, album-atomic import workflow.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(not(unix))]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::{Seek, SeekFrom};

use chrono::Utc;
use indicatif::{ProgressBar, ProgressStyle};
use pathfinding::matrix::Matrix;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

#[cfg(not(unix))]
use crate::db::sync_directory;
use crate::db::{
    file_identity, hash_path, remove_file_synced, JournalFile, Library, OperationKind,
};
use crate::pathformat::format_relative_path;
use crate::provider::{
    MetadataProvider, ProviderTrack, ReleaseCandidate, ReleaseQuery, TrackCandidate, TrackQuery,
};
use crate::tags::{is_audio_file, read_tags};
use crate::{
    validate_album_metadata, validate_item_metadata, Album, Error, ExternalId, Item, Result,
};

const ARTIST_WEIGHT: f64 = 0.25;
const ALBUM_WEIGHT: f64 = 0.25;
const TRACK_WEIGHT: f64 = 0.40;
const PROVIDER_WEIGHT: f64 = 0.10;
const ARTIST_GATE: f64 = 0.95;
const ALBUM_GATE: f64 = 0.92;
const TRACK_TITLE_GATE: f64 = 0.90;
const DURATION_GATE_SECONDS: f64 = 3.0;
const MAX_MATCH_TRACKS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Copy,
    Move,
    Link,
    #[serde(rename = "in_place", alias = "inplace")]
    InPlace,
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
    pub provider_warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AlbumPlan {
    pub source_artist: String,
    pub source_album: String,
    pub items: Vec<Item>,
    pub candidates: Vec<ScoredCandidate>,
    pub lookup_error: Option<String>,
    pub singleton: bool,
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
    pub identity: String,
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
    pub album: Option<Album>,
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
            if group.singleton {
                let query = TrackQuery {
                    artist: group.artist.clone(),
                    title: group
                        .items
                        .first()
                        .map_or_else(String::new, |item| item.title.clone()),
                };
                match self
                    .provider
                    .search_tracks(&query, self.options.search_limit)
                    .await
                {
                    Ok(tracks) => albums.push(AlbumPlan {
                        source_artist: group.artist,
                        source_album: group.album,
                        candidates: score_track_candidates(
                            &group.items,
                            tracks,
                            self.options.auto_accept_threshold,
                            self.options.runner_up_margin,
                        ),
                        items: group.items,
                        lookup_error: None,
                        singleton: true,
                    }),
                    Err(error) => albums.push(AlbumPlan {
                        source_artist: group.artist,
                        source_album: group.album,
                        items: group.items,
                        candidates: Vec::new(),
                        lookup_error: Some(error.to_string()),
                        singleton: true,
                    }),
                }
                continue;
            }
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
                    singleton: false,
                }),
                Err(error) => albums.push(AlbumPlan {
                    source_artist: group.artist,
                    source_album: group.album,
                    items: group.items,
                    candidates: Vec::new(),
                    lookup_error: Some(error.to_string()),
                    singleton: false,
                }),
            }
        }

        ImportPlan {
            albums,
            scan_issues,
            provider_warnings: self.provider.take_warnings(),
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
            .plan_artwork(selected, &tracks, album.as_mut(), &mut warnings)
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
            let scanned_mtime: chrono::DateTime<Utc> = metadata.modified()?.into();
            if item.file_size != Some(metadata.len()) || item.mtime != scanned_mtime {
                return Err(Error::Import(format!(
                    "source changed after tag scanning: {}",
                    source.display()
                )));
            }
            let destination = if self.options.action == Action::InPlace {
                source.clone()
            } else {
                let relative = format_relative_path(&self.options.path_format, &item)?;
                let extension = source.extension().ok_or_else(|| {
                    Error::Import(format!("missing extension: {}", source.display()))
                })?;
                let destination =
                    append_extension(&self.options.library_dir.join(relative), extension);
                validate_destination(&self.options.library_dir, &destination)?;
                destination
            };
            reject_duplicate_destination(&mut destinations, &source, &destination)?;

            let content_hash = hash_path(&source)?;
            let managed = self.library.item_exists(&destination)?;
            let destination_exists = destination.exists() || destination.is_symlink();
            let already_managed =
                destination_exists && managed && hash_path(&destination)? == content_hash;
            if self.options.action != Action::InPlace && destination_exists && !already_managed {
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
                    identity: file_identity(&metadata),
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
        album: Option<&mut Album>,
        warnings: &mut Vec<String>,
    ) -> Result<Option<PlannedArtwork>> {
        let Some(candidate) = selected.filter(|_| {
            self.options.fetch_art && album.is_some() && self.options.action != Action::InPlace
        }) else {
            return Ok(None);
        };
        let Some(artwork_parent) =
            common_track_parent(tracks).filter(|parent| parent != &self.options.library_dir)
        else {
            warnings.push(
                "cover art skipped because tracks have no album directory below the library root"
                    .into(),
            );
            return Ok(None);
        };
        match self
            .provider
            .fetch_cover_art_for(&candidate.release.provider, &candidate.release.external_id)
            .await
        {
            Ok(Some(bytes)) => {
                let Some(extension) = artwork_extension(&bytes) else {
                    warnings.push("cover art has an unsupported or invalid image format".into());
                    return Ok(None);
                };
                let destination = artwork_parent.join(format!("cover.{extension}"));
                validate_destination(&self.options.library_dir, &destination)?;
                if let Some(album) = album {
                    album.artpath = Some(destination.clone());
                }
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
        self.validate_plan_shape(&plan)?;
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

        if plan.action == Action::InPlace {
            return self.execute_in_place(&plan, &active_tracks, already_managed_tracks);
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
                source_identity: Some(track.fingerprint.identity.clone()),
                owned_identity: None,
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
                source_identity: None,
                owned_identity: None,
                role: "artwork".into(),
                state: "prepared".into(),
            });
        }

        let kind = match plan.action {
            Action::Copy => OperationKind::ImportCopy,
            Action::Move => OperationKind::ImportMove,
            Action::Link => OperationKind::ImportLink,
            Action::InPlace => OperationKind::ImportInPlace,
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
            .commit_import(&operation_id, plan.album.as_ref(), &items)
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

    fn execute_in_place(
        &mut self,
        plan: &ApprovedAlbumPlan,
        active_tracks: &[&PlannedTrack],
        already_managed_tracks: usize,
    ) -> Result<ImportReport> {
        for track in active_tracks {
            validate_source(track)?;
        }
        let operation_id = self
            .library
            .create_operation(OperationKind::ImportInPlace, &[])?;
        let items = active_tracks
            .iter()
            .map(|track| track.item.clone())
            .collect::<Vec<_>>();
        self.library
            .commit_import(&operation_id, plan.album.as_ref(), &items)?;
        self.library.complete_operation(&operation_id)?;
        Ok(ImportReport {
            imported_tracks: active_tracks.len(),
            already_managed_tracks,
            warnings: plan.warnings.clone(),
            ..ImportReport::default()
        })
    }

    fn validate_plan_shape(&self, plan: &ApprovedAlbumPlan) -> Result<()> {
        if let Some(album) = &plan.album {
            validate_album_metadata(album)?;
        }
        let mut sources = HashSet::new();
        let mut destinations = HashSet::new();
        for track in &plan.tracks {
            validate_item_metadata(&track.item)?;
            if plan.action == Action::InPlace {
                if track.source != track.destination {
                    return Err(Error::Import(
                        "in-place imports must preserve the source path".into(),
                    ));
                }
            } else {
                validate_destination(&plan.library_dir, &track.destination)?;
            }
            if track.item.path != track.destination {
                return Err(Error::Import(format!(
                    "planned database path does not match destination: {}",
                    track.destination.display()
                )));
            }
            if !sources.insert(track.source.clone()) {
                return Err(Error::Import(format!(
                    "approved album repeats source {}",
                    track.source.display()
                )));
            }
            if !destinations.insert(track.destination.clone()) {
                return Err(Error::Import(format!(
                    "approved album repeats destination {}",
                    track.destination.display()
                )));
            }
            let planned_mtime: chrono::DateTime<Utc> = track.fingerprint.modified.into();
            if track.item.file_size != Some(track.fingerprint.size)
                || track.item.mtime != planned_mtime
            {
                return Err(Error::Import(format!(
                    "planned item metadata does not match its source fingerprint: {}",
                    track.source.display()
                )));
            }
            if track.already_managed
                && ((!track.destination.exists() && !track.destination.is_symlink())
                    || !self.library.item_exists(&track.destination)?
                    || hash_path(&track.destination)? != track.fingerprint.content_hash)
            {
                return Err(Error::Import(format!(
                    "already-managed destination changed after planning: {}",
                    track.destination.display()
                )));
            }
        }
        if let Some(artwork) = &plan.artwork {
            validate_destination(&plan.library_dir, &artwork.destination)?;
            if destinations.contains(&artwork.destination) {
                return Err(Error::Import(format!(
                    "artwork collides with a track destination: {}",
                    artwork.destination.display()
                )));
            }
            if artwork_extension(&artwork.bytes).is_none()
                || blake3::hash(&artwork.bytes).to_hex().as_str() != artwork.content_hash
            {
                return Err(Error::Import(
                    "approved artwork content does not match its plan".into(),
                ));
            }
        }
        Ok(())
    }

    fn stage_and_finalize(
        &self,
        operation_id: &str,
        plan: &ApprovedAlbumPlan,
        tracks: &[&PlannedTrack],
        files: &[JournalFile],
    ) -> Result<()> {
        let destination_root = DestinationRoot::open(&plan.library_dir)?;
        let mut prepared = Vec::with_capacity(files.len());
        self.library
            .set_operation_state(operation_id, "staging", None)?;
        for (ordinal, track) in tracks.iter().enumerate() {
            validate_source(track)?;
            validate_destination(&plan.library_dir, &track.destination)?;
            let destination =
                destination_root.resolve(&files[ordinal].staged, &track.destination)?;
            destination.ensure_destination_absent()?;
            let staged_identity = match plan.action {
                Action::Copy | Action::Move => {
                    destination.stage_regular(&track.source, &track.fingerprint.content_hash)?
                }
                Action::Link => {
                    destination.stage_link(&track.source, &track.fingerprint.content_hash)?
                }
                Action::InPlace => {
                    return Err(Error::Import(
                        "in-place imports do not stage filesystem operations".into(),
                    ));
                }
            };
            self.library
                .set_staged_file_identity(operation_id, ordinal, &staged_identity)?;
            prepared.push(PreparedDestination {
                destination,
                identity: staged_identity,
            });
        }

        if let Some(artwork) = &plan.artwork {
            let ordinal = tracks.len();
            validate_destination(&plan.library_dir, &artwork.destination)?;
            let destination =
                destination_root.resolve(&files[ordinal].staged, &artwork.destination)?;
            destination.ensure_destination_absent()?;
            let staged_identity = destination.stage_bytes(&artwork.bytes, &artwork.content_hash)?;
            self.library
                .set_staged_file_identity(operation_id, ordinal, &staged_identity)?;
            prepared.push(PreparedDestination {
                destination,
                identity: staged_identity,
            });
        }

        self.library
            .set_operation_state(operation_id, "finalizing", None)?;
        for (ordinal, destination) in prepared.iter().enumerate() {
            destination.destination.finalize(&destination.identity)?;
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
    singleton: bool,
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

fn album_and_items(
    plan: &AlbumPlan,
    selected: Option<&ScoredCandidate>,
) -> (Option<Album>, Vec<Item>) {
    let mut items = plan.items.clone();
    if let Some(candidate) = selected {
        apply_release_metadata(&mut items, candidate);
    }
    if plan.singleton {
        for item in &mut items {
            item.singleton = true;
        }
        return (None, items);
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
            kind: "release".into(),
            value: candidate.release.external_id.clone(),
        }),
        added: Utc::now(),
        extended: crate::ExtendedMetadata::default(),
    };
    (Some(album), items)
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
    let direct_files = paths
        .iter()
        .filter(|path| path.is_file())
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect::<HashSet<_>>();
    for path in paths {
        for entry in WalkDir::new(path).follow_links(follow_symlinks) {
            match entry {
                Ok(entry) if entry.file_type().is_file() && is_audio_file(entry.path()) => {
                    match std::fs::canonicalize(entry.path()) {
                        Ok(canonical) if canonical.to_str().is_none() => {
                            issues.push(ScanIssue {
                                path: canonical,
                                message: "path is not valid UTF-8 and cannot be stored safely"
                                    .into(),
                            });
                        }
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
            Ok(mut item) => {
                item.singleton = direct_files.contains(&path)
                    || is_placeholder(&item.album)
                    || is_placeholder(item.effective_albumartist());
                items.push(item);
            }
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
        let known = !item.singleton && !is_placeholder(artist) && !is_placeholder(&item.album);
        let key = if known {
            let scope = album_scope(&item.path);
            format!(
                "tag:{}\0{}\0{}",
                scope.to_string_lossy(),
                normalize(artist),
                normalize(&item.album)
            )
        } else {
            format!("singleton:{}", item.path.to_string_lossy())
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
            singleton: items.iter().all(|item| item.singleton),
            items,
        })
        .collect::<Vec<_>>();
    output.sort_by(|left, right| (&left.artist, &left.album).cmp(&(&right.artist, &right.album)));
    output
}

fn album_scope(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let looks_like_disc = parent
        .file_name()
        .and_then(|name| name.to_str())
        .map(normalize)
        .is_some_and(|name| {
            ["disc", "disk", "cd"].iter().any(|prefix| {
                name.strip_prefix(prefix).is_some_and(|rest| {
                    !rest.trim().is_empty()
                        && rest
                            .trim()
                            .chars()
                            .all(|character| character.is_ascii_digit())
                })
            })
        });
    if looks_like_disc {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

fn score_track_candidates(
    items: &[Item],
    tracks: Vec<TrackCandidate>,
    threshold: f64,
    required_margin: f64,
) -> Vec<ScoredCandidate> {
    let Some(item) = items.first() else {
        return Vec::new();
    };
    let mut candidates = tracks
        .into_iter()
        .map(|track| {
            let artist = similarity(&item.artist, &track.artist);
            let title = similarity(&item.title, &track.title);
            let duration_delta_seconds = track
                .length_ms
                .map(|length| (item.length - length as f64 / 1_000.0).abs());
            let duration_ok =
                duration_delta_seconds.is_none_or(|delta| delta <= DURATION_GATE_SECONDS);
            let provider = if track.provider_score.is_finite() {
                track.provider_score.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let composite = artist.mul_add(0.35, title.mul_add(0.55, provider * 0.10));
            let mut failures = Vec::new();
            if artist < ARTIST_GATE {
                failures.push("artist similarity is below 95%".into());
            }
            if title < TRACK_TITLE_GATE {
                failures.push("title similarity is below 90%".into());
            }
            if !duration_ok {
                failures.push("duration differs by more than three seconds".into());
            }
            let release = ReleaseCandidate {
                provider: track.provider.clone(),
                external_id: track.release_external_id.clone().unwrap_or_default(),
                title: item.album.clone(),
                artist: track.artist.clone(),
                year: item.year,
                provider_score: provider,
                tracks: vec![ProviderTrack {
                    external_id: track.external_id,
                    title: track.title,
                    artist: track.artist,
                    number: item.track,
                    disc: item.disc,
                    length_ms: track.length_ms,
                }],
            };
            ScoredCandidate {
                release,
                confidence: ConfidenceBreakdown {
                    artist,
                    album: 1.0,
                    mean_track: title,
                    provider,
                    composite,
                    runner_up_margin: 0.0,
                    high_confidence: false,
                    gate_failures: failures,
                },
                track_matches: vec![TrackMatch {
                    item_index: 0,
                    track_index: 0,
                    title_similarity: title,
                    duration_delta_seconds,
                    number_and_disc_match: true,
                    score: title,
                }],
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
        apply_singleton_confidence_gates(best, runner_up, threshold, required_margin);
    }
    candidates
}

fn apply_singleton_confidence_gates(
    best: &mut ScoredCandidate,
    runner_up: f64,
    threshold: f64,
    required_margin: f64,
) {
    best.confidence.runner_up_margin = best.confidence.composite - runner_up;
    let thresholds_valid =
        (0.0..=1.0).contains(&threshold) && (0.0..=1.0).contains(&required_margin);
    if !thresholds_valid {
        best.confidence
            .gate_failures
            .push("matching thresholds are invalid".into());
    }
    if thresholds_valid && best.confidence.composite < threshold {
        best.confidence.gate_failures.push(format!(
            "composite score {:.1}% is below {:.1}%",
            best.confidence.composite * 100.0,
            threshold * 100.0
        ));
    }
    if thresholds_valid && best.confidence.runner_up_margin < required_margin {
        best.confidence.gate_failures.push(format!(
            "runner-up margin {:.1}% is below {:.1}%",
            best.confidence.runner_up_margin * 100.0,
            required_margin * 100.0
        ));
    }
    best.confidence.high_confidence = best.confidence.gate_failures.is_empty();
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
            let provider = if release.provider_score.is_finite() {
                release.provider_score.clamp(0.0, 1.0)
            } else {
                0.0
            };
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
    if !(0.0..=1.0).contains(&threshold) || !(0.0..=1.0).contains(&required_margin) {
        failures.push("matching thresholds are invalid".into());
    }
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
    if items.is_empty()
        || tracks.is_empty()
        || items.len() > MAX_MATCH_TRACKS
        || tracks.len() > MAX_MATCH_TRACKS
    {
        return Vec::new();
    }
    let size = items.len().max(tracks.len());
    let mut costs = vec![vec![1_000_000_i64; size]; size];
    let mut scores = vec![vec![None; tracks.len()]; items.len()];
    for (item_index, item) in items.iter().enumerate() {
        for (track_index, provider_track) in tracks.iter().enumerate() {
            let title_similarity = similarity(&item.title, &provider_track.title);
            let duration_delta_seconds = if item.length.is_finite() {
                provider_track
                    .length_ms
                    .map(|milliseconds| (item.length - milliseconds as f64 / 1000.0).abs())
            } else {
                None
            };
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
    let release_id = (!candidate.release.external_id.is_empty()).then(|| ExternalId {
        provider: candidate.release.provider.clone(),
        kind: "release".into(),
        value: candidate.release.external_id.clone(),
    });
    for item in &mut *items {
        item.album.clone_from(&candidate.release.title);
        item.albumartist = Some(candidate.release.artist.clone());
        item.year = candidate.release.year;
        item.release_external_id.clone_from(&release_id);
        if let Some(release_id) = &release_id {
            if !item.extended.external_ids.contains(release_id) {
                item.extended.external_ids.push(release_id.clone());
            }
        }
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
        let track_id = ExternalId {
            provider: candidate.release.provider.clone(),
            kind: "recording".into(),
            value: track.external_id.clone(),
        };
        item.track_external_id = Some(track_id.clone());
        if !item.extended.external_ids.contains(&track_id) {
            item.extended.external_ids.push(track_id);
        }
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

#[cfg(not(unix))]
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

fn artwork_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else {
        None
    }
}

fn staging_path(destination: &Path, operation: uuid::Uuid, suffix: &str) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .ok_or_else(|| Error::Import(format!("invalid destination: {}", destination.display())))?
        .to_str()
        .ok_or_else(|| {
            Error::Import(format!(
                "destination filename is not valid UTF-8: {}",
                destination.display()
            ))
        })?;
    Ok(destination.with_file_name(format!(".{name}.rsbts-{operation}.{suffix}")))
}

fn append_extension(path: &Path, extension: &std::ffi::OsStr) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".");
    value.push(extension);
    value.into()
}

fn validate_source(track: &PlannedTrack) -> Result<()> {
    let before = std::fs::metadata(&track.source)?;
    let content_hash = hash_path(&track.source)?;
    let after = std::fs::metadata(&track.source)?;
    if before.len() != track.fingerprint.size
        || before.modified()? != track.fingerprint.modified
        || file_identity(&before) != track.fingerprint.identity
        || file_identity(&after) != track.fingerprint.identity
        || content_hash != track.fingerprint.content_hash
    {
        return Err(Error::Import(format!(
            "source changed after planning: {}",
            track.source.display()
        )));
    }
    Ok(())
}

/// Stage and finalize one already-journaled regular file below a pinned library root.
pub(crate) fn stage_relocation(
    library: &Library,
    operation_id: &str,
    library_dir: &Path,
    track: &PlannedTrack,
    journal_file: &JournalFile,
) -> Result<()> {
    validate_source(track)?;
    validate_destination(library_dir, &track.destination)?;
    let root = DestinationRoot::open(library_dir)?;
    let destination = root.resolve(&journal_file.staged, &track.destination)?;
    destination.ensure_destination_absent()?;
    library.set_operation_state(operation_id, "staging", None)?;
    let identity = destination.stage_regular(&track.source, &track.fingerprint.content_hash)?;
    library.set_staged_file_identity(operation_id, 0, &identity)?;
    library.set_operation_state(operation_id, "finalizing", None)?;
    destination.finalize(&identity)?;
    library.set_file_state(operation_id, 0, "finalized")?;
    Ok(())
}

struct PreparedDestination<'a> {
    destination: DestinationFile<'a>,
    identity: String,
}

#[cfg(unix)]
struct DestinationRoot {
    path: PathBuf,
    directory: rustix::fd::OwnedFd,
    identity: String,
}

#[cfg(unix)]
struct DestinationFile<'a> {
    root: &'a DestinationRoot,
    directory: rustix::fd::OwnedFd,
    relative_parent: PathBuf,
    parent_identity: String,
    staged_name: std::ffi::OsString,
    destination_name: std::ffi::OsString,
    staged_path: PathBuf,
    destination_path: PathBuf,
}

#[cfg(unix)]
impl DestinationRoot {
    fn open(path: &Path) -> Result<Self> {
        let directory = open_root(path, true)?;
        let identity = directory_identity(&directory)?;
        let root = Self {
            path: path.to_path_buf(),
            directory,
            identity,
        };
        root.ensure_current()?;
        Ok(root)
    }

    fn resolve<'a>(&'a self, staged: &Path, destination: &Path) -> Result<DestinationFile<'a>> {
        self.ensure_current()?;
        let destination_relative = destination.strip_prefix(&self.path).map_err(|error| {
            Error::Import(format!(
                "destination escapes the library directory: {}: {error}",
                destination.display()
            ))
        })?;
        let staged_relative = staged.strip_prefix(&self.path).map_err(|error| {
            Error::Import(format!(
                "staging path escapes the library directory: {}: {error}",
                staged.display()
            ))
        })?;
        let destination_parent = destination_relative.parent().ok_or_else(|| {
            Error::Import(format!(
                "destination has no parent: {}",
                destination.display()
            ))
        })?;
        if staged_relative.parent() != Some(destination_parent) {
            return Err(Error::Import(format!(
                "staging path does not share the destination parent: {}",
                staged.display()
            )));
        }
        let destination_name = normal_filename(destination_relative, "destination")?;
        let staged_name = normal_filename(staged_relative, "staging")?;
        let directory = self.open_relative_directory(destination_parent, true)?;
        let parent_identity = directory_identity(&directory)?;
        Ok(DestinationFile {
            root: self,
            directory,
            relative_parent: destination_parent.to_path_buf(),
            parent_identity,
            staged_name,
            destination_name,
            staged_path: staged.to_path_buf(),
            destination_path: destination.to_path_buf(),
        })
    }

    fn ensure_current(&self) -> Result<()> {
        let current = open_root(&self.path, false)?;
        if directory_identity(&current)? == self.identity {
            Ok(())
        } else {
            Err(Error::Import(format!(
                "library directory changed during execution: {}",
                self.path.display()
            )))
        }
    }

    fn open_relative_directory(
        &self,
        relative: &Path,
        create_missing: bool,
    ) -> Result<rustix::fd::OwnedFd> {
        let mut directory =
            rustix::io::dup(&self.directory).map_err(|error| Error::Io(error.into()))?;
        let mut display_path = self.path.clone();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(Error::Import(format!(
                    "destination contains an unsafe path component: {}",
                    relative.display()
                )));
            };
            display_path.push(name);
            let flags = rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC;
            let opened =
                match rustix::fs::openat(&directory, name, flags, rustix::fs::Mode::empty()) {
                    Ok(opened) => opened,
                    Err(rustix::io::Errno::NOENT) if create_missing => {
                        match rustix::fs::mkdirat(&directory, name, rustix::fs::Mode::from(0o777)) {
                            Ok(()) => {
                                rustix::fs::fsync(&directory)
                                    .map_err(|error| Error::Io(error.into()))?;
                            }
                            Err(rustix::io::Errno::EXIST) => {}
                            Err(error) => {
                                return Err(destination_error(
                                    "cannot create destination directory",
                                    &display_path,
                                    error,
                                ));
                            }
                        }
                        rustix::fs::openat(&directory, name, flags, rustix::fs::Mode::empty())
                            .map_err(|error| {
                                destination_error(
                                    "cannot open newly created destination directory",
                                    &display_path,
                                    error,
                                )
                            })?
                    }
                    Err(error) => {
                        return Err(destination_error(
                            "destination parent is not a stable, real directory",
                            &display_path,
                            error,
                        ));
                    }
                };
            directory = opened;
        }
        Ok(directory)
    }
}

#[cfg(unix)]
impl DestinationFile<'_> {
    fn ensure_destination_absent(&self) -> Result<()> {
        self.ensure_parent_current()?;
        self.ensure_absent(&self.destination_name, &self.destination_path)
    }

    fn stage_regular(&self, source: &Path, expected_hash: &str) -> Result<String> {
        self.ensure_parent_current()?;
        self.ensure_absent(&self.destination_name, &self.destination_path)?;
        let descriptor = rustix::fs::openat(
            &self.directory,
            &self.staged_name,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from(0o644),
        )
        .map_err(|error| {
            destination_error("cannot create staging file", &self.staged_path, error)
        })?;
        let mut output = std::fs::File::from(descriptor);
        let result = (|| {
            let mut input = std::fs::File::open(source)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
            verify_open_file_hash(&mut output, expected_hash, &self.staged_path)?;
            self.ensure_parent_current()
        })();
        let identity = file_identity(&output.metadata()?);
        if let Err(error) = result {
            return Err(self.cleanup_created(
                &self.staged_name,
                &self.staged_path,
                &identity,
                error,
            ));
        }
        Ok(identity)
    }

    fn stage_bytes(&self, bytes: &[u8], expected_hash: &str) -> Result<String> {
        self.ensure_parent_current()?;
        self.ensure_absent(&self.destination_name, &self.destination_path)?;
        let descriptor = rustix::fs::openat(
            &self.directory,
            &self.staged_name,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from(0o644),
        )
        .map_err(|error| {
            destination_error("cannot create staging file", &self.staged_path, error)
        })?;
        let mut output = std::fs::File::from(descriptor);
        let result = (|| {
            output.write_all(bytes)?;
            output.sync_all()?;
            verify_open_file_hash(&mut output, expected_hash, &self.staged_path)?;
            self.ensure_parent_current()
        })();
        let identity = file_identity(&output.metadata()?);
        if let Err(error) = result {
            return Err(self.cleanup_created(
                &self.staged_name,
                &self.staged_path,
                &identity,
                error,
            ));
        }
        Ok(identity)
    }

    fn stage_link(&self, source: &Path, expected_hash: &str) -> Result<String> {
        use std::os::unix::ffi::OsStrExt;

        self.ensure_parent_current()?;
        self.ensure_absent(&self.destination_name, &self.destination_path)?;
        rustix::fs::symlinkat(source, &self.directory, &self.staged_name).map_err(|error| {
            destination_error("cannot create staging link", &self.staged_path, error)
        })?;
        let identity = match self.entry_identity(&self.staged_name, &self.staged_path) {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                return Err(Error::Recovery(format!(
                    "new staging link disappeared before it could be journaled: {}",
                    self.staged_path.display()
                )));
            }
            Err(error) => return Err(error),
        };
        let result = (|| {
            let target = rustix::fs::readlinkat(&self.directory, &self.staged_name, Vec::new())
                .map_err(|error| {
                    destination_error("cannot inspect staging link", &self.staged_path, error)
                })?;
            if target.to_bytes() != source.as_os_str().as_bytes() {
                return Err(Error::Import(format!(
                    "staging link target changed during creation: {}",
                    self.staged_path.display()
                )));
            }
            verify_hash(source, expected_hash)?;
            self.ensure_parent_current()
        })();
        if let Err(error) = result {
            return Err(self.cleanup_created(
                &self.staged_name,
                &self.staged_path,
                &identity,
                error,
            ));
        }
        Ok(identity)
    }

    fn finalize(&self, expected_identity: &str) -> Result<()> {
        self.ensure_parent_current()?;
        self.ensure_owned(&self.staged_name, &self.staged_path, expected_identity)?;
        self.ensure_absent(&self.destination_name, &self.destination_path)?;
        rustix::fs::linkat(
            &self.directory,
            &self.staged_name,
            &self.directory,
            &self.destination_name,
            rustix::fs::AtFlags::empty(),
        )
        .map_err(|error| {
            destination_error(
                "cannot finalize staging file without overwriting",
                &self.destination_path,
                error,
            )
        })?;
        let result = (|| {
            self.ensure_owned(
                &self.destination_name,
                &self.destination_path,
                expected_identity,
            )?;
            rustix::fs::unlinkat(
                &self.directory,
                &self.staged_name,
                rustix::fs::AtFlags::empty(),
            )
            .map_err(|error| {
                destination_error(
                    "cannot remove finalized staging name",
                    &self.staged_path,
                    error,
                )
            })?;
            rustix::fs::fsync(&self.directory).map_err(|error| Error::Io(error.into()))?;
            self.ensure_parent_current()
        })();
        if let Err(error) = result {
            return Err(self.cleanup_created(
                &self.destination_name,
                &self.destination_path,
                expected_identity,
                error,
            ));
        }
        Ok(())
    }

    fn ensure_parent_current(&self) -> Result<()> {
        self.root.ensure_current()?;
        let current = self
            .root
            .open_relative_directory(&self.relative_parent, false)?;
        if directory_identity(&current)? == self.parent_identity {
            Ok(())
        } else {
            Err(Error::Import(format!(
                "destination parent changed during execution: {}",
                self.destination_path
                    .parent()
                    .map_or_else(String::new, |path| path.display().to_string())
            )))
        }
    }

    fn ensure_absent(&self, name: &std::ffi::OsStr, path: &Path) -> Result<()> {
        match self.entry_identity(name, path)? {
            None => Ok(()),
            Some(_) => Err(Error::Import(format!(
                "destination appeared after planning: {}",
                path.display()
            ))),
        }
    }

    fn ensure_owned(&self, name: &std::ffi::OsStr, path: &Path, identity: &str) -> Result<()> {
        match self.entry_identity(name, path)? {
            Some(actual) if actual == identity => Ok(()),
            Some(_) => Err(Error::Recovery(format!(
                "refusing to touch a replaced destination entry: {}",
                path.display()
            ))),
            None => Err(Error::Recovery(format!(
                "owned destination entry disappeared: {}",
                path.display()
            ))),
        }
    }

    fn entry_identity(&self, name: &std::ffi::OsStr, path: &Path) -> Result<Option<String>> {
        match rustix::fs::statat(&self.directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(stat_identity(&stat))),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(destination_error(
                "cannot inspect destination entry",
                path,
                error,
            )),
        }
    }

    fn cleanup_created(
        &self,
        name: &std::ffi::OsStr,
        path: &Path,
        identity: &str,
        original: Error,
    ) -> Error {
        match self.entry_identity(name, path) {
            Ok(None) => original,
            Ok(Some(actual)) if actual != identity => Error::Recovery(format!(
                "{original}; preserving replaced destination entry {}",
                path.display()
            )),
            Ok(Some(_)) => {
                match rustix::fs::unlinkat(&self.directory, name, rustix::fs::AtFlags::empty()) {
                    Ok(()) => {
                        if let Err(sync_error) = rustix::fs::fsync(&self.directory) {
                            Error::Recovery(format!(
                            "{original}; removed failed destination but could not sync its directory: {}",
                            std::io::Error::from(sync_error)
                        ))
                        } else {
                            original
                        }
                    }
                    Err(cleanup_error) => Error::Recovery(format!(
                        "{original}; could not remove owned failed destination {}: {}",
                        path.display(),
                        std::io::Error::from(cleanup_error)
                    )),
                }
            }
            Err(cleanup_error) => Error::Recovery(format!(
                "{original}; could not verify owned failed destination {}: {cleanup_error}",
                path.display()
            )),
        }
    }
}

#[cfg(unix)]
fn open_directory(path: &Path) -> std::result::Result<rustix::fd::OwnedFd, rustix::io::Errno> {
    rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
}

#[cfg(unix)]
fn open_root(path: &Path, create_missing: bool) -> Result<rustix::fd::OwnedFd> {
    if !path.is_absolute() {
        return Err(Error::Import(format!(
            "library directory must be absolute: {}",
            path.display()
        )));
    }

    let mut directory = open_directory(Path::new("/"))
        .map_err(|error| destination_error("cannot open filesystem root", Path::new("/"), error))?;
    let mut display_path = PathBuf::from("/");
    for component in path.components() {
        let name = match component {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(name) => name,
            _ => {
                return Err(Error::Import(format!(
                    "library directory contains an unsafe path component: {}",
                    path.display()
                )));
            }
        };
        display_path.push(name);
        let flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC;
        let opened = match rustix::fs::openat(&directory, name, flags, rustix::fs::Mode::empty()) {
            Ok(opened) => opened,
            Err(rustix::io::Errno::NOENT) if create_missing => {
                match rustix::fs::mkdirat(&directory, name, rustix::fs::Mode::from(0o777)) {
                    Ok(()) => {
                        rustix::fs::fsync(&directory).map_err(|error| Error::Io(error.into()))?;
                    }
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => {
                        return Err(destination_error(
                            "cannot create library directory",
                            &display_path,
                            error,
                        ));
                    }
                }
                rustix::fs::openat(&directory, name, flags, rustix::fs::Mode::empty()).map_err(
                    |error| {
                        destination_error(
                            "new library directory is not a stable, real directory",
                            &display_path,
                            error,
                        )
                    },
                )?
            }
            Err(error) => {
                return Err(destination_error(
                    "library directory path is not a stable, real directory",
                    &display_path,
                    error,
                ));
            }
        };
        directory = opened;
    }
    Ok(directory)
}

#[cfg(unix)]
fn directory_identity(directory: &rustix::fd::OwnedFd) -> Result<String> {
    let stat = rustix::fs::fstat(directory).map_err(|error| Error::Io(error.into()))?;
    Ok(format!("{}:{}", stat.st_dev, stat.st_ino))
}

#[cfg(unix)]
fn stat_identity(stat: &rustix::fs::Stat) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime, stat.st_mtime_nsec
    )
}

#[cfg(unix)]
fn normal_filename(path: &Path, role: &str) -> Result<std::ffi::OsString> {
    let mut components = path.components();
    let Some(std::path::Component::Normal(name)) = components.next_back() else {
        return Err(Error::Import(format!(
            "{role} path has no safe filename: {}",
            path.display()
        )));
    };
    if components.any(|component| !matches!(component, std::path::Component::Normal(_))) {
        return Err(Error::Import(format!(
            "{role} path contains an unsafe component: {}",
            path.display()
        )));
    }
    Ok(name.to_os_string())
}

#[cfg(unix)]
fn destination_error(action: &str, path: &Path, error: rustix::io::Errno) -> Error {
    Error::Import(format!(
        "{action} {}: {}",
        path.display(),
        std::io::Error::from(error)
    ))
}

#[cfg(unix)]
fn verify_open_file_hash(file: &mut std::fs::File, expected: &str, path: &Path) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(file, &mut hasher)?;
    let actual = hasher.finalize().to_hex().to_string();
    if actual == expected {
        Ok(())
    } else {
        Err(Error::Import(format!(
            "staged content verification failed: {}",
            path.display()
        )))
    }
}

#[cfg(not(unix))]
struct DestinationRoot {
    path: PathBuf,
}

#[cfg(not(unix))]
struct DestinationFile<'a> {
    _root: &'a DestinationRoot,
    staged_path: PathBuf,
    destination_path: PathBuf,
}

#[cfg(not(unix))]
impl DestinationRoot {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn resolve<'a>(&'a self, staged: &Path, destination: &Path) -> Result<DestinationFile<'a>> {
        validate_destination(&self.path, staged)?;
        validate_destination(&self.path, destination)?;
        create_parent(destination)?;
        validate_destination(&self.path, destination)?;
        Ok(DestinationFile {
            _root: self,
            staged_path: staged.to_path_buf(),
            destination_path: destination.to_path_buf(),
        })
    }
}

#[cfg(not(unix))]
impl DestinationFile<'_> {
    fn ensure_destination_absent(&self) -> Result<()> {
        validate_destination_for_execution(&self.destination_path)
    }

    fn stage_regular(&self, source: &Path, expected_hash: &str) -> Result<String> {
        copy_new(source, &self.staged_path)?;
        verify_hash(&self.staged_path, expected_hash)?;
        Ok(file_identity(&std::fs::symlink_metadata(
            &self.staged_path,
        )?))
    }

    fn stage_bytes(&self, bytes: &[u8], expected_hash: &str) -> Result<String> {
        write_new(&self.staged_path, bytes)?;
        verify_hash(&self.staged_path, expected_hash)?;
        Ok(file_identity(&std::fs::symlink_metadata(
            &self.staged_path,
        )?))
    }

    fn stage_link(&self, source: &Path, expected_hash: &str) -> Result<String> {
        create_symlink(source, &self.staged_path)?;
        verify_hash(&self.staged_path, expected_hash)?;
        Ok(file_identity(&std::fs::symlink_metadata(
            &self.staged_path,
        )?))
    }

    fn finalize(&self, _expected_identity: &str) -> Result<()> {
        validate_destination_for_execution(&self.destination_path)?;
        finalize_regular_file(&self.staged_path, &self.destination_path)
    }
}

#[cfg(not(unix))]
fn create_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let mut missing = Vec::new();
    let mut current = parent;
    loop {
        match std::fs::symlink_metadata(current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => break,
            Ok(_) => {
                return Err(Error::Import(format!(
                    "destination parent is not a real directory: {}",
                    current.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current.parent().ok_or_else(|| {
                    Error::Import(format!(
                        "cannot locate an existing ancestor for {}",
                        parent.display()
                    ))
                })?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    for directory in missing.iter().rev() {
        std::fs::create_dir(directory)?;
        if let Some(ancestor) = directory.parent() {
            sync_directory(ancestor)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
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

#[cfg(not(unix))]
fn write_new(destination: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    output.write_all(bytes)?;
    output.sync_all()?;
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

#[cfg(not(unix))]
fn finalize_regular_file(staged: &Path, destination: &Path) -> Result<()> {
    std::fs::hard_link(staged, destination)?;
    std::fs::remove_file(staged)?;
    if let Some(parent) = destination.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn remove_move_sources(tracks: &[&PlannedTrack]) -> Result<()> {
    for track in tracks {
        validate_source(track)?;
        remove_file_synced(&track.source)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioFormat;
    use async_trait::async_trait;
    use std::io::Read;

    struct EmptyProvider;

    #[async_trait]
    impl MetadataProvider for EmptyProvider {
        fn name(&self) -> &'static str {
            "empty"
        }

        async fn search_releases(
            &self,
            _query: &ReleaseQuery,
            _limit: u32,
        ) -> Result<Vec<ReleaseCandidate>> {
            Ok(Vec::new())
        }

        async fn fetch_cover_art(&self, _release_id: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

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
            singleton: false,
            extended: crate::ExtendedMetadata::default(),
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
    fn singleton_confidence_requires_a_valid_runner_up_margin() {
        let source = item(PathBuf::from("single.flac"), "Track", 1);
        let candidate = TrackCandidate {
            provider: "musicbrainz".into(),
            external_id: "recording".into(),
            title: "Track".into(),
            artist: "Black Sabbath".into(),
            length_ms: Some(180_000),
            provider_score: 1.0,
            release_external_id: None,
        };
        let invalid_threshold_candidate = candidate.clone();
        let candidates = score_track_candidates(
            std::slice::from_ref(&source),
            vec![candidate.clone(), candidate],
            0.92,
            0.05,
        );
        assert!(!candidates[0].confidence.high_confidence);
        assert!(candidates[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("runner-up margin")));

        let candidates = score_track_candidates(
            &[source],
            vec![invalid_threshold_candidate],
            f64::NAN,
            f64::NAN,
        );
        assert!(!candidates[0].confidence.high_confidence);
        assert!(candidates[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("thresholds are invalid")));
    }

    #[test]
    fn pathological_track_sets_fail_closed_before_matrix_allocation() {
        let source = item(PathBuf::from("source.flac"), "Track", 1);
        let provider = ProviderTrack {
            external_id: "track".into(),
            title: "Track".into(),
            artist: "Artist".into(),
            number: Some(1),
            disc: Some(1),
            length_ms: Some(180_000),
        };
        assert!(match_tracks(&vec![source; MAX_MATCH_TRACKS + 1], &[provider]).is_empty());
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
    fn non_finite_provider_scores_fail_closed() {
        let items = vec![
            item("/tmp/one.flac".into(), "War Pigs", 1),
            item("/tmp/two.flac".into(), "Paranoid", 2),
        ];
        let mut candidate = release("bad-score", "Paranoid");
        candidate.provider_score = f64::NAN;

        let candidates = score_candidates(&items, vec![candidate], 0.92, 0.05);

        assert!(candidates[0].confidence.provider.abs() < f64::EPSILON);
        assert!(!candidates[0].confidence.high_confidence);
    }

    #[test]
    fn invalid_matching_thresholds_fail_closed() {
        let items = vec![
            item("/tmp/one.flac".into(), "War Pigs", 1),
            item("/tmp/two.flac".into(), "Paranoid", 2),
        ];
        let candidates = score_candidates(
            &items,
            vec![release("candidate", "Paranoid")],
            f64::NAN,
            0.05,
        );
        assert!(!candidates[0].confidence.high_confidence);
        assert!(candidates[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("threshold")));
    }

    #[test]
    fn artwork_uses_its_detected_format() {
        assert_eq!(artwork_extension(&[0xff, 0xd8, 0xff, 0x00]), Some("jpg"));
        assert_eq!(artwork_extension(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(artwork_extension(b"RIFF0000WEBPrest"), Some("webp"));
        assert_eq!(artwork_extension(b"GIF89arest"), Some("gif"));
        assert_eq!(artwork_extension(b"not an image"), None);
    }

    #[test]
    fn source_extensions_are_appended_to_dotted_titles() {
        assert_eq!(
            append_extension(
                Path::new("Artist/01 - Part 1.2"),
                std::ffi::OsStr::new("flac")
            ),
            PathBuf::from("Artist/01 - Part 1.2.flac")
        );
    }

    // Darwin rejects this byte sequence at directory creation time, so the
    // scanner-level fixture is only representable on Unix filesystems that
    // permit arbitrary non-NUL path bytes.
    #[cfg(target_os = "linux")]
    #[test]
    fn scanner_rejects_non_utf8_paths_before_reading_tags() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;

        let temporary = tempfile::tempdir()?;
        let directory = temporary
            .path()
            .join(std::ffi::OsString::from_vec(b"album-\xff".to_vec()));
        std::fs::create_dir(&directory)?;
        std::fs::write(directory.join("track.flac"), b"not audio")?;

        let (items, issues) = scan_paths(&[directory], false, &NoProgress);

        assert!(items.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("UTF-8"));
        Ok(())
    }

    #[tokio::test]
    async fn approval_rejects_a_source_changed_after_tag_scanning() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        let library_dir = temporary.path().join("library");
        std::fs::write(&source, b"audio")?;
        let metadata = std::fs::metadata(&source)?;
        let mut source_item = item(source.clone(), "Track", 1);
        source_item.file_size = Some(metadata.len());
        source_item.mtime = metadata.modified()?.into();
        let album = AlbumPlan {
            source_artist: "Black Sabbath".into(),
            source_album: "Paranoid".into(),
            items: vec![source_item],
            candidates: Vec::new(),
            lookup_error: None,
            singleton: false,
        };
        std::fs::write(&source, b"changed audio")?;
        let library = Library::open_in_memory()?;
        let provider = EmptyProvider;
        let options = ImportOptions {
            action: Action::Copy,
            fetch_art: false,
            follow_symlinks: false,
            path_format: "$albumartist/$album/$track - $title".into(),
            library_dir,
            search_limit: 1,
            auto_accept_threshold: 0.92,
            runner_up_margin: 0.05,
        };

        let result = ImportPlanner::new(&library, &provider, options)
            .approve(&album, ApprovalChoice::AsIs)
            .await;

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn regular_file_finalization_never_overwrites() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let temporary_path = temporary.path().canonicalize()?;
        let staged = temporary_path.join("stage");
        let destination = temporary_path.join("destination");
        std::fs::write(&staged, b"new")?;
        std::fs::write(&destination, b"old")?;
        let root = DestinationRoot::open(&temporary_path)?;
        let destination_file = root.resolve(&staged, &destination)?;
        let identity = file_identity(&std::fs::symlink_metadata(&staged)?);
        assert!(destination_file.finalize(&identity).is_err());
        let mut value = String::new();
        std::fs::File::open(destination)?.read_to_string(&mut value)?;
        assert_eq!(value, "old");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn library_creation_rejects_a_symlinked_ancestor() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let outside = temporary.path().join("outside");
        let redirected = temporary.path().join("redirected");
        std::fs::create_dir(&outside)?;
        std::os::unix::fs::symlink(&outside, &redirected)?;

        assert!(DestinationRoot::open(&redirected.join("library")).is_err());
        assert!(!outside.join("library").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_a_parent_replaced_by_a_symlink() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let temporary_path = temporary.path().canonicalize()?;
        let source = temporary_path.join("source.flac");
        let library_dir = temporary_path.join("library");
        let parent = library_dir.join("Artist/Album");
        let detached = temporary_path.join("detached-album");
        let outside = temporary_path.join("outside");
        let staged = parent.join(".track.stage");
        let destination = parent.join("track.flac");
        std::fs::write(&source, b"audio")?;
        std::fs::create_dir_all(&parent)?;
        std::fs::create_dir(&outside)?;

        let root = DestinationRoot::open(&library_dir)?;
        let destination_file = root.resolve(&staged, &destination)?;
        std::fs::rename(&parent, &detached)?;
        std::os::unix::fs::symlink(&outside, &parent)?;

        assert!(destination_file
            .stage_regular(&source, &hash_path(&source)?)
            .is_err());
        assert!(!outside.join(".track.stage").exists());
        assert!(!outside.join("track.flac").exists());
        assert!(!detached.join(".track.stage").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn finalization_rejects_a_parent_replaced_by_a_symlink() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let temporary_path = temporary.path().canonicalize()?;
        let source = temporary_path.join("source.flac");
        let library_dir = temporary_path.join("library");
        let parent = library_dir.join("Artist/Album");
        let detached = temporary_path.join("detached-album");
        let outside = temporary_path.join("outside");
        let staged = parent.join(".track.stage");
        let destination = parent.join("track.flac");
        std::fs::write(&source, b"audio")?;
        std::fs::create_dir_all(&parent)?;
        std::fs::create_dir(&outside)?;

        let root = DestinationRoot::open(&library_dir)?;
        let destination_file = root.resolve(&staged, &destination)?;
        let identity = destination_file.stage_regular(&source, &hash_path(&source)?)?;
        std::fs::rename(&parent, &detached)?;
        std::os::unix::fs::symlink(&outside, &parent)?;

        assert!(destination_file.finalize(&identity).is_err());
        assert!(!outside.join(".track.stage").exists());
        assert!(!outside.join("track.flac").exists());
        assert!(detached.join(".track.stage").exists());
        assert!(!detached.join("track.flac").exists());
        Ok(())
    }

    fn approved_plan(
        source: &Path,
        library_dir: &Path,
        action: Action,
    ) -> Result<ApprovedAlbumPlan> {
        // tempfile paths are rooted at `/var` on macOS, which is itself a
        // symlink. Runtime safety correctly rejects that alias; tests use the
        // stable physical parent so they exercise the intended operation.
        let library_dir = library_dir
            .parent()
            .ok_or_else(|| Error::Import("test library has no parent".into()))?
            .canonicalize()?
            .join(
                library_dir
                    .file_name()
                    .ok_or_else(|| Error::Import("test library has no name".into()))?,
            );
        let destination = library_dir.join("Artist/Album/01 - Track.flac");
        let metadata = std::fs::metadata(source)?;
        let mut planned_item = item(destination.clone(), "Track", 1);
        planned_item.file_size = Some(metadata.len());
        planned_item.mtime = metadata.modified()?.into();
        Ok(ApprovedAlbumPlan {
            album: Some(Album {
                id: None,
                album: "Album".into(),
                albumartist: "Artist".into(),
                year: None,
                artpath: None,
                external_id: None,
                added: Utc::now(),
                extended: crate::ExtendedMetadata::default(),
            }),
            tracks: vec![PlannedTrack {
                source: source.to_path_buf(),
                destination,
                fingerprint: SourceFingerprint {
                    size: metadata.len(),
                    modified: metadata.modified()?,
                    content_hash: hash_path(source)?,
                    identity: file_identity(&metadata),
                },
                item: planned_item,
                already_managed: false,
            }],
            artwork: None,
            action,
            library_dir,
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
    fn move_cleanup_preserves_an_identical_source_replacement() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        let original = temporary.path().join("original.flac");
        let library_dir = temporary.path().join("library");
        std::fs::write(&source, b"audio")?;
        let plan = approved_plan(&source, &library_dir, Action::Move)?;
        std::fs::rename(&source, &original)?;
        std::fs::write(&source, b"audio")?;

        assert!(remove_move_sources(&[&plan.tracks[0]]).is_err());
        assert_eq!(std::fs::read(source)?, b"audio");
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
        std::fs::create_dir_all(
            destination.parent().ok_or_else(|| {
                Error::Import("test destination unexpectedly has no parent".into())
            })?,
        )?;
        std::fs::write(&destination, b"collision")?;
        let mut library = Library::open_in_memory()?;
        assert!(ImportExecutor::new(&mut library).execute(plan).is_err());
        let mut value = String::new();
        std::fs::File::open(destination)?.read_to_string(&mut value)?;
        assert_eq!(value, "collision");
        assert!(library.query_items(&crate::query::Query::all())?.is_empty());
        Ok(())
    }

    #[test]
    fn executor_rejects_non_finite_item_metadata_before_writing() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        let library_dir = temporary.path().join("library");
        std::fs::write(&source, b"audio")?;
        let mut plan = approved_plan(&source, &library_dir, Action::Copy)?;
        let destination = plan.tracks[0].destination.clone();
        plan.tracks[0].item.length = f64::NAN;
        let mut library = Library::open_in_memory()?;

        assert!(ImportExecutor::new(&mut library).execute(plan).is_err());
        assert!(!destination.exists());
        assert!(library.query_items(&crate::query::Query::all())?.is_empty());
        Ok(())
    }

    #[test]
    fn executor_rejects_a_database_path_that_differs_from_its_destination() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        let library_dir = temporary.path().join("library");
        std::fs::write(&source, b"audio")?;
        let mut plan = approved_plan(&source, &library_dir, Action::Copy)?;
        let outside = temporary.path().join("outside.flac");
        plan.tracks[0].item.path = outside.clone();
        let destination = plan.tracks[0].destination.clone();
        let mut library = Library::open_in_memory()?;

        assert!(ImportExecutor::new(&mut library).execute(plan).is_err());

        assert!(!destination.exists());
        assert!(!outside.exists());
        assert!(library.query_items(&crate::query::Query::all())?.is_empty());
        Ok(())
    }
}
