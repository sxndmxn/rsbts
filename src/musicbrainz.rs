//! `MusicBrainz` API client

use std::fmt::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::{Error, Result};

const API_BASE: &str = "https://musicbrainz.org/ws/2";
const USER_AGENT: &str = "rsbts/0.1.0 (https://github.com/user/rsbts)";
const RATE_LIMIT: Duration = Duration::from_secs(1);

pub struct Client {
    http: reqwest::Client,
    last_request: Mutex<Option<Instant>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseSearchResult {
    pub releases: Vec<Release>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    pub media: Vec<Medium>,
    #[serde(default)]
    pub score: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistCredit {
    pub artist: Artist,
    #[serde(default)]
    pub joinphrase: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Medium {
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub id: String,
    pub number: String,
    pub title: String,
    pub length: Option<u64>,
    pub recording: Recording,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Recording {
    pub id: String,
    pub title: String,
    pub length: Option<u64>,
}

impl Client {
    /// Create a new `MusicBrainz` API client.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built (should never happen).
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(30))
                .build()
                .expect("Failed to create HTTP client"),
            last_request: Mutex::new(None),
        }
    }

    async fn rate_limit(&self) {
        let sleep_duration = {
            let mut last = self.last_request.lock().unwrap();
            let duration = last.and_then(|last_time| {
                let elapsed = last_time.elapsed();
                RATE_LIMIT.checked_sub(elapsed)
            });
            *last = Some(Instant::now());
            duration
        };

        if let Some(d) = sleep_duration {
            tokio::time::sleep(d).await;
        }
    }

    /// Search for releases matching artist and album.
    ///
    /// # Errors
    /// Returns an error if the API request fails.
    #[allow(clippy::future_not_send)]
    pub async fn search_release(
        &self,
        artist: &str,
        album: &str,
        limit: u32,
    ) -> Result<Vec<Release>> {
        self.rate_limit().await;

        let query = format!("artist:{artist} AND release:{album}");
        let url = format!(
            "{API_BASE}/release?query={}&limit={limit}&fmt=json",
            urlencoding::encode(&query)
        );

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::MusicBrainz(e.to_string()))?;

        if !response.status().is_success() {
            return Err(Error::MusicBrainz(format!(
                "API error: {}",
                response.status()
            )));
        }

        let result: ReleaseSearchResult = response
            .json()
            .await
            .map_err(|e| Error::MusicBrainz(e.to_string()))?;

        Ok(result.releases)
    }

    /// Look up a release by `MusicBrainz` ID.
    ///
    /// # Errors
    /// Returns an error if the API request fails.
    #[allow(clippy::future_not_send)]
    pub async fn lookup_release(&self, mbid: &str) -> Result<Release> {
        self.rate_limit().await;

        let url = format!("{API_BASE}/release/{mbid}?inc=recordings+artist-credits&fmt=json");

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::MusicBrainz(e.to_string()))?;

        if !response.status().is_success() {
            return Err(Error::MusicBrainz(format!(
                "API error: {}",
                response.status()
            )));
        }

        response
            .json()
            .await
            .map_err(|e| Error::MusicBrainz(e.to_string()))
    }

    /// Fetch cover art for a release.
    ///
    /// # Errors
    /// Returns an error if the API request fails.
    #[allow(clippy::future_not_send)]
    pub async fn fetch_cover_art(&self, mbid: &str) -> Result<Option<Vec<u8>>> {
        self.rate_limit().await;

        let url = format!("https://coverartarchive.org/release/{mbid}/front");

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::MusicBrainz(e.to_string()))?;

        if response.status().as_u16() == 404 {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(Error::MusicBrainz(format!(
                "Cover art error: {}",
                response.status()
            )));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| Error::MusicBrainz(e.to_string()))?;

        Ok(Some(bytes.to_vec()))
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Release {
    #[must_use]
    pub fn artist_name(&self) -> String {
        self.artist_credit.iter().fold(String::new(), |mut acc, ac| {
            let _ = write!(acc, "{}{}", ac.artist.name, ac.joinphrase);
            acc
        })
    }

    #[must_use]
    pub fn year(&self) -> Option<i32> {
        self.date
            .as_ref()
            .and_then(|d| d.split('-').next())
            .and_then(|y| y.parse().ok())
    }

    #[must_use]
    pub fn tracks(&self) -> Vec<&Track> {
        self.media.iter().flat_map(|m| &m.tracks).collect()
    }
}
