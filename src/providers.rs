//! Aggregation for the fixed set of built-in metadata providers.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::config::Config;
use crate::discogs::DiscogsProvider;
use crate::musicbrainz::MusicBrainzProvider;
use crate::provider::{
    MetadataProvider, ProviderEntityId, ReleaseCandidate, ReleaseQuery, SearchPage, TrackCandidate,
    TrackQuery,
};
use crate::{Error, Result};

pub struct ProviderSet {
    providers: Vec<ProviderEntry>,
    warnings: Mutex<Vec<String>>,
}

struct ProviderEntry {
    provider: Box<dyn MetadataProvider>,
    search_limit: u32,
}

impl ProviderSet {
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut providers = Vec::new();
        for name in &config.providers.enabled {
            match name.as_str() {
                "musicbrainz" => {
                    providers.push(ProviderEntry {
                        provider: Box::new(MusicBrainzProvider::new(&config.musicbrainz)?),
                        search_limit: config.musicbrainz.search_limit,
                    });
                }
                "discogs" => providers.push(ProviderEntry {
                    provider: Box::new(DiscogsProvider::new(
                        &config.discogs,
                        std::env::var("RSBTS_DISCOGS_TOKEN").map_err(|_error| {
                            Error::Config(
                                "Discogs is enabled but RSBTS_DISCOGS_TOKEN is not set".into(),
                            )
                        })?,
                    )?),
                    search_limit: config.discogs.search_limit,
                }),
                other => return Err(Error::Config(format!("unknown built-in provider: {other}"))),
            }
        }
        if providers.is_empty() {
            return Err(Error::Config(
                "at least one metadata provider must be enabled".into(),
            ));
        }
        Ok(Self {
            providers,
            warnings: Mutex::new(Vec::new()),
        })
    }

    fn record_warnings(&self, values: Vec<String>) {
        if let Ok(mut warnings) = self.warnings.lock() {
            warnings.extend(values);
        }
    }

    fn finish<T>(&self, values: Vec<T>, errors: Vec<String>, operation: &str) -> Result<Vec<T>> {
        if values.is_empty() && errors.len() == self.providers.len() {
            Err(Error::Provider(format!(
                "every enabled provider failed during {operation}: {}",
                errors.join("; ")
            )))
        } else {
            self.record_warnings(errors);
            Ok(values)
        }
    }
}

#[async_trait]
impl MetadataProvider for ProviderSet {
    fn name(&self) -> &'static str {
        "built-in"
    }

    async fn search_releases(&self, query: &ReleaseQuery, limit: u32) -> Result<SearchPage> {
        let mut values = Vec::new();
        let mut errors = Vec::new();
        let mut requested = 0_usize;
        let mut resolved = 0_usize;
        let mut complete = true;
        for entry in &self.providers {
            match entry
                .provider
                .search_releases(query, limit.min(entry.search_limit))
                .await
            {
                Ok(mut page) => {
                    requested = requested.saturating_add(page.requested);
                    resolved = resolved.saturating_add(page.resolved);
                    complete &= page.complete;
                    errors.extend(
                        page.errors
                            .drain(..)
                            .map(|error| format!("{}: {}", entry.provider.name(), error.detail)),
                    );
                    values.append(&mut page.candidates);
                }
                Err(error) => {
                    complete = false;
                    errors.push(format!("{}: {error}", entry.provider.name()));
                }
            }
        }
        let candidates = self.finish(values, errors.clone(), "release search")?;
        Ok(SearchPage {
            candidates,
            requested,
            resolved,
            complete: complete && errors.is_empty(),
            errors: errors
                .into_iter()
                .map(|detail| crate::provider::ProviderFailure {
                    external_id: None,
                    detail,
                    retriable: true,
                })
                .collect(),
        })
    }

    async fn search_tracks(&self, query: &TrackQuery, limit: u32) -> Result<Vec<TrackCandidate>> {
        let mut values = Vec::new();
        let mut errors = Vec::new();
        for entry in &self.providers {
            match entry
                .provider
                .search_tracks(query, limit.min(entry.search_limit))
                .await
            {
                Ok(mut candidates) => values.append(&mut candidates),
                Err(error) => errors.push(format!("{}: {error}", entry.provider.name())),
            }
        }
        self.finish(values, errors, "track search")
    }

    async fn lookup_release(&self, id: &ProviderEntityId) -> Result<ReleaseCandidate> {
        let provider_name = id.provider();
        let provider = self
            .providers
            .iter()
            .map(|entry| &entry.provider)
            .find(|provider| provider.name() == provider_name)
            .ok_or_else(|| Error::Provider(format!("provider is not enabled: {provider_name}")))?;
        provider.lookup_release(id).await
    }

    async fn lookup_track(&self, track_id: &str) -> Result<TrackCandidate> {
        let (provider_name, value) = split_qualified_id(track_id)?;
        let provider = self
            .providers
            .iter()
            .map(|entry| &entry.provider)
            .find(|provider| provider.name() == provider_name)
            .ok_or_else(|| Error::Provider(format!("provider is not enabled: {provider_name}")))?;
        provider.lookup_track(value).await
    }

    async fn fetch_cover_art(&self, _release_id: &str) -> Result<Option<Vec<u8>>> {
        Err(Error::Provider(
            "aggregate cover-art lookup requires a provider-qualified candidate".into(),
        ))
    }

    async fn fetch_cover_art_for(
        &self,
        provider_name: &str,
        release_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        let Some(provider) = self
            .providers
            .iter()
            .map(|entry| &entry.provider)
            .find(|provider| provider.name() == provider_name)
        else {
            return Ok(None);
        };
        provider.fetch_cover_art(release_id).await
    }

    fn take_warnings(&self) -> Vec<String> {
        self.warnings.lock().map_or_else(
            |_error| Vec::new(),
            |mut warnings| std::mem::take(&mut *warnings),
        )
    }
}

