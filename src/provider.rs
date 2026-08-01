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

#[async_trait]
pub trait MetadataProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn search_releases(
        &self,
        query: &ReleaseQuery,
        limit: u32,
    ) -> Result<Vec<ReleaseCandidate>>;

    async fn fetch_cover_art(&self, release_id: &str) -> Result<Option<Vec<u8>>>;
}
