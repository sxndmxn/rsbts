//! Built-in Discogs release and track metadata provider.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use num_traits::ToPrimitive;
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::DiscogsConfig;
use crate::provider::{
    MetadataProvider, ProviderEntityId, ProviderEntityKind, ProviderTrack, ReleaseCandidate,
    ReleaseQuery, SearchPage, TrackCandidate, TrackQuery,
};
use crate::{Error, Result};

const API_BASE: &str = "https://api.discogs.com";
const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub struct DiscogsProvider {
    http: reqwest::Client,
    token: String,
    next_request: Mutex<Instant>,
    request_interval: Duration,
    max_retries: u32,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct Release {
    id: u64,
    title: String,
    #[serde(default)]
    artists_sort: String,
    year: Option<i32>,
    #[serde(default)]
    tracklist: Vec<ReleaseTrack>,
    #[serde(default)]
    images: Vec<ReleaseImage>,
}

#[derive(Debug, Deserialize)]
struct ReleaseTrack {
    position: String,
    title: String,
    #[serde(default)]
    duration: String,
    #[serde(default)]
    artists: Vec<ReleaseArtist>,
}

#[derive(Debug, Deserialize)]
struct ReleaseArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseImage {
    #[serde(rename = "type")]
    image_type: String,
    uri: String,
}

impl DiscogsProvider {
    pub fn new(config: &DiscogsConfig, token: String) -> Result<Self> {
        config.validate()?;
        if token.trim().is_empty() {
            return Err(Error::Config("RSBTS_DISCOGS_TOKEN cannot be empty".into()));
        }
        let http = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| Error::Provider(format!("cannot create HTTP client: {error}")))?;
        Ok(Self {
            http,
            token,
            next_request: Mutex::new(Instant::now()),
            request_interval: Duration::try_from_secs_f64(config.rate_limit_seconds)
                .map_err(|error| Error::Config(format!("invalid Discogs rate limit: {error}")))?,
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
            let response = self
                .http
                .get(url)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Discogs token={}", self.token),
                )
                .send()
                .await;
            match response {
                Ok(value) if !is_transient(value.status()) || attempt >= self.max_retries => {
                    return Ok(value)
                }
                Ok(_) | Err(_) if attempt < self.max_retries => {
                    tokio::time::sleep(Duration::from_millis(
                        250 * 2_u64.saturating_pow(attempt.min(5)),
                    ))
                    .await;
                }
                Err(error) => return Err(Error::Provider(error.to_string())),
                Ok(value) => return Ok(value),
            }
            attempt += 1;
        }
    }

    async fn search_ids(
        &self,
        artist: &str,
        field: &str,
        value: &str,
        limit: u32,
    ) -> Result<Vec<u64>> {
        let url = format!(
            "{API_BASE}/database/search?type=release&artist={}&{field}={}&per_page={limit}",
            urlencoding::encode(artist),
            urlencoding::encode(value),
        );
        let response = self.get_with_retry(&url).await?;
        ensure_success(&response, "Discogs search")?;
        let response: SearchResponse = decode_json_limited(response, "Discogs search").await?;
        Ok(response
            .results
            .into_iter()
            .map(|result| result.id)
            .collect())
    }

    async fn get_release(&self, id: &str) -> Result<Release> {
        let parsed = id
            .parse::<u64>()
            .map_err(|_error| Error::Provider(format!("invalid Discogs release ID: {id}")))?;
        let response = self
            .get_with_retry(&format!("{API_BASE}/releases/{parsed}"))
            .await?;
        ensure_success(&response, "Discogs release lookup")?;
        decode_json_limited(response, "Discogs release lookup").await
    }
}

