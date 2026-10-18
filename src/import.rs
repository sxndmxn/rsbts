//! Plan-first, album-atomic import workflow.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(not(unix))]
use std::fs::OpenOptions;

use chrono::Utc;
use indicatif::{ProgressBar, ProgressStyle};
use num_traits::ToPrimitive;
use pathfinding::matrix::Matrix;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::artwork::{
    original_path, validate_artwork, ArtworkAssetMetadata, ArtworkProvenance, ArtworkRole,
};
use crate::asset::{digest_file, digest_reader};
use crate::db::{
    file_identity, file_object_identity, hash_path, JournalFile, Library, OperationKind,
};
use crate::fsops::AnchoredRoot;
use crate::pathformat::format_relative_path;
use crate::provider::{
    MetadataProvider, ProviderEntityId, ProviderEntityKind, ProviderTrack, ReleaseCandidate,
    ReleaseQuery, SearchPage, TrackCandidate, TrackQuery,
};
use crate::roots::RootCapabilities;
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
#[non_exhaustive]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportPlan {
    pub albums: Vec<AlbumPlan>,
    pub scan_issues: Vec<ScanIssue>,
    pub provider_warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumPlan {
    pub source_artist: String,
    pub source_album: String,
    pub items: Vec<Item>,
    pub candidates: Vec<ScoredCandidate>,
    pub candidate_set_complete: bool,
    pub lookup_error: Option<String>,
    pub singleton: bool,
}

impl AlbumPlan {
    /// Return the compact release ID when all source release IDs agree.
    #[must_use]
    pub fn common_embedded_release_id(&self) -> Option<&ExternalId> {
        common_release_id(&self.items)
    }

