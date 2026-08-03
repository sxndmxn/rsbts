//! Aggregation for the fixed set of built-in metadata providers.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::config::Config;
use crate::discogs::DiscogsProvider;
use crate::musicbrainz::MusicBrainzProvider;
use crate::provider::{
    MetadataProvider, ReleaseCandidate, ReleaseQuery, TrackCandidate, TrackQuery,
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

    async fn search_releases(
        &self,
        query: &ReleaseQuery,
        limit: u32,
    ) -> Result<Vec<ReleaseCandidate>> {
        let mut values = Vec::new();
        let mut errors = Vec::new();
        for entry in &self.providers {
            match entry
                .provider
                .search_releases(query, limit.min(entry.search_limit))
                .await
            {
                Ok(mut candidates) => values.append(&mut candidates),
                Err(error) => errors.push(format!("{}: {error}", entry.provider.name())),
            }
        }
        self.finish(values, errors, "release search")
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

    async fn lookup_release(&self, release_id: &str) -> Result<ReleaseCandidate> {
        let (provider_name, value) = split_qualified_id(release_id)?;
        let provider = self
            .providers
            .iter()
            .map(|entry| &entry.provider)
            .find(|provider| provider.name() == provider_name)
            .ok_or_else(|| Error::Provider(format!("provider is not enabled: {provider_name}")))?;
        provider.lookup_release(value).await
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
