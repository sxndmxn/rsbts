//! `MusicBrainz` implementation of the metadata-provider contract.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::artwork::ArtworkRole;
use crate::config::MusicBrainzConfig;
use crate::provider::{
    ArtworkCandidate, EditionEvidence, MetadataProvider, ProviderEntityId, ProviderEntityKind,
    ProviderFailure, ProviderTrack, ReleaseCandidate, ReleaseQuery, SearchPage, TrackCandidate,
    TrackQuery,
};
use crate::{Error, Result};

const API_BASE: &str = "https://musicbrainz.org/ws/2";
const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_METADATA_BYTES_U64: u64 = MAX_METADATA_BYTES as u64;
const MAX_COVER_ART_BYTES: usize = 20 * 1024 * 1024;
const MAX_COVER_ART_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_COVER_ART_IMAGES: usize = 32;
const MAX_RETRY_AFTER_SECONDS: u64 = 60;

pub struct MusicBrainzProvider {
    http: reqwest::Client,
    next_request: Mutex<Instant>,
    request_interval: Duration,
    max_retries: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseSearchResult {
    releases: Vec<Release>,
}

#[derive(Debug, Clone, Deserialize)]
struct RecordingSearchResult {
    recordings: Vec<RecordingSearchEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct RecordingSearchEntry {
    id: String,
    title: String,
    length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    releases: Vec<RecordingRelease>,
    #[serde(default)]
    score: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct RecordingRelease {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Release {
    id: String,
    title: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    disambiguation: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    packaging: Option<String>,
    #[serde(default)]
    barcode: Option<String>,
    #[serde(rename = "release-group", default)]
    group: Option<ReleaseGroup>,
    #[serde(rename = "label-info", default)]
    label_info: Vec<LabelInfo>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    media: Vec<Medium>,
    #[serde(default)]
    score: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct ArtistCredit {
    artist: Artist,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    joinphrase: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Artist {
    id: String,
    name: String,
    #[serde(rename = "sort-name", default)]
    sort_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseGroup {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LabelInfo {
    #[serde(rename = "catalog-number", default)]
    catalog_number: Option<String>,
    #[serde(default)]
    label: Option<Label>,
}

#[derive(Debug, Clone, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Medium {
    #[serde(default)]
    position: u32,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "track-count", default)]
    track_count: Option<u32>,
    #[serde(default)]
    discs: Vec<Disc>,
    #[serde(rename = "data-tracks", default)]
    data_tracks: Vec<Track>,
    #[serde(default)]
    pregap: Option<Track>,
    #[serde(default)]
    tracks: Vec<Track>,
}

#[derive(Debug, Clone, Deserialize)]
struct Track {
    id: String,
    number: String,
    #[serde(default)]
    position: Option<u32>,
    title: String,
    length: Option<u64>,
    recording: Recording,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
}

#[derive(Debug, Clone, Deserialize)]
struct Recording {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Disc {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CoverArtResponse {
    #[serde(default)]
    images: Vec<CoverArtImage>,
}

#[derive(Debug, Clone, Deserialize)]
struct CoverArtImage {
    image: String,
    #[serde(default)]
    types: Vec<String>,
    #[serde(default)]
    front: bool,
    #[serde(default)]
    back: bool,
    #[serde(default)]
    approved: bool,
}

impl MusicBrainzProvider {
    pub fn new(config: &MusicBrainzConfig) -> Result<Self> {
        config.validate()?;
        let http = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|error| Error::Provider(format!("cannot create HTTP client: {error}")))?;
        let request_interval = Duration::try_from_secs_f64(config.rate_limit_seconds)
            .map_err(|error| Error::Config(format!("invalid MusicBrainz rate limit: {error}")))?;
        Ok(Self {
            http,
            next_request: Mutex::new(Instant::now()),
            request_interval,
            max_retries: config.max_retries,
        })
    }

    async fn reserve_request_slot(&self) {
        let scheduled = {
            let mut next = self.next_request.lock().await;
            let now = Instant::now();
            let scheduled = (*next).max(now);
            *next = scheduled + self.request_interval;
            scheduled
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(scheduled)).await;
    }

    async fn get_with_retry(&self, url: &str) -> Result<Response> {
        let mut attempt = 0;
        loop {
            self.reserve_request_slot().await;
            match self.http.get(url).send().await {
                Ok(response) if !is_transient(response.status()) => return Ok(response),
                Ok(response) if attempt >= self.max_retries => return Ok(response),
                Ok(response) => {
                    let delay = retry_delay(&response, attempt);
                    tokio::time::sleep(delay).await;
                }
                Err(error) if attempt >= self.max_retries => {
                    return Err(Error::Provider(error.to_string()));
                }
                Err(_) => {
                    tokio::time::sleep(exponential_delay(attempt)).await;
                }
            }
            attempt += 1;
        }
    }

    async fn lookup_release_detail(
        &self,
        release_id: &str,
        score: u32,
        explicit_id: bool,
    ) -> Result<ReleaseCandidate> {
        let release_id = urlencoding::encode(release_id);
        let url = format!(
            "{API_BASE}/release/{release_id}?inc=recordings+artist-credits+release-groups+labels+discids&fmt=json"
        );
        let response = self.get_with_retry(&url).await?;
        ensure_success(&response, "release lookup")?;
        let mut release: Release = decode_json_limited(response, "release lookup").await?;
        release.score = score;
        Ok(release.into_candidate(explicit_id))
    }

    async fn fetch_limited_bytes(
        &self,
        url: &str,
        limit: usize,
        description: &str,
    ) -> Result<Option<Vec<u8>>> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|error| Error::Provider(format!("invalid {description} URL: {error}")))?;
        let host = parsed.host_str().unwrap_or_default();
        if parsed.scheme() != "https"
            || !(host == "coverartarchive.org"
                || host == "archive.org"
                || host.ends_with(".archive.org"))
        {
            return Err(Error::Provider(format!(
                "refusing untrusted {description} URL"
            )));
        }
        let mut response = self.get_with_retry(parsed.as_str()).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ensure_success(&response, description)?;
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(Error::Provider(format!(
                "{description} exceeds the {limit}-byte limit"
            )));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| Error::Provider(error.to_string()))?
        {
            append_limited(&mut bytes, &chunk, limit, description)?;
        }
        Ok(Some(bytes))
    }
}

#[async_trait]
impl MetadataProvider for MusicBrainzProvider {
    fn name(&self) -> &'static str {
        "musicbrainz"
    }

    async fn search_releases(&self, query: &ReleaseQuery, limit: u32) -> Result<SearchPage> {
        let expression = format!(
            "artist:\"{}\" AND release:\"{}\"",
            escape_lucene_phrase(&query.artist),
            escape_lucene_phrase(&query.album)
        );
        let url = format!(
            "{API_BASE}/release?query={}&limit={limit}&fmt=json",
            urlencoding::encode(&expression)
        );
        let response = self.get_with_retry(&url).await?;
        ensure_success(&response, "release search")?;
        let result: ReleaseSearchResult = decode_json_limited(response, "release search").await?;

        let requested = result.releases.len();
        let mut candidates = Vec::with_capacity(requested);
        let mut errors = Vec::new();
        for release in result.releases {
            match self
                .lookup_release_detail(&release.id, release.score, false)
                .await
            {
                Ok(candidate) => candidates.push(candidate),
                Err(error) => errors.push(ProviderFailure {
                    external_id: Some(release.id),
                    detail: error.to_string(),
                    retriable: true,
                }),
            }
        }
        let resolved = candidates.len();
        Ok(SearchPage {
            candidates,
            requested,
            resolved,
            complete: errors.is_empty(),
            errors,
        })
    }

    async fn lookup_release(&self, id: &ProviderEntityId) -> Result<ReleaseCandidate> {
        if id.provider() != self.name() || id.kind() != ProviderEntityKind::Release {
            return Err(Error::Provider(
                "MusicBrainz direct release lookup requires a MusicBrainz release ID".into(),
            ));
        }
        self.lookup_release_detail(id.value(), 0, true).await
    }

    async fn search_tracks(&self, query: &TrackQuery, limit: u32) -> Result<Vec<TrackCandidate>> {
        let expression = format!(
            "artist:\"{}\" AND recording:\"{}\"",
            escape_lucene_phrase(&query.artist),
            escape_lucene_phrase(&query.title)
        );
        let url = format!(
            "{API_BASE}/recording?query={}&limit={limit}&fmt=json",
            urlencoding::encode(&expression)
        );
        let response = self.get_with_retry(&url).await?;
        ensure_success(&response, "recording search")?;
        let result: RecordingSearchResult =
            decode_json_limited(response, "recording search").await?;
        Ok(result
            .recordings
            .into_iter()
            .map(recording_candidate)
            .collect())
    }

    async fn lookup_track(&self, track_id: &str) -> Result<TrackCandidate> {
        let track_id = urlencoding::encode(track_id);
        let url = format!("{API_BASE}/recording/{track_id}?inc=artist-credits+releases&fmt=json");
        let response = self.get_with_retry(&url).await?;
        ensure_success(&response, "recording lookup")?;
        let entry: RecordingSearchEntry = decode_json_limited(response, "recording lookup").await?;
        Ok(recording_candidate(entry))
    }

    async fn fetch_cover_art(&self, release_id: &str) -> Result<Option<Vec<u8>>> {
        let artwork = self.fetch_artwork(release_id).await?;
        Ok(artwork
            .into_iter()
            .find(|candidate| candidate.role() == &ArtworkRole::Front)
            .map(|candidate| candidate.bytes().to_vec()))
    }

    async fn fetch_artwork(&self, release_id: &str) -> Result<Vec<ArtworkCandidate>> {
        let encoded = urlencoding::encode(release_id);
        let endpoint = format!("https://coverartarchive.org/release/{encoded}");
        let Some(payload) = self
            .fetch_limited_bytes(&endpoint, MAX_METADATA_BYTES, "cover-art metadata")
            .await?
        else {
            return Ok(Vec::new());
        };
        let response: CoverArtResponse = serde_json::from_slice(&payload)
            .map_err(|error| Error::Provider(format!("invalid cover-art metadata: {error}")))?;
        if response.images.len() > MAX_COVER_ART_IMAGES {
            return Err(Error::Provider(format!(
                "cover-art response exceeds the {MAX_COVER_ART_IMAGES}-image limit"
            )));
        }
        let mut total = 0_usize;
        let mut output = Vec::new();
        for image in response.images.into_iter().filter(|image| image.approved) {
            let Some(bytes) = self
                .fetch_limited_bytes(&image.image, MAX_COVER_ART_BYTES, "cover art")
                .await?
            else {
                continue;
            };
            total = total
                .checked_add(bytes.len())
                .filter(|total| *total <= MAX_COVER_ART_TOTAL_BYTES)
                .ok_or_else(|| Error::Provider("cover-art set exceeds total byte limit".into()))?;
            let role = cover_art_role(&image);
            output.push(ArtworkCandidate::new(
                bytes,
                role,
                image.image,
                Some(release_id.to_owned()),
                true,
                Some("source-specific:cover-art-archive".into()),
            )?);
        }
        Ok(output)
    }
}

fn cover_art_role(image: &CoverArtImage) -> ArtworkRole {
    if image.front
        || image
            .types
            .iter()
            .any(|kind| kind.eq_ignore_ascii_case("front"))
    {
        ArtworkRole::Front
    } else if image.back
        || image
            .types
            .iter()
            .any(|kind| kind.eq_ignore_ascii_case("back"))
    {
        ArtworkRole::Back
    } else if image
        .types
        .iter()
        .any(|kind| kind.eq_ignore_ascii_case("booklet"))
    {
        ArtworkRole::Booklet
    } else if image
        .types
        .iter()
        .any(|kind| kind.eq_ignore_ascii_case("medium") || kind.eq_ignore_ascii_case("disc"))
    {
        ArtworkRole::Disc
    } else if image
        .types
        .iter()
        .any(|kind| kind.eq_ignore_ascii_case("obi"))
    {
        ArtworkRole::Obi
    } else if image
        .types
        .iter()
        .any(|kind| kind.eq_ignore_ascii_case("spine"))
    {
        ArtworkRole::Spine
    } else {
        ArtworkRole::Other(
            image
                .types
                .first()
                .cloned()
                .unwrap_or_else(|| "other".into()),
        )
    }
}

fn recording_candidate(entry: RecordingSearchEntry) -> TrackCandidate {
    TrackCandidate {
        provider: "musicbrainz".into(),
        external_id: entry.id,
        title: entry.title,
        artist: format_artist_credit(&entry.artist_credit),
        length_ms: entry.length,
        provider_score: f64::from(entry.score.min(100)) / 100.0,
        release_external_id: entry.releases.first().map(|release| release.id.clone()),
    }
}

impl Release {
    fn artist_name(&self) -> String {
        format_artist_credit(&self.artist_credit)
    }

