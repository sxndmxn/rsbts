//! Provider-neutral release metadata contracts.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::artwork::ArtworkRole;
use crate::Result;

/// Provider entity families accepted at direct-lookup boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ProviderEntityKind {
    Release,
    ReleaseGroup,
    ReleaseTrack,
    Recording,
    Work,
    Artist,
}

/// A validated, provider- and entity-typed external identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEntityId {
    provider: String,
    kind: ProviderEntityKind,
    value: String,
}

impl ProviderEntityId {
    pub fn new(
        provider: impl Into<String>,
        kind: ProviderEntityKind,
        value: impl Into<String>,
    ) -> Result<Self> {
        let provider = provider.into();
        let value = value.into();
        if provider.trim().is_empty()
            || value.trim().is_empty()
            || provider.chars().any(char::is_control)
            || value.chars().any(char::is_control)
            || provider.len() > 128
            || value.len() > 512
        {
            return Err(crate::Error::Provider(
                "provider IDs must be non-empty, bounded, and control-free".into(),
            ));
        }
        Ok(Self {
            provider,
            kind,
            value,
        })
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderEntityKind {
        self.kind
    }

    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseQuery {
    pub artist: String,
    pub album: String,
    pub track_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderTrack {
    /// Recording identifier; never a release-track identifier.
    pub external_id: String,
    /// Release-track identifier when the provider models it separately.
    #[serde(default)]
    pub release_track_external_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub number: Option<u32>,
    #[serde(default)]
    pub printed_position: Option<String>,
    pub disc: Option<u32>,
    pub length_ms: Option<u64>,
    #[serde(default)]
    pub is_hidden: bool,
    #[serde(default)]
    pub is_data_track: bool,
    #[serde(default)]
    pub pregap_ms: Option<u64>,
}

/// Exact-edition facts kept distinct from text-search relevance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EditionEvidence {
    pub explicit_id: bool,
    pub release_group_external_id: Option<String>,
    pub disambiguation: Option<String>,
    pub country: Option<String>,
    pub date: Option<String>,
    pub status: Option<String>,
    pub packaging: Option<String>,
    pub barcode: Option<String>,
    pub labels_and_catalog_numbers: Vec<(String, Option<String>)>,
    pub media_formats: Vec<String>,
    pub disc_ids: Vec<String>,
    pub source_evidence: Vec<String>,
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
    #[serde(default)]
    pub edition: EditionEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFailure {
    pub external_id: Option<String>,
    pub detail: String,
    pub retriable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPage {
    pub candidates: Vec<ReleaseCandidate>,
    pub requested: usize,
    pub resolved: usize,
    pub errors: Vec<ProviderFailure>,
    pub complete: bool,
}

#[derive(Debug, Clone)]
pub struct ArtworkCandidate {
    bytes: Vec<u8>,
    role: ArtworkRole,
    source_reference: String,
    provider_release_id: Option<String>,
    exact_release: bool,
    rights: Option<String>,
}

impl ArtworkCandidate {
    pub fn new(
        bytes: Vec<u8>,
        role: ArtworkRole,
        source_reference: impl Into<String>,
        provider_release_id: Option<String>,
        exact_release: bool,
        rights: Option<String>,
    ) -> Result<Self> {
        let source_reference = source_reference.into();
        if source_reference.chars().any(char::is_control) || source_reference.len() > 4096 {
            return Err(crate::Error::Provider(
                "invalid artwork source reference".into(),
            ));
        }
        Ok(Self {
            bytes,
            role,
            source_reference,
            provider_release_id,
            exact_release,
            rights,
        })
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub const fn role(&self) -> &ArtworkRole {
        &self.role
    }

    #[must_use]
    pub fn source_reference(&self) -> &str {
        &self.source_reference
    }

    #[must_use]
    pub fn provider_release_id(&self) -> Option<&str> {
        self.provider_release_id.as_deref()
    }

    #[must_use]
    pub const fn exact_release(&self) -> bool {
        self.exact_release
    }

    #[must_use]
    pub fn rights(&self) -> Option<&str> {
        self.rights.as_deref()
    }
}

impl SearchPage {
    #[must_use]
    pub const fn complete(candidates: Vec<ReleaseCandidate>) -> Self {
        let resolved = candidates.len();
        Self {
            candidates,
            requested: resolved,
            resolved,
            errors: Vec::new(),
            complete: true,
        }
    }
}

#[async_trait]
pub trait MetadataProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn search_releases(&self, query: &ReleaseQuery, limit: u32) -> Result<SearchPage>;

    /// Resolve a known provider release ID without fuzzy text search.
    async fn lookup_release(&self, id: &ProviderEntityId) -> Result<ReleaseCandidate> {
        Err(crate::Error::Provider(format!(
            "{} does not support direct {:?} lookup",
            self.name(),
            id.kind()
        )))
    }

    async fn fetch_cover_art(&self, release_id: &str) -> Result<Option<Vec<u8>>>;

    /// Fetch typed artwork and provenance. Implementations may return several roles.
    async fn fetch_artwork(&self, release_id: &str) -> Result<Vec<ArtworkCandidate>> {
        self.fetch_cover_art(release_id).await?.map_or_else(
            || Ok(Vec::new()),
            |bytes| {
                ArtworkCandidate::new(
                    bytes,
                    ArtworkRole::Front,
                    String::new(),
                    Some(release_id.to_owned()),
                    true,
                    None,
                )
                .map(|artwork| vec![artwork])
            },
        )
    }
}