#[async_trait]
impl MetadataProvider for DiscogsProvider {
    fn name(&self) -> &'static str {
        "discogs"
    }

    async fn search_releases(&self, query: &ReleaseQuery, limit: u32) -> Result<SearchPage> {
        let ids = self
            .search_ids(&query.artist, "release_title", &query.album, limit)
            .await?;
        let requested = ids.len();
        let mut candidates = Vec::new();
        let mut errors = Vec::new();
        let count = requested.max(1);
        for (index, id) in ids.into_iter().enumerate() {
            match self.get_release(&id.to_string()).await {
                Ok(release) => {
                    let index = index.to_f64().unwrap_or(f64::MAX);
                    let count = count.to_f64().unwrap_or(f64::MAX);
                    candidates.push(release_candidate(release, 1.0 - index / count, false));
                }
                Err(error) => errors.push(crate::provider::ProviderFailure {
                    external_id: Some(id.to_string()),
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

    async fn search_tracks(&self, query: &TrackQuery, limit: u32) -> Result<Vec<TrackCandidate>> {
        let ids = self
            .search_ids(&query.artist, "track", &query.title, limit)
            .await?;
        let mut candidates = Vec::new();
        for id in ids {
            let release = self.get_release(&id.to_string()).await?;
            let release_id = release.id.to_string();
            let release_artist = release.artists_sort.clone();
            for track in release.tracklist {
                if track.title.eq_ignore_ascii_case(&query.title) {
                    candidates.push(track_candidate(&track, &release_artist, &release_id));
                }
            }
        }
        candidates.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(candidates)
    }

    async fn lookup_release(&self, id: &ProviderEntityId) -> Result<ReleaseCandidate> {
        if id.provider() != self.name() || id.kind() != ProviderEntityKind::Release {
            return Err(Error::Provider(
                "Discogs direct release lookup requires a Discogs release ID".into(),
            ));
        }
        Ok(release_candidate(
            self.get_release(id.value()).await?,
            1.0,
            true,
        ))
    }

    async fn lookup_track(&self, track_id: &str) -> Result<TrackCandidate> {
        let (release_id, position) = track_id.split_once(':').ok_or_else(|| {
            Error::Provider("Discogs track IDs use release:position syntax".into())
        })?;
        let release = self.get_release(release_id).await?;
        let track = release
            .tracklist
            .iter()
            .find(|track| track.position == position)
            .ok_or_else(|| Error::Provider(format!("Discogs track does not exist: {track_id}")))?;
        Ok(track_candidate(track, &release.artists_sort, release_id))
    }

    async fn fetch_cover_art(&self, release_id: &str) -> Result<Option<Vec<u8>>> {
        let release = self.get_release(release_id).await?;
        let Some(image) = release
            .images
            .iter()
            .find(|image| image.image_type == "primary")
            .or_else(|| release.images.first())
        else {
            return Ok(None);
        };
        let mut response = self.get_with_retry(&image.uri).await?;
        ensure_success(&response, "Discogs cover-art lookup")?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| Error::Provider(error.to_string()))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
                return Err(Error::Provider("Discogs cover art is too large".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(bytes))
    }
}

fn release_candidate(release: Release, provider_score: f64, explicit_id: bool) -> ReleaseCandidate {
    let artist = release.artists_sort.clone();
    let release_id = release.id.to_string();
    let tracks = release
        .tracklist
        .iter()
        .map(|track| {
            let (disc, number) = parse_position(&track.position);
            ProviderTrack {
                external_id: String::new(),
                release_track_external_id: Some(format!("{}:{}", release.id, track.position)),
                title: track.title.clone(),
                artist: track_artist(track, &artist),
                number,
                printed_position: Some(track.position.clone()),
                disc,
                length_ms: parse_duration(&track.duration),
                is_hidden: false,
                is_data_track: false,
                pregap_ms: None,
            }
        })
        .collect();
    ReleaseCandidate {
        provider: "discogs".into(),
        external_id: release_id,
        title: release.title,
        artist,
        year: release.year,
        provider_score,
        tracks,
        edition: crate::provider::EditionEvidence {
            explicit_id,
            ..crate::provider::EditionEvidence::default()
        },
    }
}

fn track_candidate(track: &ReleaseTrack, release_artist: &str, release_id: &str) -> TrackCandidate {
    TrackCandidate {
        provider: "discogs".into(),
        external_id: format!("{release_id}:{}", track.position),
        title: track.title.clone(),
        artist: track_artist(track, release_artist),
        length_ms: parse_duration(&track.duration),
        provider_score: 1.0,
        release_external_id: Some(release_id.into()),
    }
}

fn track_artist(track: &ReleaseTrack, fallback: &str) -> String {
    if track.artists.is_empty() {
        fallback.to_string()
    } else {
        track
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn parse_position(value: &str) -> (Option<u32>, Option<u32>) {
    for separator in ['-', '.'] {
        if let Some((disc, track)) = value.split_once(separator) {
            let disc = disc.trim().parse().ok();
            let track = track.trim().parse().ok();
            if disc.is_some() && track.is_some() {
                return (disc, track);
            }
        }
    }
    let digits = value
        .chars()
        .skip_while(|character| !character.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (None, digits.parse().ok())
}

fn parse_duration(value: &str) -> Option<u64> {
    let (minutes, seconds) = value.split_once(':')?;
    let minutes = minutes.parse::<u64>().ok()?;
    let seconds = seconds.parse::<u64>().ok()?;
    Some(minutes.saturating_mul(60_000) + seconds.saturating_mul(1_000))
}

fn is_transient(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
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
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| Error::Provider(error.to_string()))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_METADATA_BYTES {
            return Err(Error::Provider(format!(
                "{operation} response is too large"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| Error::Provider(format!("invalid {operation} response: {error}")))
}

#[cfg(test)]
mod tests {
    use super::{parse_duration, parse_position};

    #[test]
    fn parses_discogs_positions_and_durations() {
        assert_eq!(parse_position("A2"), (None, Some(2)));
        assert_eq!(parse_position("2-03"), (Some(2), Some(3)));
        assert_eq!(parse_duration("3:45"), Some(225_000));
        assert_eq!(parse_duration(""), None);
    }
}