    fn year(&self) -> Option<i32> {
        self.date
            .as_ref()
            .and_then(|date| date.split('-').next())
            .and_then(|year| year.parse().ok())
    }

    fn into_candidate(self, explicit_id: bool) -> ReleaseCandidate {
        let artist = self.artist_name();
        let year = self.year();
        let tracks = self
            .media
            .iter()
            .flat_map(|medium| {
                let artist = artist.clone();
                medium
                    .tracks
                    .iter()
                    .chain(medium.data_tracks.iter())
                    .map(move |track| {
                        let track_artist = if track.artist_credit.is_empty() {
                            artist.clone()
                        } else {
                            format_artist_credit(&track.artist_credit)
                        };
                        ProviderTrack {
                            external_id: track.recording.id.clone(),
                            release_track_external_id: Some(track.id.clone()),
                            title: track.title.clone(),
                            artist: track_artist,
                            number: track.position.or_else(|| parse_track_number(&track.number)),
                            printed_position: Some(track.number.clone()),
                            disc: (medium.position > 0).then_some(medium.position),
                            length_ms: track.length,
                            is_hidden: false,
                            is_data_track: medium
                                .data_tracks
                                .iter()
                                .any(|data| data.id == track.id),
                            pregap_ms: None,
                        }
                    })
            })
            .collect();
        let edition = EditionEvidence {
            explicit_id,
            release_group_external_id: self.group.map(|group| group.id),
            disambiguation: self.disambiguation,
            country: self.country,
            date: self.date.clone(),
            status: self.status,
            packaging: self.packaging,
            barcode: self.barcode,
            labels_and_catalog_numbers: self
                .label_info
                .into_iter()
                .map(|info| {
                    (
                        info.label
                            .map_or_else(|| "[no label]".into(), |label| label.name),
                        info.catalog_number,
                    )
                })
                .collect(),
            media_formats: self
                .media
                .iter()
                .filter_map(|medium| medium.format.clone())
                .collect(),
            disc_ids: self
                .media
                .iter()
                .flat_map(|medium| medium.discs.iter().map(|disc| disc.id.clone()))
                .collect(),
            source_evidence: self
                .media
                .iter()
                .flat_map(|medium| {
                    [
                        medium
                            .title
                            .as_ref()
                            .map(|title| format!("medium title: {title}")),
                        medium
                            .track_count
                            .map(|count| format!("medium track count: {count}")),
                        medium
                            .pregap
                            .as_ref()
                            .map(|track| format!("pregap: {}", track.number)),
                    ]
                    .into_iter()
                    .flatten()
                })
                .collect(),
        };
        ReleaseCandidate {
            provider: "musicbrainz".into(),
            external_id: self.id,
            title: self.title,
            artist,
            year,
            provider_score: f64::from(self.score.min(100)) / 100.0,
            tracks,
            edition,
        }
    }
}

fn format_artist_credit(credits: &[ArtistCredit]) -> String {
    credits.iter().fold(String::new(), |mut output, credit| {
        let name = credit.name.as_ref().unwrap_or(&credit.artist.name);
        let _ = write!(output, "{name}{}", credit.joinphrase);
        let _ = (&credit.artist.id, &credit.artist.sort_name);
        output
    })
}

fn escape_lucene_phrase(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_track_number(number: &str) -> Option<u32> {
    number
        .split(['/', '-'])
        .next()
        .and_then(|part| part.trim().parse().ok())
}

fn ensure_success(response: &Response, operation: &str) -> Result<()> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(Error::Provider(format!(
            "{operation} returned HTTP {}",
            response.status()
        )))
    }
}

async fn decode_json_limited<T: DeserializeOwned>(
    mut response: Response,
    operation: &str,
) -> Result<T> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_METADATA_BYTES_U64)
    {
        return Err(Error::Provider(format!(
            "{operation} response exceeds the {MAX_METADATA_BYTES}-byte limit"
        )));
    }
    let mut bytes = Vec::new();
    let description = format!("{operation} response");
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| Error::Provider(error.to_string()))?
    {
        append_limited(&mut bytes, &chunk, MAX_METADATA_BYTES, &description)?;
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| Error::Provider(format!("invalid {operation} response: {error}")))
}