    /// Check the compact release ID and all same-provider source release IDs.
    #[must_use]
    pub fn matches_embedded_release_id(&self, release: &ReleaseCandidate) -> bool {
        matches_embedded_release_id(&self.items, release)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredCandidate {
    pub release: ReleaseCandidate,
    pub confidence: ConfidenceBreakdown,
    pub track_matches: Vec<TrackMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceBreakdown {
    pub artist: f64,
    pub album: f64,
    pub mean_track: f64,
    pub provider: f64,
    pub composite: f64,
    /// Confidence that the files represent the candidate recordings.
    pub recording_identity: f64,
    /// Confidence that the album belongs to the candidate release group.
    pub release_group_identity: f64,
    /// Confidence in the exact edition, based only on edition-specific evidence.
    pub exact_release_identity: f64,
    pub runner_up_margin: Option<f64>,
    pub candidate_set_complete: bool,
    pub high_confidence: bool,
    pub gate_failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the booleans are independent, serialized matching-gate evidence rather than mutually exclusive state"
)]
pub struct TrackMatch {
    pub item_index: usize,
    pub track_index: usize,
    pub title_similarity: f64,
    pub duration_delta_seconds: Option<f64>,
    pub number_and_disc_match: bool,
    pub medium_match: bool,
    pub printed_position_match: bool,
    pub special_track_compatible: bool,
    pub pregap_compatible: bool,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    pub sha256: String,
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
    pub original: ArtworkAssetMetadata,
    pub original_already_present: bool,
    pub bytes: Vec<u8>,
    pub content_hash: String,
    pub sha256: String,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// Scan existing tags without querying metadata providers.
    pub fn plan_existing_tags(&self, paths: &[PathBuf]) -> ImportPlan {
        let progress = ConsoleProgress::new();
        let (items, scan_issues) = scan_paths(paths, self.options.follow_symlinks, &progress);
        let albums = group_into_albums(items)
            .into_iter()
            .map(|group| AlbumPlan {
                source_artist: group.artist,
                source_album: group.album,
                items: group.items,
                candidates: Vec::new(),
                candidate_set_complete: false,
                lookup_error: None,
                singleton: group.singleton,
            })
            .collect();
        ImportPlan {
            albums,
            scan_issues,
            provider_warnings: Vec::new(),
        }
    }

    /// Scan paths and query the metadata provider without mutating the library.
    #[allow(clippy::future_not_send)]
    #[expect(
        clippy::too_many_lines,
        reason = "planning keeps singleton and release candidate completeness decisions together"
    )]
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
                        candidate_set_complete: false,
                        lookup_error: None,
                        singleton: true,
                    }),
                    Err(error) => albums.push(AlbumPlan {
                        source_artist: group.artist,
                        source_album: group.album,
                        items: group.items,
                        candidates: Vec::new(),
                        candidate_set_complete: false,
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
            let direct_id = common_release_id(&group.items);
            let lookup = if let Some(id) = direct_id {
                match ProviderEntityId::new(id.provider(), ProviderEntityKind::Release, id.value())
                {
                    Ok(id) => self
                        .provider
                        .lookup_release(&id)
                        .await
                        .map(|candidate| SearchPage::complete(vec![candidate])),
                    Err(error) => Err(error),
                }
            } else {
                self.provider
                    .search_releases(&query, self.options.search_limit)
                    .await
            };
            match lookup {
                Ok(page) => {
                    let lookup_error = (!page.errors.is_empty()).then(|| {
                        page.errors
                            .iter()
                            .map(|error| error.detail.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    });
                    albums.push(AlbumPlan {
                        source_artist: group.artist,
                        source_album: group.album,
                        candidates: score_candidates(
                            &group.items,
                            page.candidates,
                            page.complete,
                            self.options.auto_accept_threshold,
                            self.options.runner_up_margin,
                        ),
                        items: group.items,
                        candidate_set_complete: page.complete,
                        lookup_error,
                        singleton: false,
                    });
                }
                Err(error) => albums.push(AlbumPlan {
                    source_artist: group.artist,
                    source_album: group.album,
                    items: group.items,
                    candidates: Vec::new(),
                    candidate_set_complete: false,
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

            let digests = digest_file(&source)?;
            let content_hash = digests.blake3().to_string();
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
                    sha256: digests.sha256().to_string(),
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
            .fetch_artwork(&candidate.release.external_id)
            .await
        {
            Ok(candidates) => {
                let Some(candidate_artwork) = candidates
                    .into_iter()
                    .find(|artwork| artwork.role() == &ArtworkRole::Front)
                else {
                    return Ok(None);
                };
                let bytes = candidate_artwork.bytes().to_vec();
                let validated = match validate_artwork(&bytes) {
                    Ok(validated) => validated,
                    Err(error) => {
                        warnings.push(format!(
                            "cover art failed bounded decode validation: {error}"
                        ));
                        return Ok(None);
                    }
                };
                let extension = validated.extension();
                let original_destination = original_path(&self.options.library_dir, &validated);
                let original_already_present =
                    original_destination.exists() || original_destination.is_symlink();
                if original_already_present {
                    let existing = digest_file(&original_destination)?;
                    if existing.sha256() != validated.sha256() {
                        return Err(Error::Import(format!(
                            "content-addressed artwork path contains different bytes: {}",
                            original_destination.display()
                        )));
                    }
                }
                let destination = artwork_parent.join(format!("cover.{extension}"));
                validate_destination(&self.options.library_dir, &destination)?;
                if destination.exists() || destination.is_symlink() {
                    warnings.push("existing cover art will not be overwritten".into());
                    Ok(None)
                } else {
                    if let Some(album) = album {
                        album.artpath = Some(destination.clone());
                    }
                    let content_hash = blake3::hash(&bytes).to_hex().to_string();
                    let provenance = ArtworkProvenance {
                        role: candidate_artwork.role().clone(),
                        source_provider: Some(self.provider.name().into()),
                        source_reference: Some(candidate_artwork.source_reference().into()),
                        provider_release_id: candidate_artwork
                            .provider_release_id()
                            .map(str::to_owned),
                        exact_release: candidate_artwork.exact_release(),
                        rights: candidate_artwork.rights().map(str::to_owned),
                    };
                    Ok(Some(PlannedArtwork {
                        destination,
                        original: ArtworkAssetMetadata::from_validated(
                            original_destination,
                            &validated,
                            provenance,
                        ),
                        original_already_present,
                        bytes,
                        content_hash,
                        sha256: validated.sha256().to_owned(),
                    }))
                }
            }
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
    #[allow(clippy::too_many_lines)]
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
        self.library.require_path_root_mutation(&plan.library_dir)?;
        for track in &active_tracks {
            self.library
                .require_path_root_mutation(&track.destination)?;
            if plan.action == Action::Move {
                self.library
                    .require_path_root_mutation(&std::fs::canonicalize(&track.source)?)?;
            }
        }
        if let Some(artwork) = &plan.artwork {
            if !artwork.original_already_present {
                self.library
                    .require_path_root_mutation(artwork.original.path())?;
            }
            self.library
                .require_path_root_mutation(&artwork.destination)?;
        }
        RootCapabilities::detect(&plan.library_dir)?.require_safe_mutation()?;

        let transfer_id = uuid::Uuid::new_v4();
        let mut journal_files = Vec::with_capacity(
            active_tracks.len()
                + plan.artwork.as_ref().map_or(0, |artwork| {
                    1 + usize::from(!artwork.original_already_present)
                }),
        );
        for track in &active_tracks {
            journal_files.push(JournalFile {
                source: track.source.clone(),
                staged: staging_path(&track.destination, transfer_id, "stage")?,
                destination: track.destination.clone(),
                content_hash: Some(track.fingerprint.content_hash.clone()),
                sha256: Some(track.fingerprint.sha256.clone()),
                source_identity: Some(track.fingerprint.identity.clone()),
                owned_identity: None,
                role: "track".into(),
                state: "prepared".into(),
            });
        }
        if let Some(artwork) = &plan.artwork {
            if !artwork.original_already_present {
                journal_files.push(JournalFile {
                    source: PathBuf::new(),
                    staged: staging_path(artwork.original.path(), transfer_id, "art-original")?,
                    destination: artwork.original.path().to_path_buf(),
                    content_hash: Some(artwork.content_hash.clone()),
                    sha256: Some(artwork.sha256.clone()),
                    source_identity: None,
                    owned_identity: None,
                    role: "artwork-original".into(),
                    state: "prepared".into(),
                });
            }
            journal_files.push(JournalFile {
                source: PathBuf::new(),
                staged: staging_path(&artwork.destination, transfer_id, "art")?,
                destination: artwork.destination.clone(),
                content_hash: Some(artwork.content_hash.clone()),
                sha256: Some(artwork.sha256.clone()),
                source_identity: None,
                owned_identity: None,
                role: "artwork-projection".into(),
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
        let root = match AnchoredRoot::open_or_create(&plan.library_dir) {
            Ok(root) => root,
            Err(error) => return Err(self.rollback_failed(&operation_id, error)),
        };
        if let Err(error) =
            self.stage_and_finalize(&root, &operation_id, &plan, &active_tracks, &journal_files)
        {
            return Err(self.rollback_failed(&operation_id, error));
        }

        let items = active_tracks
            .iter()
            .map(|track| track.item.clone())
            .collect::<Vec<_>>();
        if let Err(error) = self.library.commit_import_at_root_with_artwork(
            &operation_id,
            plan.album.as_ref(),
            &items,
            Some(&plan.library_dir),
            plan.artwork.as_ref().map(|artwork| &artwork.original),
        ) {
            return Err(self.rollback_failed(&operation_id, error));
        }

        let cleanup_recovered = false;
        if plan.action == Action::Move {
            self.library
                .set_operation_state(&operation_id, "cleanup-pending", None)?;
            let recovery = self.library.recover_pending()?;
            if !recovery.unresolved.is_empty() {
                return Err(Error::Recovery(recovery.unresolved.join("; ")));
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
            validate_destination(&plan.library_dir, artwork.original.path())?;
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
            if artwork.original_already_present {
                let existing = digest_file(artwork.original.path())?;
                if existing.blake3() != artwork.content_hash {
                    return Err(Error::Import(
                        "content-addressed original changed after planning".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn stage_and_finalize(
        &self,
        root: &AnchoredRoot,
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
            root.create_parent_all(&track.destination)?;
            match plan.action {
                Action::Copy | Action::Move => {
                    root.copy_new_observed(&track.source, &files[ordinal].staged, |metadata| {
                        self.library.set_acquired_file_identity(
                            operation_id,
                            ordinal,
                            &file_object_identity(metadata),
                        )
                    })?;
                    verify_anchored_digests(
                        root,
                        &files[ordinal].staged,
                        &track.fingerprint.content_hash,
                        &track.fingerprint.sha256,
                    )?;
                }
                Action::Link => {
                    root.symlink_new_observed(&track.source, &files[ordinal].staged, |metadata| {
                        self.library.set_acquired_file_identity(
                            operation_id,
                            ordinal,
                            &file_object_identity(metadata),
                        )
                    })?;
                    if root.read_link(&files[ordinal].staged)? != track.source {
                        return Err(Error::Import(format!(
                            "staged symlink validation failed: {}",
                            files[ordinal].staged.display()
                        )));
                    }
                }
                Action::InPlace => {
                    return Err(Error::Import(
                        "in-place imports do not have a file-staging phase".into(),
                    ));
                }
            }
            let staged_identity = file_identity(&root.entry_metadata(&files[ordinal].staged)?);
            self.library
                .set_staged_file_identity(operation_id, ordinal, &staged_identity)?;
        }

        if let Some(artwork) = &plan.artwork {
            for (ordinal, file) in files.iter().enumerate().skip(tracks.len()) {
                validate_destination(&plan.library_dir, &file.destination)?;
                root.create_parent_all(&file.destination)?;
                root.write_new_observed(&file.staged, &artwork.bytes, |metadata| {
                    self.library.set_acquired_file_identity(
                        operation_id,
                        ordinal,
                        &file_object_identity(metadata),
                    )
                })?;
                verify_anchored_digests(
                    root,
                    &file.staged,
                    &artwork.content_hash,
                    &artwork.sha256,
                )?;
                validate_artwork(&read_anchored(root, &file.staged)?)?;
                let staged_identity = file_identity(&root.entry_metadata(&file.staged)?);
                self.library
                    .set_staged_file_identity(operation_id, ordinal, &staged_identity)?;
            }
        }

        self.library
            .set_operation_state(operation_id, "finalizing", None)?;
        for (ordinal, track) in tracks.iter().enumerate() {
            validate_destination(&plan.library_dir, &track.destination)?;
            root.rename_noreplace(&files[ordinal].staged, &track.destination)?;
            self.library
                .set_file_state(operation_id, ordinal, "finalized")?;
        }
        if plan.artwork.is_some() {
            for (ordinal, file) in files.iter().enumerate().skip(tracks.len()) {
                validate_destination(&plan.library_dir, &file.destination)?;
                root.rename_noreplace(&file.staged, &file.destination)?;
                validate_artwork(&read_anchored(root, &file.destination)?)?;
                self.library
                    .set_file_state(operation_id, ordinal, "finalized")?;
            }
        }
        Ok(())
    }

    fn rollback_failed(&mut self, operation_id: &str, error: Error) -> Error {
        let _ = self
            .library
            .record_operation_failure(operation_id, &error.to_string());
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
        extended: shared_album_metadata(&items),
    };
    (Some(album), items)
}

fn shared_album_metadata(items: &[Item]) -> crate::ExtendedMetadata {
    let Some(first) = items.first() else {
        return crate::ExtendedMetadata::default();
    };
    let mut metadata = crate::ExtendedMetadata {
        date: shared_album_value(items, |value| &value.date),
        original_date: shared_album_value(items, |value| &value.original_date),
        disc_total: shared_album_value(items, |value| &value.disc_total),
        compilation: shared_album_value(items, |value| &value.compilation),
        label: shared_album_value(items, |value| &value.label),
        catalog_number: shared_album_value(items, |value| &value.catalog_number),
        country: shared_album_value(items, |value| &value.country),
        media: shared_album_value(items, |value| &value.media),
        language: shared_album_value(items, |value| &value.language),
        album_artists: shared_album_value(items, |value| &value.album_artists),
        genres: shared_album_value(items, |value| &value.genres),
        ..crate::ExtendedMetadata::default()
    };
    for name in [
        "labels",
        "catalog_numbers",
        "barcode",
        "replaygain_album_gain",
        "replaygain_album_peak",
    ] {
        if let Some(value) = first.extended.flexible_fields.get(name) {
            if items
                .iter()
                .all(|item| item.extended.flexible_fields.get(name) == Some(value))
            {
                metadata.flexible_fields.insert(name.into(), value.clone());
            }
        }
    }
    for id in &first.extended.external_ids {
        if matches!(id.kind(), "release" | "release-group" | "album-artist")
            && items.iter().all(|item| {
                let same_scope = |candidate: &&ExternalId| {
                    candidate.provider() == id.provider() && candidate.kind() == id.kind()
                };
                first
                    .extended
                    .external_ids
                    .iter()
                    .filter(same_scope)
                    .eq(item.extended.external_ids.iter().filter(same_scope))
            })
        {
            metadata.external_ids.push(id.clone());
        }
    }
    metadata
}

fn shared_album_value<T: Clone + Default + PartialEq>(
    items: &[Item],
    field: impl Fn(&crate::ExtendedMetadata) -> &T,
) -> T {
    let Some(first) = items.first() else {
        return T::default();
    };
    let value = field(&first.extended);
    if items.iter().all(|item| field(&item.extended) == value) {
        value.clone()
    } else {
        T::default()
    }
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
                Ok(entry) if entry.file_type().is_file() => {
                    match std::fs::canonicalize(entry.path()) {
                        Ok(canonical) if canonical.to_str().is_none() => {
                            issues.push(ScanIssue {
                                path: canonical,
                                message: "path is not valid UTF-8 and cannot be stored safely"
                                    .into(),
                            });
                        }
                        Ok(canonical)
                            if (is_audio_file(&canonical)
                                || direct_files.contains(&canonical)
                                || has_audio_extension(&canonical))
                                && seen.insert(canonical.clone()) =>
                        {
                            files.push(canonical);
                        }
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

fn has_audio_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            crate::AudioFormat::from_extension(extension) != crate::AudioFormat::Unknown
                || matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "wma" | "dsf" | "dff" | "mka"
                )
        })
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
                .and_then(|length| length.to_f64())
                .map(|length| (item.length - length / 1_000.0).abs());
            let duration_ok =
                duration_delta_seconds.is_none_or(|delta| delta <= DURATION_GATE_SECONDS);
            let provider = if track.provider_score.is_finite() {
                track.provider_score.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let composite = artist.mul_add(0.35, title.mul_add(0.55, provider * 0.10));
            let mut failures = Vec::new();
            failures.extend(provider_score_failure(track.provider_score));
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
                    release_track_external_id: None,
                    title: track.title,
                    artist: track.artist,
                    number: item.track,
                    printed_position: item.track.map(|value| value.to_string()),
                    disc: item.disc,
                    length_ms: track.length_ms,
                    is_hidden: false,
                    is_data_track: false,
                    pregap_ms: None,
                }],
                edition: crate::provider::EditionEvidence::default(),
            };
            ScoredCandidate {
                release,
                confidence: ConfidenceBreakdown {
                    artist,
                    album: 1.0,
                    mean_track: title,
                    provider,
                    composite,
                    recording_identity: title,
                    release_group_identity: 0.0,
                    exact_release_identity: 0.0,
                    runner_up_margin: None,
                    candidate_set_complete: false,
                    high_confidence: false,
                    gate_failures: failures,
                },
                track_matches: vec![TrackMatch {
                    item_index: 0,
                    track_index: 0,
                    title_similarity: title,
                    duration_delta_seconds,
                    number_and_disc_match: true,
                    medium_match: true,
                    printed_position_match: true,
                    special_track_compatible: true,
                    pregap_compatible: true,
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
        .map(|candidate| candidate.confidence.composite);
    if let Some(best) = candidates.first_mut() {
        apply_singleton_confidence_gates(best, runner_up, threshold, required_margin);
    }
    candidates
}

fn apply_singleton_confidence_gates(
    best: &mut ScoredCandidate,
    runner_up: Option<f64>,
    threshold: f64,
    required_margin: f64,
) {
    best.confidence.runner_up_margin =
        runner_up.map(|runner_up| best.confidence.composite - runner_up);
    let thresholds_valid =
        (0.0..=1.0).contains(&threshold) && (0.0..=1.0).contains(&required_margin);
    if !thresholds_valid {
        best.confidence
            .gate_failures
            .push("matching thresholds are invalid".into());
    }
    if !best.confidence.candidate_set_complete {
        best.confidence
            .gate_failures
            .push("provider candidate set is incomplete".into());
    }
    if thresholds_valid && best.confidence.composite < threshold {
        best.confidence.gate_failures.push(format!(
            "composite score {:.1}% is below {:.1}%",
            best.confidence.composite * 100.0,
            threshold * 100.0
        ));
    }
    if thresholds_valid
        && best
            .confidence
            .runner_up_margin
            .is_none_or(|margin| margin < required_margin)
    {
        best.confidence
            .gate_failures
            .push(best.confidence.runner_up_margin.map_or_else(
                || "runner-up margin is unknown".into(),
                |margin| {
                    format!(
                        "runner-up margin {:.1}% is below {:.1}%",
                        margin * 100.0,
                        required_margin * 100.0
                    )
                },
            ));
    }
    best.confidence.high_confidence = best.confidence.gate_failures.is_empty();
}

fn score_candidates(
    items: &[Item],
    releases: Vec<ReleaseCandidate>,
    candidate_set_complete: bool,
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
                    / track_matches.len().to_f64().unwrap_or(f64::MAX)
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
            let (exact_release_identity, edition_failures) =
                compare_release_edition(items, &release);
            ScoredCandidate {
                release,
                confidence: ConfidenceBreakdown {
                    artist,
                    album,
                    mean_track,
                    provider,
                    composite,
                    recording_identity: mean_track,
                    release_group_identity: artist
                        .mul_add(0.35, album.mul_add(0.35, mean_track * 0.30)),
                    exact_release_identity,
                    runner_up_margin: None,
                    candidate_set_complete,
                    high_confidence: false,
                    gate_failures: edition_failures,
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
        .map(|candidate| candidate.confidence.composite);
    if let Some(best) = candidates.first_mut() {
        best.confidence.runner_up_margin =
            runner_up.map(|runner_up| best.confidence.composite - runner_up);
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
    let mut failures = candidate.confidence.gate_failures.clone();
    failures.extend(provider_score_failure(candidate.release.provider_score));
    if !(0.0..=1.0).contains(&threshold) || !(0.0..=1.0).contains(&required_margin) {
        failures.push("matching thresholds are invalid".into());
    }
    if !candidate.confidence.candidate_set_complete {
        failures.push("provider candidate set is incomplete".into());
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
        if !matched.medium_match {
            failures.push(format!(
                "track {} crosses a candidate medium boundary",
                matched.item_index + 1
            ));
        }
        if !matched.printed_position_match {
            failures.push(format!(
                "track {} printed position contradicts its numeric position",
                matched.item_index + 1
            ));
        }
        if !matched.special_track_compatible {
            failures.push(format!(
                "track {} cannot substantiate candidate hidden/data-track state",
                matched.item_index + 1
            ));
        }
        if !matched.pregap_compatible {
            failures.push(format!(
                "track {} cannot substantiate candidate pregap evidence",
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
    let direct_release_identity = candidate.release.edition.explicit_id
        && matches_embedded_release_id(items, &candidate.release);
    match candidate.confidence.runner_up_margin {
        Some(margin) if margin + 1e-12 < required_margin => failures.push(format!(
            "runner-up margin {:.1} points is below {:.1}",
            margin * 100.0,
            required_margin * 100.0
        )),
        None if !direct_release_identity => failures
            .push("runner-up margin is unknown because fewer than two candidates resolved".into()),
        _ => {}
    }
    failures
}

fn provider_score_failure(score: f64) -> Option<String> {
    (!score.is_finite()).then(|| "provider score is not finite".into())
}

fn common_release_id(items: &[Item]) -> Option<&ExternalId> {
    let first = items.first()?.release_external_id.as_ref()?;
    if first.kind() != "release"
        || !items.iter().all(|item| {
            item.release_external_id.as_ref().is_some_and(|id| {
                id.provider() == first.provider()
                    && id.kind() == "release"
                    && same_provider_id_value(first.provider(), id.value(), first.value())
            })
        })
        || has_release_id_conflict(items, first.provider(), first.value())
    {
        return None;
    }
    Some(first)
}

fn same_provider_id_value(provider: &str, left: &str, right: &str) -> bool {
    if provider == "musicbrainz" {
        match (uuid::Uuid::parse_str(left), uuid::Uuid::parse_str(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
    } else {
        left == right
    }
}

fn has_release_id_conflict(items: &[Item], provider: &str, value: &str) -> bool {
    items
        .iter()
        .flat_map(|item| {
            item.release_external_id
                .iter()
                .chain(item.extended.external_ids.iter())
        })
        .filter(|id| id.provider() == provider && external_id_family(id.kind()) == "release")
        .any(|id| !same_provider_id_value(provider, id.value(), value))
}

fn matches_embedded_release_id(items: &[Item], release: &ReleaseCandidate) -> bool {
    common_release_id(items).is_some_and(|id| matches_release_id(id, release))
}

fn matches_release_id(id: &ExternalId, release: &ReleaseCandidate) -> bool {
    id.kind() == "release"
        && id.provider() == release.provider
        && same_provider_id_value(&release.provider, id.value(), &release.external_id)
}

/// Compare source edition facts and return credit and review reasons.
fn compare_release_edition(items: &[Item], release: &ReleaseCandidate) -> (f64, Vec<String>) {
    let evidence = &release.edition;
    let label_catalog = label_catalog_agreement(items, &evidence.labels_and_catalog_numbers);
    let country = edition_values_agree(
        items,
        |item| item.extended.country.as_deref(),
        evidence.country.iter().map(String::as_str),
    );
    let medium = edition_values_agree(
        items,
        |item| item.extended.media.as_deref(),
        evidence.media_formats.iter().map(String::as_str),
    );
    let mut score: f64 = 0.0;
    let mut failures = Vec::new();
    if items
        .iter()
        .filter_map(|item| item.release_external_id.as_ref())
        .filter(|id| id.provider() == release.provider)
        .any(|id| !matches_release_id(id, release))
        || has_release_id_conflict(items, &release.provider, &release.external_id)
    {
        failures.push("candidate contradicts an embedded release ID".into());
    }
    for (agreement, weight, reason) in [
        (
            label_catalog,
            0.35,
            "source label/catalog facts contradict candidate edition",
        ),
        (country, 0.1, "source country contradicts candidate edition"),
        (medium, 0.1, "source medium contradicts candidate edition"),
    ] {
        match agreement {
            Some(true) => score += weight,
            Some(false) => failures.push(reason.into()),
            None => {}
        }
    }
    if evidence.explicit_id && matches_embedded_release_id(items, release) && failures.is_empty() {
        score = 1.0;
    }
    (score, failures)
}

/// Missing facts are unknown. Every supplied source value must agree.
fn edition_values_agree<'a>(
    items: &[Item],
    source_value: fn(&Item) -> Option<&str>,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<bool> {
    let candidates = candidates
        .into_iter()
        .filter_map(edition_fact)
        .map(normalize_edition_fact)
        .collect::<Vec<_>>();
    let mut saw_source = false;
    for source in items
        .iter()
        .filter_map(source_value)
        .filter_map(edition_fact)
    {
        if candidates.is_empty() {
            return None;
        }
        saw_source = true;
        if !candidates.contains(&normalize_edition_fact(source)) {
            return Some(false);
        }
    }
    saw_source.then_some(true)
}

fn label_catalog_agreement(
    items: &[Item],
    candidates: &[(String, Option<String>)],
) -> Option<bool> {
    let mut saw_source = false;
    let mut unknown = false;
    let (known_labels, missing_labels) =
        known_edition_values(candidates.iter().map(|(label, _)| Some(label.as_str())));
    let (known_catalogs, missing_catalogs) =
        known_edition_values(candidates.iter().map(|(_, catalog)| catalog.as_deref()));
    for item in items {
        for (singular, field, known, missing) in [
            (
                item.extended.label.as_deref(),
                "labels",
                &known_labels,
                missing_labels,
            ),
            (
                item.extended.catalog_number.as_deref(),
                "catalog_numbers",
                &known_catalogs,
                missing_catalogs,
            ),
        ] {
            let plural = match item.extended.flexible_fields.get(field) {
                Some(crate::FlexibleValue::StringList(values)) => values.as_slice(),
                _ => &[],
            };
            for source in singular
                .into_iter()
                .chain(plural.iter().map(String::as_str))
                .filter_map(edition_fact)
            {
                saw_source = true;
                if !known.contains(&normalize_edition_fact(source)) {
                    if missing {
                        unknown = true;
                    } else {
                        return Some(false);
                    }
                }
            }
        }
        let label = item.extended.label.as_deref().and_then(edition_fact);
        let catalog = item
            .extended
            .catalog_number
            .as_deref()
            .and_then(edition_fact);
        if label.is_none() && catalog.is_none() {
            continue;
        }
        saw_source = true;
        let mut matched = false;
        let mut missing_candidate = candidates.is_empty();
        for (candidate_label, candidate_catalog) in candidates {
            let label_match = label
                .zip(edition_fact(candidate_label))
                .map(|(source, target)| {
                    normalize_edition_fact(source) == normalize_edition_fact(target)
                });
            let catalog_match = catalog
                .zip(candidate_catalog.as_deref().and_then(edition_fact))
                .map(|(source, target)| {
                    normalize_edition_fact(source) == normalize_edition_fact(target)
                });
            if label_match == Some(false) || catalog_match == Some(false) {
                continue;
            }
            if (label.is_none() || label_match == Some(true))
                && (catalog.is_none() || catalog_match == Some(true))
            {
                matched = true;
                break;
            }
            missing_candidate = true;
        }
        if !matched && !missing_candidate {
            return Some(false);
        }
        unknown |= !matched;
    }
    (saw_source && !unknown).then_some(true)
}

fn known_edition_values<'a>(values: impl Iterator<Item = Option<&'a str>>) -> (Vec<String>, bool) {
    let mut known = Vec::new();
    let mut missing = false;
    for value in values {
        if let Some(value) = value.and_then(edition_fact) {
            known.push(normalize_edition_fact(value));
        } else {
            missing = true;
        }
    }
    let missing = missing || known.is_empty();
    (known, missing)
}

fn edition_fact(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn normalize_edition_fact(value: &str) -> String {
    value
        .nfc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Assign source items to provider release tracks using the same bounded
/// structural matcher used by import planning.
///
/// Malformed, empty, or pathologically large inputs abstain with an empty
/// assignment rather than allocating an unbounded cost matrix.
#[must_use]
pub fn assign_release_tracks(items: &[Item], tracks: &[ProviderTrack]) -> Vec<TrackMatch> {
    match_tracks(items, tracks)
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
                    .and_then(|milliseconds| milliseconds.to_f64())
                    .map(|milliseconds| (item.length - milliseconds / 1000.0).abs())
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
            let number_and_disc_match = item.track.is_some()
                && provider_track.number.is_some()
                && (item.track == provider_track.number)
                && (item.disc.unwrap_or(1) == provider_track.disc.unwrap_or(1));
            let medium_match = item.disc.unwrap_or(1) == provider_track.disc.unwrap_or(1);
            let printed_position_match =
                provider_track
                    .printed_position
                    .as_ref()
                    .is_none_or(|printed| {
                        item.track
                            .is_some_and(|number| printed == &number.to_string())
                    });
            let special_track_compatible =
                !provider_track.is_hidden && !provider_track.is_data_track;
            let pregap_compatible = provider_track.pregap_ms.unwrap_or(0) == 0;
            let structural_score = [
                number_and_disc_match,
                medium_match,
                printed_position_match,
                special_track_compatible,
                pregap_compatible,
            ]
            .into_iter()
            .map(u8::from)
            .map(f64::from)
            .sum::<f64>()
                / 5.0;
            let score = if duration_delta_seconds.is_some() {
                title_similarity
                    .mul_add(0.55, duration_score.mul_add(0.25, structural_score * 0.20))
            } else {
                title_similarity.mul_add(0.75, structural_score * 0.25)
            };
            let score = if special_track_compatible && medium_match {
                score
            } else {
                score * 0.25
            };
            costs[item_index][track_index] = (-(score * 100_000.0)).round() as i64;
            scores[item_index][track_index] = Some(TrackMatch {
                item_index,
                track_index,
                title_similarity,
                duration_delta_seconds,
                number_and_disc_match,
                medium_match,
                printed_position_match,
                special_track_compatible,
                pregap_compatible,
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
        for id in item
            .track_external_id
            .iter()
            .chain(item.release_external_id.iter())
        {
            if !item.extended.external_ids.contains(id) {
                item.extended.external_ids.push(id.clone());
            }
        }
        item.album.clone_from(&candidate.release.title);
        if item.effective_albumartist() != candidate.release.artist {
            item.extended.album_artists = vec![candidate.release.artist.clone()];
        }
        item.albumartist = Some(candidate.release.artist.clone());
        if let Some(year) = candidate.release.year {
            if item.extended.date.year.or(item.year) != Some(year) {
                item.extended.date = crate::PartialDate {
                    year: Some(year),
                    month: None,
                    day: None,
                };
            }
            item.year = Some(year);
        }
        if let Some(release_id) = &release_id {
            item.release_external_id = Some(release_id.clone());
            replace_external_id(&mut item.extended.external_ids, release_id.clone());
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
        if item.artist != track.artist {
            item.extended.artists = vec![track.artist.clone()];
        }
        item.artist.clone_from(&track.artist);
        clear_exceeded_total(item.track, track.number, &mut item.extended.track_total);
        clear_exceeded_total(item.disc, track.disc, &mut item.extended.disc_total);
        item.track = track.number;
        item.disc = track.disc;
        if !track.external_id.is_empty() {
            let recording_id = ExternalId {
                provider: candidate.release.provider.clone(),
                kind: "recording".into(),
                value: track.external_id.clone(),
            };
            item.track_external_id = Some(recording_id.clone());
            replace_external_id(&mut item.extended.external_ids, recording_id);
        }
        if let Some(value) = track
            .release_track_external_id
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            let release_track_id = ExternalId {
                provider: candidate.release.provider.clone(),
                kind: "release-track".into(),
                value: value.clone(),
            };
            replace_external_id(&mut item.extended.external_ids, release_track_id);
        }
    }
    for item in items {
        if let (Some(track), Some(release)) = (&item.track_external_id, &item.release_external_id) {
            if track.provider() != release.provider() {
                if release.provider() == candidate.release.provider {
                    item.track_external_id = None;
                } else if track.provider() == candidate.release.provider {
                    item.release_external_id = None;
                }
            }
        }
    }
}

fn clear_exceeded_total(old: Option<u32>, selected: Option<u32>, total: &mut Option<u32>) {
    if old != selected
        && selected
            .zip(*total)
            .is_some_and(|(number, total)| number > total)
    {
        *total = None;
    }
}

fn external_id_family(kind: &str) -> &str {
    match kind {
        "legacy" => "release",
        "track" => "recording",
        "release_track" => "release-track",
        _ => kind,
    }
}

fn replace_external_id(ids: &mut Vec<ExternalId>, selected: ExternalId) {
    ids.retain(|id| {
        id.provider() != selected.provider()
            || external_id_family(id.kind()) != external_id_family(selected.kind())
            || id == &selected
    });
    if !ids.contains(&selected) {
        ids.push(selected);
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
    validate_artwork(bytes)
        .ok()
        .map(|artwork| artwork.extension())
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
    Ok(crate::fsops::bounded_sibling_path(
        destination,
        format!(".{name}.rsbts-{operation}.{suffix}"),
    ))
}

fn append_extension(path: &Path, extension: &std::ffi::OsStr) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".");
    value.push(extension);
    value.into()
}

fn validate_source(track: &PlannedTrack) -> Result<()> {
    let before = std::fs::metadata(&track.source)?;
    let digests = digest_file(&track.source)?;
    let after = std::fs::metadata(&track.source)?;
    if before.len() != track.fingerprint.size
        || before.modified()? != track.fingerprint.modified
        || file_identity(&before) != track.fingerprint.identity
        || file_identity(&after) != track.fingerprint.identity
        || digests.blake3() != track.fingerprint.content_hash
        || digests.sha256() != track.fingerprint.sha256
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
    let root = AnchoredRoot::open_or_create(library_dir)?;
    root.create_parent_all(&journal_file.staged)?;
    library.set_operation_state(operation_id, "staging", None)?;
    root.copy_new_observed(&track.source, &journal_file.staged, |metadata| {
        library.set_acquired_file_identity(operation_id, 0, &file_object_identity(metadata))
    })?;
    verify_anchored_digests(
        &root,
        &journal_file.staged,
        &track.fingerprint.content_hash,
        &track.fingerprint.sha256,
    )?;
    let identity = file_identity(&root.entry_metadata(&journal_file.staged)?);
    library.set_staged_file_full_evidence(
        operation_id,
        0,
        &identity,
        &track.fingerprint.content_hash,
        &track.fingerprint.sha256,
    )?;
    library.set_operation_state(operation_id, "finalizing", None)?;
    root.rename_noreplace(&journal_file.staged, &track.destination)?;
    library.set_file_state(operation_id, 0, "finalized")?;
    Ok(())
}

fn verify_anchored_digests(
    root: &AnchoredRoot,
    path: &Path,
    expected_blake3: &str,
    expected_sha256: &str,
) -> Result<()> {
    let digests = digest_reader(root.open_file(path)?)?;
    if digests.blake3() == expected_blake3 && digests.sha256() == expected_sha256 {
        Ok(())
    } else {
        Err(Error::Import(format!(
            "staged content verification failed: {}",
            path.display()
        )))
    }
}

fn read_anchored(root: &AnchoredRoot, path: &Path) -> Result<Vec<u8>> {
    let mut input = root.open_file(path)?;
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;

    use super::*;
    use crate::provider::SearchPage;
    use crate::AudioFormat;
    use async_trait::async_trait;
    use std::io::Read;

    struct EmptyProvider;

    #[async_trait]
    impl MetadataProvider for EmptyProvider {
        fn name(&self) -> &'static str {
            "empty"
        }

        async fn search_releases(&self, _query: &ReleaseQuery, _limit: u32) -> Result<SearchPage> {
            Ok(SearchPage::complete(Vec::new()))
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

    proptest! {
        #[test]
        fn arbitrary_track_sets_have_finite_unique_bounded_assignments(
            sources in proptest::collection::vec(("[a-zA-Z0-9 ]{0,32}", 0_u32..32, 0_u32..8), 0..32),
            candidates in proptest::collection::vec(("[a-zA-Z0-9 ]{0,32}", 0_u32..32, 0_u32..8), 0..32),
        ) {
            let items = sources
                .iter()
                .enumerate()
                .map(|(index, (title, track, disc))| {
                    let mut value = item(PathBuf::from(format!("{index}.wav")), title, *track);
                    value.disc = Some(*disc);
                    value
                })
                .collect::<Vec<_>>();
            let tracks = candidates
                .iter()
                .enumerate()
                .map(|(index, (title, number, disc))| ProviderTrack {
                    external_id: format!("recording-{index}"),
                    release_track_external_id: Some(format!("release-track-{index}")),
                    title: title.clone(),
                    artist: "Artist".into(),
                    number: Some(*number),
                    printed_position: Some(number.to_string()),
                    disc: Some(*disc),
                    length_ms: Some(index as u64 * 1_000),
                    is_hidden: false,
                    is_data_track: false,
                    pregap_ms: None,
                })
                .collect::<Vec<_>>();
            let assignments = assign_release_tracks(&items, &tracks);
            prop_assert!(assignments.len() <= items.len().min(tracks.len()));
            let assignments_valid = assignments.iter().all(|assignment| {
                assignment.item_index < items.len()
                    && assignment.track_index < tracks.len()
                    && assignment.score.is_finite()
            });
            prop_assert!(assignments_valid);
            let item_indices = assignments
                .iter()
                .map(|assignment| assignment.item_index)
                .collect::<HashSet<_>>();
            let track_indices = assignments
                .iter()
                .map(|assignment| assignment.track_index)
                .collect::<HashSet<_>>();
            prop_assert_eq!(item_indices.len(), assignments.len());
            prop_assert_eq!(track_indices.len(), assignments.len());
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
                    release_track_external_id: Some("release-track-one".into()),
                    title: "War Pigs".into(),
                    artist: "Black Sabbath".into(),
                    number: Some(1),
                    printed_position: Some("1".into()),
                    disc: Some(1),
                    length_ms: Some(180_000),
                    is_hidden: false,
                    is_data_track: false,
                    pregap_ms: None,
                },
                ProviderTrack {
                    external_id: "two".into(),
                    release_track_external_id: Some("release-track-two".into()),
                    title: second_track.into(),
                    artist: "Black Sabbath".into(),
                    number: Some(2),
                    printed_position: Some("2".into()),
                    disc: Some(1),
                    length_ms: Some(180_000),
                    is_hidden: false,
                    is_data_track: false,
                    pregap_ms: None,
                },
            ],
            edition: crate::provider::EditionEvidence::default(),
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
            true,
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
    fn a_single_candidate_has_unknown_margin_and_cannot_autoaccept() {
        let items = vec![
            item("/tmp/one.flac".into(), "War Pigs", 1),
            item("/tmp/two.flac".into(), "Paranoid", 2),
        ];
        let candidates =
            score_candidates(&items, vec![release("only", "Paranoid")], true, 0.92, 0.05);
        assert_eq!(candidates[0].confidence.runner_up_margin, None);
        assert!(!candidates[0].confidence.high_confidence);
        assert!(candidates[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("unknown")));
    }

    #[test]
    fn a_single_track_candidate_keeps_unknown_runner_up_margin() {
        let source = item("one.flac".into(), "War Pigs", 1);
        let candidate = TrackCandidate {
            provider: "test".into(),
            external_id: "only".into(),
            title: "War Pigs".into(),
            artist: "Black Sabbath".into(),
            length_ms: Some(180_000),
            provider_score: 1.0,
            release_external_id: None,
        };

        let candidates = score_track_candidates(&[source], vec![candidate], 0.92, 0.05);

        assert_eq!(candidates[0].confidence.runner_up_margin, None);
        assert!(!candidates[0].confidence.high_confidence);
        assert!(candidates[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("runner-up margin is unknown")));
    }

    #[test]
    fn a_direct_embedded_release_id_supplies_real_uniqueness_evidence() -> Result<()> {
        let release_id = ExternalId::new_typed("test", "release", "direct")?;
        let mut items = vec![
            item("/tmp/one.flac".into(), "War Pigs", 1),
            item("/tmp/two.flac".into(), "Paranoid", 2),
        ];
        for item in &mut items {
            item.release_external_id = Some(release_id.clone());
        }
        let mut direct = release("direct", "Paranoid");
        direct.edition.explicit_id = true;

        let candidates = score_candidates(&items, vec![direct], true, 0.92, 0.05);

        assert_eq!(candidates[0].confidence.runner_up_margin, None);
        assert!(
            candidates[0].confidence.high_confidence,
            "{:?}",
            candidates[0].confidence.gate_failures
        );
        assert!(candidates[0]
            .confidence
            .gate_failures
            .iter()
            .all(|failure| !failure.contains("runner-up")));
        Ok(())
    }

    fn edition_source_items() -> Vec<Item> {
        vec![
            item("one.flac".into(), "War Pigs", 1),
            item("two.flac".into(), "Paranoid", 2),
        ]
    }

    fn direct_musicbrainz_case() -> Result<(Vec<Item>, ReleaseCandidate)> {
        let value = "abcdef12-3456-4789-8123-123456789abc";
        let mut items = edition_source_items();
        for item in &mut items {
            item.release_external_id =
                Some(ExternalId::new_typed("musicbrainz", "release", value)?);
        }
        let mut candidate = release(value, "Paranoid");
        candidate.provider = "musicbrainz".into();
        candidate.edition.explicit_id = true;
        Ok((items, candidate))
    }

    fn assert_extended_release_id_conflict(item_index: usize, kind: &str) -> Result<()> {
        let (mut items, mut candidate) = direct_musicbrainz_case()?;
        if item_index == 0 {
            items.truncate(1);
            candidate.tracks.truncate(1);
        }
        items[item_index]
            .extended
            .external_ids
            .push(ExternalId::new_typed(
                "musicbrainz",
                kind,
                "22222222-2222-4222-8222-222222222222",
            )?);
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);
        assert!(
            !scored[0].confidence.high_confidence,
            "an extended release ID conflict retained direct-ID acceptance"
        );
        assert!(common_release_id(&items).is_none());
        assert!(scored[0].confidence.exact_release_identity.abs() < f64::EPSILON);
        assert!(scored[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("embedded release ID")));
        Ok(())
    }

    #[test]
    fn one_track_extended_release_id_conflict_requires_review() -> Result<()> {
        assert_extended_release_id_conflict(0, "release")
    }

    #[test]
    fn cross_track_extended_release_id_conflict_requires_review() -> Result<()> {
        assert_extended_release_id_conflict(1, "release")
    }

    #[test]
    fn legacy_extended_release_id_conflict_requires_review() -> Result<()> {
        assert_extended_release_id_conflict(0, "legacy")
    }

    #[test]
    fn opaque_extended_release_id_comparison_is_exact() -> Result<()> {
        let mut items = edition_source_items();
        for item in &mut items {
            item.release_external_id = Some(ExternalId::new_typed("test", "release", "opaque-A")?);
        }
        items[0]
            .extended
            .external_ids
            .push(ExternalId::new_typed("test", "release", "opaque-a")?);
        let mut candidate = release("opaque-A", "Paranoid");
        candidate.edition.explicit_id = true;
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);
        assert!(
            !scored[0].confidence.high_confidence,
            "different opaque release IDs retained direct-ID acceptance"
        );
        assert!(scored[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("embedded release ID")));
        Ok(())
    }

    #[test]
    fn musicbrainz_release_id_uuid_aliases_preserve_common_evidence() -> Result<()> {
        let value = "abcdef12-3456-4789-8123-123456789abc";
        let id = uuid::Uuid::parse_str(value)
            .map_err(|error| Error::Import(format!("invalid fixture UUID: {error}")))?;
        let mut accepted = Vec::new();
        for alias in [
            id.simple().to_string(),
            id.braced().to_string(),
            id.urn().to_string(),
            value.to_uppercase(),
        ] {
            let (mut items, candidate) = direct_musicbrainz_case()?;
            items[1].release_external_id =
                Some(ExternalId::new_typed("musicbrainz", "release", &alias)?);
            items[0].extended.external_ids.push(ExternalId::new_typed(
                "musicbrainz",
                "release",
                &alias,
            )?);
            accepted.push(
                score_candidates(&items, vec![candidate], true, 0.92, 0.05)[0]
                    .confidence
                    .high_confidence,
            );
        }
        assert_eq!(
            accepted, [true; 4],
            "equivalent UUID spellings lost evidence"
        );
        Ok(())
    }

    #[test]
    fn matching_and_unrelated_extended_release_ids_preserve_evidence() -> Result<()> {
        let mut accepted = Vec::new();
        for (provider, kind, value) in [
            (
                "musicbrainz",
                "release",
                "abcdef12-3456-4789-8123-123456789abc",
            ),
            ("discogs", "release", "123"),
            (
                "musicbrainz",
                "recording",
                "22222222-2222-4222-8222-222222222222",
            ),
        ] {
            let (mut items, candidate) = direct_musicbrainz_case()?;
            items[0]
                .extended
                .external_ids
                .push(ExternalId::new_typed(provider, kind, value)?);
            accepted.push(
                score_candidates(&items, vec![candidate], true, 0.92, 0.05)[0]
                    .confidence
                    .high_confidence,
            );
        }
        assert_eq!(
            accepted, [true; 3],
            "unrelated IDs blocked direct-ID evidence"
        );
        Ok(())
    }

    fn candidate_with_edition_facts() -> ReleaseCandidate {
        let mut candidate = release("edition", "Paranoid");
        candidate.edition.barcode = Some("1234567890123".into());
        candidate.edition.labels_and_catalog_numbers = vec![("Vertigo".into(), Some("VO6".into()))];
        candidate.edition.country = Some("US".into());
        candidate.edition.media_formats = vec!["Vinyl".into()];
        candidate.edition.disc_ids = vec!["candidate-disc-id".into()];
        candidate
    }

    fn selected_metadata_case() -> Result<(Item, ScoredCandidate)> {
        let old = "11111111-1111-4111-8111-111111111111";
        let mut source = item("fixture.flac".into(), "War Pigs", 1);
        source.release_external_id = Some(ExternalId::new_typed("musicbrainz", "release", old)?);
        source.track_external_id = Some(ExternalId::new_typed("musicbrainz", "recording", old)?);
        source.extended.artists = vec![source.artist.clone(), "Guest".into()];
        source.extended.album_artists = vec![source.artist.clone(), "Album Guest".into()];
        source.extended.date = crate::PartialDate {
            year: Some(1970),
            month: Some(10),
            day: Some(2),
        };
        source.extended.track_total = Some(2);
        source.extended.disc_total = Some(2);
        for kind in ["legacy", "track", "release_track"] {
            source
                .extended
                .external_ids
                .push(ExternalId::new_typed("musicbrainz", kind, old)?);
        }
        let mut selected = release("22222222-2222-4222-8222-222222222222", "Paranoid");
        selected.provider = "musicbrainz".into();
        selected.artist = "Selected Album Artist".into();
        selected.year = Some(2025);
        selected.tracks.truncate(1);
        selected.tracks[0].artist = "Selected Track Artist".into();
        selected.tracks[0].number = Some(4);
        selected.tracks[0].disc = Some(3);
        selected.tracks[0].external_id = "33333333-3333-4333-8333-333333333333".into();
        selected.tracks[0].release_track_external_id =
            Some("44444444-4444-4444-8444-444444444444".into());
        let candidate = score_candidates(
            std::slice::from_ref(&source),
            vec![selected],
            true,
            0.92,
            0.05,
        )
        .pop()
        .ok_or_else(|| Error::Import("fixture candidate is missing".into()))?;
        Ok((source, candidate))
    }

    fn selected_canonical_metadata(
        mut source: Item,
        candidate: &ScoredCandidate,
    ) -> Result<serde_json::Value> {
        source.extended.track_total = None;
        source.extended.disc_total = None;
        apply_release_metadata(std::slice::from_mut(&mut source), candidate);
        Ok(serde_json::to_value(crate::write::canonical_tags(
            &source,
        )?)?)
    }

    #[test]
    fn selected_metadata_updates_canonical_artist_lists() -> Result<()> {
        let (source, candidate) = selected_metadata_case()?;
        let tags = selected_canonical_metadata(source, &candidate)?;
        assert_eq!(
            (tags["track_artists"].clone(), tags["album_artists"].clone()),
            (
                serde_json::json!(["Selected Track Artist"]),
                serde_json::json!(["Selected Album Artist"])
            )
        );
        Ok(())
    }

    #[test]
    fn selected_metadata_updates_a_changed_canonical_year() -> Result<()> {
        let (source, candidate) = selected_metadata_case()?;
        assert_eq!(
            selected_canonical_metadata(source, &candidate)?["recording_date"],
            "2025"
        );
        Ok(())
    }

    #[test]
    fn selected_metadata_preserves_year_when_provider_omits_it() -> Result<()> {
        let (mut source, mut candidate) = selected_metadata_case()?;
        candidate.release.year = None;
        apply_release_metadata(std::slice::from_mut(&mut source), &candidate);
        assert_eq!(
            source.year,
            Some(1970),
            "missing provider year erased the known year"
        );
        assert_eq!(
            source.extended.date,
            crate::PartialDate {
                year: Some(1970),
                month: Some(10),
                day: Some(2),
            }
        );
        Ok(())
    }

    #[test]
    fn selected_metadata_replaces_identity_families_in_canonical_tags() -> Result<()> {
        let (source, candidate) = selected_metadata_case()?;
        let tags = selected_canonical_metadata(source, &candidate)?;
        assert_eq!(
            [
                tags["musicbrainz_release_ids"].clone(),
                tags["musicbrainz_recording_ids"].clone(),
                tags["musicbrainz_release_track_ids"].clone()
            ],
            [
                serde_json::json!(["22222222-2222-4222-8222-222222222222"]),
                serde_json::json!(["33333333-3333-4333-8333-333333333333"]),
                serde_json::json!(["44444444-4444-4444-8444-444444444444"])
            ],
            "selected IDs retained obsolete canonical or legacy identities"
        );
        Ok(())
    }

    #[test]
    fn selected_metadata_clears_only_exceeded_position_totals() -> Result<()> {
        let (mut source, candidate) = selected_metadata_case()?;
        apply_release_metadata(std::slice::from_mut(&mut source), &candidate);
        assert_eq!(
            (source.extended.track_total, source.extended.disc_total),
            (None, None),
            "changed positions exceed retained totals"
        );
        crate::write::canonical_tags(&source)?;
        Ok(())
    }

    #[test]
    fn selected_metadata_provider_change_preserves_missing_recording_evidence() -> Result<()> {
        let (mut source, mut candidate) = selected_metadata_case()?;
        candidate.release.provider = "other".into();
        candidate.release.external_id = "selected-release".into();
        candidate.release.tracks[0].external_id.clear();
        candidate.release.tracks[0].release_track_external_id = None;
        apply_release_metadata(std::slice::from_mut(&mut source), &candidate);
        assert!(
            source.track_external_id.is_none(),
            "compact IDs retained different providers"
        );
        assert!(source
            .extended
            .external_ids
            .iter()
            .any(|id| id.provider() == "musicbrainz"
                && id.kind() == "recording"
                && id.value() == "11111111-1111-4111-8111-111111111111"));
        crate::validate_item_metadata(&source)?;
        Ok(())
    }

    #[test]
    fn selected_metadata_preserves_unchanged_facts_and_valid_totals() -> Result<()> {
        let (mut source, mut candidate) = selected_metadata_case()?;
        let old = "11111111-1111-4111-8111-111111111111";
        candidate.release.artist.clone_from(&source.artist);
        candidate.release.year = source.year;
        candidate.release.external_id = old.into();
        candidate.release.tracks[0]
            .artist
            .clone_from(&source.artist);
        candidate.release.tracks[0].number = Some(2);
        candidate.release.tracks[0].disc = Some(2);
        candidate.release.tracks[0].external_id = old.into();
        candidate.release.tracks[0].release_track_external_id = Some(old.into());
        source.extended.external_ids.clear();
        for kind in ["release", "recording", "release-track", "artist"] {
            source
                .extended
                .external_ids
                .push(ExternalId::new_typed("musicbrainz", kind, old)?);
        }
        source
            .extended
            .external_ids
            .push(ExternalId::new_typed("discogs", "release", "123")?);
        source.extended.country = Some("US".into());
        source.extended.label = Some("Source label".into());
        source.extended.catalog_number = Some("SRC-1".into());
        source.extended.media = Some("Vinyl".into());
        source.extended.genres = vec!["Rock".into(), "Metal".into()];
        source.extended.composers = vec!["Composer".into()];
        source.extended.original_date = crate::PartialDate {
            year: Some(1969),
            month: None,
            day: None,
        };
        source.extended.flexible_fields.insert(
            "replaygain_track_gain".into(),
            crate::FlexibleValue::String("-6.0 dB".into()),
        );
        source.extended.flexible_fields.insert(
            "custom_note".into(),
            crate::FlexibleValue::String("Keep".into()),
        );
        let expected = source.extended.clone();
        apply_release_metadata(std::slice::from_mut(&mut source), &candidate);
        assert_eq!(
            source.extended, expected,
            "unchanged or unrelated source facts were lost"
        );
        let tags = serde_json::to_value(crate::write::canonical_tags(&source)?)?;
        assert_eq!(tags["recording_date"], "1970-10-02");
        assert_eq!(
            tags["track_artists"],
            serde_json::json!(["Black Sabbath", "Guest"])
        );
        Ok(())
    }

    #[test]
    fn exact_release_confidence_rejects_unmatched_explicit_ids() -> Result<()> {
        let mut observations = Vec::new();
        for (provider, kind, value) in [
            ("other", "release", "edition"),
            ("test", "recording", "edition"),
            ("test", "release", "other"),
        ] {
            let mut items = edition_source_items();
            let id = ExternalId::new_typed(provider, kind, value)?;
            for item in &mut items {
                item.release_external_id = Some(id.clone());
            }
            let mut candidate = release("edition", "Paranoid");
            candidate.edition.explicit_id = true;
            observations.extend(
                score_candidates(&items, vec![candidate], true, 0.92, 0.05)
                    .into_iter()
                    .map(|candidate| (provider, candidate)),
            );
        }

        assert!(
            observations.iter().all(|(_, candidate)| candidate
                .confidence
                .exact_release_identity
                .abs()
                < f64::EPSILON),
            "unmatched provider, ID kind, or ID value received exact-release credit: {:?}",
            observations
                .iter()
                .map(|(_, candidate)| candidate.confidence.exact_release_identity)
                .collect::<Vec<_>>()
        );
        for (provider, candidate) in observations {
            assert_eq!(
                candidate
                    .confidence
                    .gate_failures
                    .iter()
                    .any(|failure| failure.contains("embedded release ID")),
                provider == "test",
                "complementary providers must not create a release-ID contradiction"
            );
            assert!(candidate
                .confidence
                .gate_failures
                .iter()
                .any(|failure| failure.contains("runner-up margin is unknown")));
        }
        Ok(())
    }

    #[test]
    fn exact_release_confidence_ignores_candidate_only_facts() {
        let items = edition_source_items();
        let scored = score_candidates(
            &items,
            vec![candidate_with_edition_facts()],
            true,
            0.92,
            0.05,
        );

        assert!(
            scored[0].confidence.exact_release_identity.abs() < f64::EPSILON,
            "candidate-only metadata received exact-release credit"
        );
        assert!((scored[0].confidence.composite - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn exact_release_confidence_uses_matching_source_facts() {
        let mut items = edition_source_items();
        for item in &mut items {
            item.extended.label = Some("  VERTIGO ".into());
            item.extended.catalog_number = Some(" vo6 ".into());
            item.extended.country = Some("us".into());
            item.extended.media = Some(" vinyl ".into());
        }
        let scored = score_candidates(
            &items,
            vec![candidate_with_edition_facts()],
            true,
            0.92,
            0.05,
        );

        assert!(
            (scored[0].confidence.exact_release_identity - 0.55).abs() < f64::EPSILON,
            "edition credit did not use the matching source facts"
        );
        assert!(scored[0]
            .confidence
            .gate_failures
            .iter()
            .all(|failure| !failure.contains("contradict")));
    }

    #[test]
    fn edition_contradictions_require_review_without_conflicting_credit() -> Result<()> {
        let mut items = edition_source_items();
        let id = ExternalId::new_typed("test", "release", "edition")?;
        for item in &mut items {
            item.release_external_id = Some(id.clone());
            item.extended.label = Some("Other label".into());
            item.extended.catalog_number = Some("OTHER-1".into());
            item.extended.country = Some("GB".into());
            item.extended.media = Some("CD".into());
        }
        let mut candidate = candidate_with_edition_facts();
        candidate.edition.explicit_id = true;
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);

        assert!(
            !scored[0].confidence.high_confidence,
            "a shared release ID hid contradictory source edition facts"
        );
        assert!(scored[0].confidence.exact_release_identity.abs() < f64::EPSILON);
        assert_eq!(
            scored[0].confidence.gate_failures,
            [
                "source label/catalog facts contradict candidate edition",
                "source country contradicts candidate edition",
                "source medium contradicts candidate edition",
            ]
        );
        Ok(())
    }

    #[test]
    fn catalog_number_punctuation_distinguishes_editions() {
        let mut items = edition_source_items();
        for item in &mut items {
            item.extended.label = Some("Vertigo".into());
            item.extended.catalog_number = Some("VO-6".into());
        }
        let mut candidate = release("edition", "Paranoid");
        candidate.edition.labels_and_catalog_numbers = vec![("Vertigo".into(), Some("VO6".into()))];
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);

        assert!(
            scored[0].confidence.exact_release_identity.abs() < f64::EPSILON,
            "different catalog numbers received the same edition credit"
        );
        assert!(scored[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("label/catalog")));
    }

    fn secondary_edition_values(
        field: &str,
        secondary: &str,
    ) -> Result<(Vec<Item>, ReleaseCandidate)> {
        let (mut items, mut candidate) = direct_musicbrainz_case()?;
        for item in &mut items {
            item.extended.label = Some("Vertigo".into());
            item.extended.catalog_number = Some("VO6".into());
        }
        let primary = if field == "labels" { "Vertigo" } else { "VO6" };
        items[0].extended.flexible_fields.insert(
            field.into(),
            crate::FlexibleValue::StringList(vec![primary.into(), secondary.into()]),
        );
        candidate.edition.labels_and_catalog_numbers = vec![("Vertigo".into(), Some("VO6".into()))];
        Ok((items, candidate))
    }

    fn assert_secondary_edition_conflict(field: &str) -> Result<()> {
        let (items, candidate) = secondary_edition_values(field, "Contradictory value")?;
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);
        assert!(
            !scored[0].confidence.high_confidence,
            "a shared release ID hid contradictory secondary {field}"
        );
        assert!(scored[0].confidence.exact_release_identity.abs() < f64::EPSILON);
        assert!(scored[0]
            .confidence
            .gate_failures
            .iter()
            .any(|reason| reason.contains("label/catalog")));
        Ok(())
    }

    #[test]
    fn secondary_label_contradictions_require_review() -> Result<()> {
        assert_secondary_edition_conflict("labels")
    }

    #[test]
    fn secondary_catalog_contradictions_require_review() -> Result<()> {
        assert_secondary_edition_conflict("catalog_numbers")
    }

    #[test]
    fn secondary_edition_matching_duplicates_keep_shared_id_acceptance() -> Result<()> {
        let (mut items, candidate) = secondary_edition_values("labels", "  VERTIGO  ")?;
        items[0].extended.flexible_fields.insert(
            "catalog_numbers".into(),
            crate::FlexibleValue::StringList(vec!["VO6".into(), " vo6 ".into()]),
        );
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);
        assert!(
            scored[0].confidence.high_confidence,
            "matching repeated source values lost shared-ID acceptance: {:?}",
            scored[0].confidence.gate_failures
        );
        Ok(())
    }

    #[test]
    fn secondary_edition_missing_candidate_catalog_stays_unknown() -> Result<()> {
        let (items, mut candidate) =
            secondary_edition_values("catalog_numbers", "Unresolved catalog")?;
        candidate
            .edition
            .labels_and_catalog_numbers
            .push(("Vertigo".into(), None));
        assert_eq!(
            label_catalog_agreement(&items, &candidate.edition.labels_and_catalog_numbers),
            None
        );
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);
        assert!(
            scored[0].confidence.high_confidence,
            "missing candidate evidence became a definite contradiction: {:?}",
            scored[0].confidence.gate_failures
        );
        Ok(())
    }

    #[test]
    fn label_and_catalog_must_match_the_same_candidate_entry() {
        let mut items = edition_source_items();
        for item in &mut items {
            item.extended.label = Some("Vertigo".into());
            item.extended.catalog_number = Some("VO6".into());
        }
        let mut candidate = release("edition", "Paranoid");
        candidate.edition.labels_and_catalog_numbers = vec![
            ("Vertigo".into(), Some("OTHER".into())),
            ("Other label".into(), Some("VO6".into())),
        ];
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);

        assert!(
            scored[0].confidence.exact_release_identity.abs() < f64::EPSILON,
            "separate label and catalog entries supplied a false matched pair"
        );
        assert!(scored[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("label/catalog")));
    }

    #[test]
    fn missing_candidate_edition_facts_stay_unknown() {
        let mut items = edition_source_items();
        for item in &mut items {
            item.extended.label = Some("Vertigo".into());
            item.extended.catalog_number = Some("VO6".into());
            item.extended.country = Some("US".into());
            item.extended.media = Some("Vinyl".into());
        }
        let mut candidate = candidate_with_edition_facts();
        candidate.edition.country = None;
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);

        assert!(
            (scored[0].confidence.exact_release_identity - 0.45).abs() < f64::EPSILON,
            "missing country evidence received credit or suppressed matching facts"
        );
        assert!(scored[0]
            .confidence
            .gate_failures
            .iter()
            .all(|failure| !failure.contains("contradict")));
    }

    #[test]
    fn conflicting_source_countries_require_review() {
        let mut items = edition_source_items();
        items[0].extended.country = Some("US".into());
        items[1].extended.country = Some("GB".into());
        let mut candidate = release("edition", "Paranoid");
        candidate.edition.country = Some("US".into());
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);

        assert!(
            scored[0].confidence.exact_release_identity.abs() < f64::EPSILON,
            "one track hid another track's contradictory country evidence"
        );
        assert!(scored[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("source country contradicts")));
    }

    #[test]
    fn conflicting_embedded_release_ids_require_review() -> Result<()> {
        let mut items = edition_source_items();
        items[0].release_external_id = Some(ExternalId::new_typed("test", "release", "edition")?);
        items[1].release_external_id = Some(ExternalId::new_typed("test", "release", "other")?);
        let mut candidate = release("edition", "Paranoid");
        candidate.edition.explicit_id = true;
        let scored = score_candidates(&items, vec![candidate], true, 0.92, 0.05);

        assert!(
            scored[0]
                .confidence
                .gate_failures
                .iter()
                .any(|failure| failure.contains("embedded release ID")),
            "conflicting source IDs received no explicit review reason"
        );
        assert!(scored[0].confidence.exact_release_identity.abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn an_incomplete_candidate_page_always_abstains() {
        let items = vec![
            item("/tmp/one.flac".into(), "War Pigs", 1),
            item("/tmp/two.flac".into(), "Paranoid", 2),
        ];
        let candidates = score_candidates(
            &items,
            vec![release("best", "Paranoid"), release("other", "Wrong")],
            false,
            0.92,
            0.05,
        );
        assert!(!candidates[0].confidence.high_confidence);
        assert!(candidates[0]
            .confidence
            .gate_failures
            .iter()
            .any(|failure| failure.contains("incomplete")));
    }

    #[test]
    fn pathological_track_sets_fail_closed_before_matrix_allocation() {
        let source = item(PathBuf::from("source.flac"), "Track", 1);
        let provider = ProviderTrack {
            external_id: "track".into(),
            release_track_external_id: None,
            title: "Track".into(),
            artist: "Artist".into(),
            number: Some(1),
            printed_position: Some("1".into()),
            disc: Some(1),
            length_ms: Some(180_000),
            is_hidden: false,
            is_data_track: false,
            pregap_ms: None,
        };
        assert!(match_tracks(&vec![source; MAX_MATCH_TRACKS + 1], &[provider]).is_empty());
    }

    #[test]
    fn high_confidence_exposes_each_gate() {
        let items = vec![
            item("/tmp/one.flac".into(), "War Pigs", 1),
            item("/tmp/two.flac".into(), "Paranoid", 2),
        ];
        let mut other = release("other", "Wrong");
        other.tracks[0].title = "Different".into();
        let candidates = score_candidates(
            &items,
            vec![release("best", "Paranoid"), other],
            true,
            0.92,
            0.05,
        );
        assert!(
            candidates[0].confidence.high_confidence,
            "{:?}",
            candidates[0].confidence.gate_failures
        );
        assert!(candidates[0].confidence.composite >= 0.92);
    }

    #[test]
    fn non_finite_provider_scores_fail_closed() -> Result<()> {
        let release_id = ExternalId::new_typed("test", "release", "direct")?;
        let mut items = vec![
            item("one.flac".into(), "War Pigs", 1),
            item("two.flac".into(), "Paranoid", 2),
        ];
        for item in &mut items {
            item.release_external_id = Some(release_id.clone());
        }
        let mut candidate = release("direct", "Paranoid");
        candidate.edition.explicit_id = true;
        let scored = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY]
            .into_iter()
            .map(|score| {
                candidate.provider_score = score;
                score_candidates(&items, vec![candidate.clone()], true, 0.85, 0.05)
            })
            .collect::<Vec<_>>();

        assert!(
            scored
                .iter()
                .all(|result| !result[0].confidence.high_confidence),
            "non-finite scores passed all confidence rules: {:?}",
            scored
                .iter()
                .map(|result| result[0].confidence.high_confidence)
                .collect::<Vec<_>>()
        );
        for result in scored {
            assert!(result[0].confidence.provider.abs() < f64::EPSILON);
            assert_eq!(
                result[0].confidence.gate_failures,
                ["provider score is not finite"]
            );
        }
        candidate.provider_score = 0.0;
        let finite = score_candidates(&items, vec![candidate], true, 0.85, 0.05);
        assert!(finite[0].confidence.high_confidence);
        Ok(())
    }

    #[test]
    fn non_finite_singleton_scores_report_malformed_provider_evidence() {
        let source = item("one.flac".into(), "War Pigs", 1);
        let candidates = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY]
            .into_iter()
            .enumerate()
            .map(|(index, score)| TrackCandidate {
                provider: "test".into(),
                external_id: format!("invalid-{index}"),
                title: "War Pigs".into(),
                artist: "Black Sabbath".into(),
                length_ms: Some(180_000),
                provider_score: score,
                release_external_id: None,
            })
            .collect();
        let scored = score_track_candidates(&[source], candidates, 0.85, 0.05);

        assert!(
            scored.iter().all(|result| result
                .confidence
                .gate_failures
                .iter()
                .any(|failure| failure == "provider score is not finite")),
            "singleton scoring hid malformed provider scores behind other rules"
        );
        assert!(scored
            .iter()
            .all(|result| result.confidence.provider.abs() < f64::EPSILON));
        assert!(scored
            .iter()
            .all(|result| !result.confidence.high_confidence));
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
            true,
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
        let image = image::DynamicImage::new_rgb8(1, 1);
        let mut png = std::io::Cursor::new(Vec::new());
        assert!(image.write_to(&mut png, image::ImageFormat::Png).is_ok());
        assert_eq!(artwork_extension(png.get_ref()), Some("png"));
        assert_eq!(artwork_extension(b"\x89PNG\r\n\x1a\nrest"), None);
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
            candidate_set_complete: true,
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn regular_file_finalization_never_overwrites() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let temporary_path = temporary.path().canonicalize()?;
        let staged = temporary_path.join("stage");
        let destination = temporary_path.join("destination");
        std::fs::write(&staged, b"new")?;
        std::fs::write(&destination, b"old")?;
        let root = AnchoredRoot::open_or_create(&temporary_path)?;
        assert!(root.rename_noreplace(&staged, &destination).is_err());
        let mut value = String::new();
        std::fs::File::open(destination)?.read_to_string(&mut value)?;
        assert_eq!(value, "old");
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn library_creation_rejects_a_symlinked_ancestor() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let outside = temporary.path().join("outside");
        let redirected = temporary.path().join("redirected");
        std::fs::create_dir(&outside)?;
        std::os::unix::fs::symlink(&outside, &redirected)?;

        assert!(AnchoredRoot::open_or_create(&redirected.join("library")).is_err());
        assert!(!outside.join("library").exists());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
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
        std::fs::write(&source, b"audio")?;
        std::fs::create_dir_all(&parent)?;
        std::fs::create_dir(&outside)?;

        let root = AnchoredRoot::open_or_create(&library_dir)?;
        std::fs::rename(&parent, &detached)?;
        std::os::unix::fs::symlink(&outside, &parent)?;

        assert!(root
            .copy_new_observed(&source, &staged, |_metadata| Ok(()))
            .is_err());
        assert!(!outside.join(".track.stage").exists());
        assert!(!outside.join("track.flac").exists());
        assert!(!detached.join(".track.stage").exists());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
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

        let root = AnchoredRoot::open_or_create(&library_dir)?;
        root.copy_new_observed(&source, &staged, |_metadata| Ok(()))?;
        std::fs::rename(&parent, &detached)?;
        std::os::unix::fs::symlink(&outside, &parent)?;

        assert!(root.rename_noreplace(&staged, &destination).is_err());
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
        let digests = digest_file(source)?;
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
                    content_hash: digests.blake3().to_string(),
                    sha256: digests.sha256().to_string(),
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn import_rejects_disabled_destination_roots() -> Result<()> {
        use crate::roots::RootState;

        for action in [Action::Copy, Action::Move, Action::Link] {
            for state in [RootState::Offline, RootState::ReadOnly, RootState::Degraded] {
                let temporary = tempfile::tempdir()?;
                let source = temporary.path().join("source.flac");
                let destination_root = temporary.path().join("library");
                std::fs::write(&source, b"generated audio")?;
                let plan = approved_plan(&source, &destination_root, action)?;
                let mut library = Library::open_in_memory()?;
                let root = library.register_root(&plan.library_dir)?;
                library.set_root_state(root.id(), state)?;

                assert!(
                    ImportExecutor::new(&mut library)
                        .execute(plan.clone())
                        .is_err(),
                    "import {action:?} ignored {state:?} destination policy"
                );
                assert_eq!(std::fs::read(&source)?, b"generated audio");
                assert!(!destination_root.exists());
                assert!(library.query_items(&crate::query::Query::all())?.is_empty());
                let journals: u64 = library.conn.query_row(
                    "SELECT COUNT(*) FROM operation_journal",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(journals, 0);
                library.set_root_state(root.id(), RootState::Online)?;
                assert_eq!(
                    ImportExecutor::new(&mut library)
                        .execute(plan)?
                        .imported_tracks,
                    1
                );
            }
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn import_audio_rejects_disabled_child_roots() -> Result<()> {
        use crate::roots::RootState;

        for action in [Action::Copy, Action::Move, Action::Link] {
            let temporary = tempfile::tempdir()?;
            let source = temporary.path().join("source.flac");
            let destination_root = temporary.path().join("library");
            std::fs::write(&source, b"generated audio")?;
            let plan = approved_plan(&source, &destination_root, action)?;
            let mut library = Library::open_in_memory()?;
            library.register_root(&plan.library_dir)?;
            let nested = library.register_root(
                plan.tracks[0]
                    .destination
                    .parent()
                    .ok_or_else(|| Error::Import("test destination has no parent".into()))?,
            )?;
            library.set_root_state(nested.id(), RootState::Offline)?;

            assert!(
                ImportExecutor::new(&mut library)
                    .execute(plan.clone())
                    .is_err(),
                "import {action:?} wrote into a disabled child root"
            );
            assert_eq!(std::fs::read(&source)?, b"generated audio");
            assert!(!destination_root.exists());
            let journals: u64 =
                library
                    .conn
                    .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| {
                        row.get(0)
                    })?;
            assert_eq!(journals, 0);
            library.set_root_state(nested.id(), RootState::Online)?;
            assert_eq!(
                ImportExecutor::new(&mut library)
                    .execute(plan)?
                    .imported_tracks,
                1
            );
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn import_artwork_rejects_disabled_original_child_roots() -> Result<()> {
        require_disabled_artwork_child_root(true)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn import_artwork_rejects_disabled_projection_child_roots() -> Result<()> {
        require_disabled_artwork_child_root(false)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn require_disabled_artwork_child_root(original_root: bool) -> Result<()> {
        use crate::roots::RootState;

        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        let destination_root = temporary.path().join("library");
        std::fs::write(&source, b"generated audio")?;
        let mut plan = approved_plan(&source, &destination_root, Action::Copy)?;
        let mut output = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1).write_to(&mut output, image::ImageFormat::Png)?;
        let bytes = output.into_inner();
        let validated = validate_artwork(&bytes)?;
        let original = original_path(&plan.library_dir, &validated);
        let projection = plan.library_dir.join("Artwork/cover.png");
        plan.album
            .as_mut()
            .ok_or_else(|| Error::Import("test import has no album".into()))?
            .artpath = Some(projection.clone());
        let disabled_path = if original_root {
            &original
        } else {
            &projection
        };
        let mut library = Library::open_in_memory()?;
        library.register_root(&plan.library_dir)?;
        let nested = library.register_root(
            disabled_path
                .parent()
                .ok_or_else(|| Error::Import("test artwork has no parent".into()))?,
        )?;
        let provenance = ArtworkProvenance {
            role: ArtworkRole::Front,
            source_provider: Some("test".into()),
            source_reference: Some("generated".into()),
            provider_release_id: None,
            exact_release: false,
            rights: None,
        };
        plan.artwork = Some(PlannedArtwork {
            destination: projection,
            original: ArtworkAssetMetadata::from_validated(original, &validated, provenance),
            original_already_present: false,
            content_hash: blake3::hash(&bytes).to_hex().to_string(),
            sha256: validated.sha256().to_owned(),
            bytes,
        });
        library.set_root_state(nested.id(), RootState::Offline)?;

        assert!(
            ImportExecutor::new(&mut library)
                .execute(plan.clone())
                .is_err(),
            "artwork import wrote into a disabled child root: original={original_root}"
        );
        assert_eq!(std::fs::read(&source)?, b"generated audio");
        assert!(!destination_root.exists());
        let journals: u64 =
            library
                .conn
                .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(journals, 0);
        library.set_root_state(nested.id(), RootState::Online)?;
        assert_eq!(
            ImportExecutor::new(&mut library)
                .execute(plan)?
                .imported_tracks,
            1
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn in_place_cataloging_preserves_disabled_root_semantics() -> Result<()> {
        use crate::roots::RootState;

        for state in [RootState::Offline, RootState::ReadOnly, RootState::Degraded] {
            let temporary = tempfile::tempdir()?;
            let source = temporary.path().join("source.flac");
            let destination_root = temporary.path().join("library");
            std::fs::write(&source, b"generated audio")?;
            let mut plan = approved_plan(&source, &destination_root, Action::InPlace)?;
            plan.tracks[0].destination.clone_from(&source);
            plan.tracks[0].item.path.clone_from(&source);
            let mut library = Library::open_in_memory()?;
            let root = library.register_root(&plan.library_dir)?;
            library.set_root_state(root.id(), state)?;

            let result = ImportExecutor::new(&mut library).execute(plan);
            assert!(
                result.is_ok(),
                "in-place cataloging rejected {state:?} root policy: {result:?}"
            );
            assert_eq!(result?.imported_tracks, 1);
            assert_eq!(std::fs::read(&source)?, b"generated audio");
            assert!(!destination_root.exists());
            assert_eq!(library.query_items(&crate::query::Query::all())?.len(), 1);
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn import_move_preserves_disabled_source_roots() -> Result<()> {
        use crate::roots::RootState;

        for state in [RootState::Offline, RootState::ReadOnly, RootState::Degraded] {
            let temporary = tempfile::tempdir()?;
            let source_root = temporary.path().join("incoming");
            std::fs::create_dir(&source_root)?;
            let source = source_root.join("source.flac");
            let destination_root = temporary.path().join("library");
            std::fs::write(&source, b"generated audio")?;
            let plan = approved_plan(&source, &destination_root, Action::Move)?;
            let mut library = Library::open_in_memory()?;
            let root = library.register_root(&source_root)?;
            library.set_root_state(root.id(), state)?;

            assert!(
                ImportExecutor::new(&mut library)
                    .execute(plan.clone())
                    .is_err(),
                "import move ignored {state:?} source policy"
            );
            assert_eq!(std::fs::read(&source)?, b"generated audio");
            assert!(!destination_root.exists());
            assert!(library.query_items(&crate::query::Query::all())?.is_empty());
            let journals: u64 =
                library
                    .conn
                    .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| {
                        row.get(0)
                    })?;
            assert_eq!(journals, 0);
            library.set_root_state(root.id(), RootState::Online)?;
            assert_eq!(
                ImportExecutor::new(&mut library)
                    .execute(plan)?
                    .imported_tracks,
                1
            );
            assert!(!source.exists());
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn every_import_safety_boundary_recovers_without_losing_unowned_bytes() -> Result<()> {
        fn one(
            failure: Option<(usize, Option<i32>)>,
        ) -> Result<(Result<ImportReport>, Vec<&'static str>)> {
            let temporary = tempfile::tempdir()?;
            let source = temporary.path().join("source.flac");
            let library_dir = temporary.path().join("library");
            std::fs::write(&source, b"unowned source bytes")?;
            let plan = approved_plan(&source, &library_dir, Action::Copy)?;
            let destination = plan.tracks[0].destination.clone();
            let mut library = Library::open_in_memory()?;
            let (result, hits) = match failure {
                None => crate::failpoints::run_recording(|| {
                    ImportExecutor::new(&mut library).execute(plan)
                }),
                Some((index, None)) => crate::failpoints::run_failing(index, || {
                    ImportExecutor::new(&mut library).execute(plan)
                }),
                Some((index, Some(raw_os_error))) => {
                    crate::failpoints::run_failing_io(index, raw_os_error, || {
                        let result = ImportExecutor::new(&mut library).execute(plan);
                        assert!(
                            matches!(result, Err(Error::Io(ref error)) if error.raw_os_error() == Some(raw_os_error)),
                            "executor did not propagate the injected operating-system error: {result:?}"
                        );
                        result
                    })
                }
            };

            let recovery = library.recover_pending()?;
            assert!(recovery.unresolved.is_empty(), "{recovery:?}");
            assert!(library.recover_pending()?.unresolved.is_empty());
            assert_eq!(std::fs::read(&source)?, b"unowned source bytes");
            let items = library.query_items(&crate::query::Query::all())?;
            if destination.exists() {
                assert_eq!(std::fs::read(&destination)?, b"unowned source bytes");
                assert_eq!(items.len(), 1);
            } else {
                assert!(items.is_empty());
            }
            if library_dir.exists() {
                for entry in walkdir::WalkDir::new(&library_dir) {
                    let entry = entry.map_err(|error| Error::Import(error.to_string()))?;
                    let name = entry.file_name().to_string_lossy();
                    assert!(
                        !name.contains(".rsbts-") || entry.path() == destination,
                        "orphaned staging entry: {}",
                        entry.path().display()
                    );
                }
            }
            Ok((result, hits))
        }

        let (success, boundaries) = one(None)?;
        assert!(success.is_ok());
        assert!(boundaries.len() >= 10, "insufficient boundary coverage");
        for index in 0..boundaries.len() {
            let (result, observed) = one(Some((index, None)))?;
            assert!(
                result.is_err(),
                "boundary {index} did not fail: {observed:?}"
            );
        }
        for (boundary, raw_os_error) in [
            ("fs.before-create-new-file", 13),
            ("fs.before-copy-staged-bytes", 28),
        ] {
            let index = boundaries
                .iter()
                .position(|observed| observed == &boundary)
                .ok_or_else(|| Error::Operation(format!("missing {boundary} boundary")))?;
            let (result, observed) = one(Some((index, Some(raw_os_error))))?;
            assert!(
                matches!(result, Err(Error::Io(ref error)) if error.raw_os_error() == Some(raw_os_error)),
                "{boundary} did not propagate its operating-system error: {observed:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn move_execution_preserves_an_identical_source_replacement() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source.flac");
        let original = temporary.path().join("original.flac");
        let library_dir = temporary.path().join("library");
        std::fs::write(&source, b"audio")?;
        let plan = approved_plan(&source, &library_dir, Action::Move)?;
        std::fs::rename(&source, &original)?;
        std::fs::write(&source, b"audio")?;

        let mut library = Library::open_in_memory()?;
        assert!(ImportExecutor::new(&mut library).execute(plan).is_err());
        assert_eq!(std::fs::read(source)?, b"audio");
        assert_eq!(std::fs::read(original)?, b"audio");
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
