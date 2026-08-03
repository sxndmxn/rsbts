//! Provider-neutral release metadata contracts.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseQuery {
    pub artist: String,
    pub album: String,
    pub track_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackQuery {
    pub artist: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderTrack {
    pub external_id: String,
    pub title: String,
    pub artist: String,
    pub number: Option<u32>,
    pub disc: Option<u32>,
    pub length_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseCandidate {
    pub provider: String,
    pub external_id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<i32>,
    pub provider_score: f64,
    pub tracks: Vec<ProviderTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackCandidate {
    pub provider: String,
    pub external_id: String,
    pub title: String,
    pub artist: String,
    pub length_ms: Option<u64>,
    pub provider_score: f64,
    pub release_external_id: Option<String>,
}

#[async_trait]
pub trait MetadataProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn search_releases(
        &self,
        query: &ReleaseQuery,
        limit: u32,
    ) -> Result<Vec<ReleaseCandidate>>;

    async fn search_tracks(&self, _query: &TrackQuery, _limit: u32) -> Result<Vec<TrackCandidate>> {
        Ok(Vec::new())
    }

    async fn lookup_release(&self, _release_id: &str) -> Result<ReleaseCandidate> {
        Err(crate::Error::Provider(format!(
            "{} does not support release lookup",
            self.name()
        )))
    }

    async fn lookup_track(&self, _track_id: &str) -> Result<TrackCandidate> {
        Err(crate::Error::Provider(format!(
            "{} does not support track lookup",
            self.name()
        )))
    }

    async fn fetch_cover_art(&self, release_id: &str) -> Result<Option<Vec<u8>>>;

    async fn fetch_cover_art_for(
        &self,
        provider: &str,
        release_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        if provider == self.name() {
            self.fetch_cover_art(release_id).await
        } else {
            Ok(None)
        }
    }

    /// Return and clear non-fatal provider warnings accumulated by an aggregate provider.
    fn take_warnings(&self) -> Vec<String> {
        Vec::new()
    }
}