fn is_transient(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retry_delay(response: &Response, attempt: u32) -> Duration {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map_or_else(
            || exponential_delay(attempt),
            |seconds| Duration::from_secs(seconds.min(MAX_RETRY_AFTER_SECONDS)),
        )
}

fn exponential_delay(attempt: u32) -> Duration {
    Duration::from_millis(250 * 2_u64.saturating_pow(attempt.min(5)))
}

fn append_limited(
    output: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
    description: &str,
) -> Result<()> {
    let new_length = output
        .len()
        .checked_add(chunk.len())
        .filter(|length| *length <= limit)
        .ok_or_else(|| Error::Provider(format!("{description} exceeds the {limit}-byte limit")))?;
    output.reserve(new_length - output.len());
    output.extend_from_slice(chunk);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_number_parser_handles_common_forms() {
        assert_eq!(parse_track_number("02"), Some(2));
        assert_eq!(parse_track_number("2/10"), Some(2));
        assert_eq!(parse_track_number("A1"), None);
    }

    #[test]
    fn lucene_phrases_escape_quotes_and_backslashes() {
        assert_eq!(
            escape_lucene_phrase("AC\\DC \"Live\""),
            "AC\\\\DC \\\"Live\\\""
        );
    }

    #[test]
    fn cover_art_chunks_are_bounded() -> Result<()> {
        let mut output = vec![1, 2];
        append_limited(&mut output, &[3, 4], 4, "test data")?;
        assert_eq!(output, [1, 2, 3, 4]);
        assert!(append_limited(&mut output, &[5], 4, "test data").is_err());
        assert_eq!(output, [1, 2, 3, 4]);
        Ok(())
    }
}