fn split_qualified_id(value: &str) -> Result<(&str, &str)> {
    let (provider, id) = value
        .split_once(':')
        .ok_or_else(|| Error::Provider("explicit IDs must use provider:value syntax".into()))?;
    if provider.is_empty() || id.is_empty() {
        Err(Error::Provider(
            "explicit IDs must use provider:value syntax".into(),
        ))
    } else {
        Ok((provider, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{EditionEvidence, ProviderEntityKind, ProviderFailure, ProviderTrack};

    struct MockProvider {
        name: &'static str,
        page: SearchPage,
    }

    #[async_trait]
    impl MetadataProvider for MockProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn search_releases(&self, _query: &ReleaseQuery, _limit: u32) -> Result<SearchPage> {
            Ok(self.page.clone())
        }

        async fn lookup_release(&self, id: &ProviderEntityId) -> Result<ReleaseCandidate> {
            if id.provider() != self.name {
                return Err(Error::Provider("lookup reached the wrong provider".into()));
            }
            Ok(release(self.name, id.value()))
        }

        async fn fetch_cover_art(&self, _release_id: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    fn release(provider: &str, external_id: &str) -> ReleaseCandidate {
        ReleaseCandidate {
            provider: provider.into(),
            external_id: external_id.into(),
            title: "Album".into(),
            artist: "Artist".into(),
            year: None,
            provider_score: 1.0,
            tracks: Vec::<ProviderTrack>::new(),
            edition: EditionEvidence::default(),
        }
    }

    fn provider_set(providers: Vec<MockProvider>) -> ProviderSet {
        ProviderSet {
            providers: providers
                .into_iter()
                .map(|provider| ProviderEntry {
                    provider: Box::new(provider) as Box<dyn MetadataProvider>,
                    search_limit: 5,
                })
                .collect(),
            warnings: Mutex::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn aggregate_search_discloses_partial_provider_results() -> Result<()> {
        let set = provider_set(vec![
            MockProvider {
                name: "musicbrainz",
                page: SearchPage::complete(vec![release("musicbrainz", "mb-release")]),
            },
            MockProvider {
                name: "discogs",
                page: SearchPage {
                    candidates: vec![release("discogs", "123")],
                    requested: 2,
                    resolved: 1,
                    errors: vec![ProviderFailure {
                        external_id: Some("456".into()),
                        detail: "lookup failed".into(),
                        retriable: true,
                    }],
                    complete: false,
                },
            },
        ]);

        let page = set
            .search_releases(
                &ReleaseQuery {
                    artist: "Artist".into(),
                    album: "Album".into(),
                    track_count: 1,
                },
                5,
            )
            .await?;

        assert_eq!(page.candidates.len(), 2);
        assert_eq!(page.requested, 3);
        assert_eq!(page.resolved, 2);
        assert!(!page.complete);
        assert_eq!(page.errors.len(), 1);
        assert!(page.errors[0].detail.contains("discogs"));
        assert_eq!(set.take_warnings().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn direct_release_ids_route_only_to_the_owning_provider() -> Result<()> {
        let set = provider_set(vec![
            MockProvider {
                name: "musicbrainz",
                page: SearchPage::complete(Vec::new()),
            },
            MockProvider {
                name: "discogs",
                page: SearchPage::complete(Vec::new()),
            },
        ]);
        let id = ProviderEntityId::new("discogs", ProviderEntityKind::Release, "123")?;

        let candidate = set.lookup_release(&id).await?;

        assert_eq!(candidate.provider, "discogs");
        assert_eq!(candidate.external_id, "123");
        Ok(())
    }
}
