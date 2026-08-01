//! `MusicBrainz` implementation of the metadata-provider contract.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::MusicBrainzConfig;
use crate::provider::{MetadataProvider, ProviderTrack, ReleaseCandidate, ReleaseQuery};
use crate::{Error, Result};

const API_BASE: &str = "https://musicbrainz.org/ws/2";
const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_COVER_ART_BYTES: usize = 20 * 1024 * 1024;
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
struct Release {
    id: String,
    title: String,
    #[serde(default)]
    date: Option<String>,
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
    joinphrase: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Artist {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Medium {
    #[serde(default)]
    position: u32,
    #[serde(default)]
    tracks: Vec<Track>,
}

#[derive(Debug, Clone, Deserialize)]
struct Track {
    number: String,
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

impl MusicBrainzProvider {
    pub fn new(config: &MusicBrainzConfig) -> Result<Self> {
        config.validate()?;
        let http = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .timeout(Duration::from_secs(30))
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

    async fn lookup_release(&self, release_id: &str, score: u32) -> Result<ReleaseCandidate> {
        let release_id = urlencoding::encode(release_id);
        let url = format!("{API_BASE}/release/{release_id}?inc=recordings+artist-credits&fmt=json");
        let response = self.get_with_retry(&url).await?;
        ensure_success(&response, "release lookup")?;
        let mut release: Release = decode_json_limited(response, "release lookup").await?;
        release.score = score;
        Ok(release.into_candidate())
    }
}

#[async_trait]
impl MetadataProvider for MusicBrainzProvider {
    fn name(&self) -> &'static str {
        "musicbrainz"
    }

    async fn search_releases(
        &self,
        query: &ReleaseQuery,
        limit: u32,
    ) -> Result<Vec<ReleaseCandidate>> {
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

        let mut candidates = Vec::with_capacity(result.releases.len());
        let mut first_error = None;
        for release in result.releases {
            match self.lookup_release(&release.id, release.score).await {
                Ok(candidate) => candidates.push(candidate),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if candidates.is_empty() {
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        Ok(candidates)
    }

    async fn fetch_cover_art(&self, release_id: &str) -> Result<Option<Vec<u8>>> {
        let release_id = urlencoding::encode(release_id);
        let url = format!("https://coverartarchive.org/release/{release_id}/front");
        let mut response = self.get_with_retry(&url).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ensure_success(&response, "cover-art lookup")?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_COVER_ART_BYTES as u64)
        {
            return Err(Error::Provider(format!(
                "cover art exceeds the {MAX_COVER_ART_BYTES}-byte limit"
            )));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| Error::Provider(error.to_string()))?
        {
            append_limited(&mut bytes, &chunk, MAX_COVER_ART_BYTES, "cover art")?;
        }
        Ok(Some(bytes))
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

    fn into_candidate(self) -> ReleaseCandidate {
        let artist = self.artist_name();
        let year = self.year();
        let tracks = self
            .media
            .iter()
            .flat_map(|medium| {
                let artist = artist.clone();
                medium.tracks.iter().map(move |track| {
                    let track_artist = if track.artist_credit.is_empty() {
                        artist.clone()
                    } else {
                        format_artist_credit(&track.artist_credit)
                    };
                    ProviderTrack {
                        external_id: track.recording.id.clone(),
                        title: track.title.clone(),
                        artist: track_artist,
                        number: parse_track_number(&track.number),
                        disc: (medium.position > 0).then_some(medium.position),
                        length_ms: track.length,
                    }
                })
            })
            .collect();
        ReleaseCandidate {
            provider: "musicbrainz".into(),
            external_id: self.id,
            title: self.title,
            artist,
            year,
            provider_score: f64::from(self.score.min(100)) / 100.0,
            tracks,
        }
    }
}

fn format_artist_credit(credits: &[ArtistCredit]) -> String {
    credits.iter().fold(String::new(), |mut output, credit| {
        let _ = write!(output, "{}{}", credit.artist.name, credit.joinphrase);
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
        .is_some_and(|length| length > MAX_METADATA_BYTES as u64)
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
