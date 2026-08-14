//! Source-scoped provider adapters and licensing gates.

use std::collections::BTreeMap;
use std::io::BufRead;

use serde::{Deserialize, Serialize};

use crate::catalog::{Confidence, DataLicense, EntityId, EntityKind, MetadataClaim, ValueState};
use crate::db::Library;
use crate::{Error, Result};

const MAX_DUMP_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// One lossless normalized record produced from a licensed Discogs CC0 dump.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscogsReleaseRecord {
    id: u64,
    formats: Vec<DiscogsFormat>,
    labels: Vec<DiscogsLabel>,
    identifiers: Vec<DiscogsIdentifier>,
    credits: Vec<DiscogsCredit>,
    genres: Vec<String>,
    styles: Vec<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscogsFormat {
    name: String,
    quantity: Option<u32>,
    descriptions: Vec<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscogsLabel {
    name: String,
    catalog_number: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscogsIdentifier {
    kind: String,
    value: String,
    description: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscogsCredit {
    artist_id: Option<u64>,
    name: String,
    role: String,
    tracks: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

impl DiscogsReleaseRecord {
    fn validate(&self) -> Result<()> {
        if self.id == 0 {
            return Err(Error::Provider(
                "Discogs release ID must be positive".into(),
            ));
        }
        for (label, values) in [("genres", &self.genres), ("styles", &self.styles)] {
            validate_values(values, label)?;
        }
        for format in &self.formats {
            validate_text(&format.name, "Discogs format")?;
            validate_values(&format.descriptions, "Discogs format descriptions")?;
        }
        for label in &self.labels {
            validate_text(&label.name, "Discogs label")?;
            if let Some(value) = &label.catalog_number {
                validate_text(value, "Discogs catalog number")?;
            }
        }
        for identifier in &self.identifiers {
            validate_text(&identifier.kind, "Discogs identifier type")?;
            validate_text(&identifier.value, "Discogs identifier value")?;
        }
        for credit in &self.credits {
            validate_text(&credit.name, "Discogs credit name")?;
            validate_text(&credit.role, "Discogs credit role")?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub fn genres(&self) -> &[String] {
        &self.genres
    }

    #[must_use]
    pub fn styles(&self) -> &[String] {
        &self.styles
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscogsImportReport {
    parsed: u64,
    resolved: u64,
    unresolved: u64,
}

impl DiscogsImportReport {
    #[must_use]
    pub const fn counts(self) -> (u64, u64, u64) {
        (self.parsed, self.resolved, self.unresolved)
    }
}

/// Evidence scope is part of the type so acoustic results cannot assert an edition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EvidenceScope {
    Recording,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AcousticFingerprint(String);

impl AcousticFingerprint {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_text(&value, "Chromaprint fingerprint")?;
        if value.len() > 64 * 1024 {
            return Err(Error::Provider(
                "Chromaprint fingerprint exceeds 64 KiB".into(),
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for AcousticFingerprint {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingCandidate {
    provider: String,
    recording_id: String,
    score: Confidence,
    scope: EvidenceScope,
}

impl RecordingCandidate {
    pub fn acoustid(recording_id: impl Into<String>, score: f64) -> Result<Self> {
        let recording_id = recording_id.into();
        validate_text(&recording_id, "AcoustID recording ID")?;
        Ok(Self {
            provider: "acoustid".into(),
            recording_id,
            score: Confidence::new(score)?,
            scope: EvidenceScope::Recording,
        })
    }

    #[must_use]
    pub const fn scope(&self) -> EvidenceScope {
        self.scope
    }
}

/// Lawful RYM/Sonemic entry paths. No crawler or scraper mode exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CurationSource {
    OfficialApi,
    Partnership,
    UserProvidedExport,
    ManualEntry,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CurationValue {
    Rating(CommunityRating),
    Genres(Vec<String>),
    Descriptors(Vec<String>),
    PersonalList(String),
}

/// A finite community rating in the inclusive source scale from zero to five.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CommunityRating(f64);

impl CommunityRating {
    pub fn new(value: f64) -> Result<Self> {
        if value.is_finite() && (0.0..=5.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(Error::Provider(
                "community rating must be finite and between 0 and 5".into(),
            ))
        }
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for CommunityRating {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl Library {
    /// Stream normalized JSONL records originating from the Discogs CC0 dump.
    /// Exact raw lines are retained as licensed snapshots; unresolved records
    /// remain cached rather than being forced onto the wrong release.
    pub fn ingest_discogs_cc0_jsonl<R, F>(
        &self,
        reader: R,
        mut resolve_release: F,
    ) -> Result<DiscogsImportReport>
    where
        R: BufRead,
        F: FnMut(u64) -> Result<Option<EntityId>>,
    {
        let mut report = DiscogsImportReport::default();
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() || line.len() > MAX_DUMP_RECORD_BYTES {
                return Err(Error::Provider(
                    "Discogs dump record is empty or exceeds 16 MiB".into(),
                ));
            }
            let record: DiscogsReleaseRecord = serde_json::from_str(&line)?;
            record.validate()?;
            report.parsed = report.parsed.saturating_add(1);
            let entity = resolve_release(record.id)?;
            self.store_provider_snapshot(
                "discogs",
                &format!("cc0-dump-release:{}", record.id),
                entity.as_ref().map(|id| (EntityKind::Release, id)),
                &DataLicense::Cc0,
                line.as_bytes(),
                true,
            )?;
            let Some(entity) = entity else {
                report.unresolved = report.unresolved.saturating_add(1);
                continue;
            };
            for (field, value) in [
                ("discogs.formats", serde_json::to_value(&record.formats)?),
                ("discogs.labels", serde_json::to_value(&record.labels)?),
                (
                    "discogs.identifiers",
                    serde_json::to_value(&record.identifiers)?,
                ),
                ("discogs.credits", serde_json::to_value(&record.credits)?),
                ("discogs.genres", serde_json::to_value(&record.genres)?),
                ("discogs.styles", serde_json::to_value(&record.styles)?),
                ("discogs.extra", serde_json::to_value(&record.extra)?),
            ] {
                self.append_metadata_claim(&MetadataClaim::new(
                    EntityKind::Release,
                    entity.clone(),
                    field,
                    ValueState::Known(value),
                    "provider-dump",
                    Some("discogs".into()),
                    Some(format!("release:{}", record.id)),
                    Confidence::new(1.0)?,
                    DataLicense::Cc0,
                    false,
                )?)?;
            }
            report.resolved = report.resolved.saturating_add(1);
        }
        Ok(report)
    }

    /// Store community curation strictly as curation claims, never identity facts.
    pub fn append_rym_curation(
        &self,
        entity_kind: EntityKind,
        entity_id: &EntityId,
        source: CurationSource,
        value: &CurationValue,
        source_reference: Option<String>,
    ) -> Result<EntityId> {
        let (field, json) = match &value {
            CurationValue::Rating(_rating) => ("curation.rym.rating", serde_json::to_value(value)?),
            CurationValue::Genres(values) => {
                validate_values(values, "RYM genres")?;
                ("curation.rym.genres", serde_json::to_value(value)?)
            }
            CurationValue::Descriptors(values) => {
                validate_values(values, "RYM descriptors")?;
                ("curation.rym.descriptors", serde_json::to_value(value)?)
            }
            CurationValue::PersonalList(name) => {
                validate_text(name, "RYM personal list")?;
                ("curation.rym.personal-list", serde_json::to_value(value)?)
            }
        };
        let (source_kind, license) = match source {
            CurationSource::OfficialApi => (
                "official-api",
                DataLicense::SourceSpecific("rym-official-api".into()),
            ),
            CurationSource::Partnership => (
                "partnership",
                DataLicense::SourceSpecific("rym-partnership".into()),
            ),
            CurationSource::UserProvidedExport => ("user-export", DataLicense::UserOwned),
            CurationSource::ManualEntry => ("manual", DataLicense::UserOwned),
        };
        let claim = MetadataClaim::new(
            entity_kind,
            entity_id.clone(),
            field,
            ValueState::Known(json),
            source_kind,
            Some("rateyourmusic".into()),
            source_reference,
            Confidence::new(1.0)?,
            license,
            false,
        )?;
        let id = claim.id().clone();
        self.append_metadata_claim(&claim)?;
        Ok(id)
    }
}

fn validate_values(values: &[String], label: &str) -> Result<()> {
    if values.len() > 100_000 {
        return Err(Error::Provider(format!(
            "{label} exceeds value-count bound"
        )));
    }
    for value in values {
        validate_text(value, label)?;
    }
    Ok(())
}

fn validate_text(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        Err(Error::Provider(format!("invalid {label}")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn discogs_dump_preserves_multivalue_fields_and_separates_styles() -> Result<()> {
        let library = Library::open_in_memory()?;
        let entity = EntityId::new();
        let line = serde_json::json!({
            "id": 42,
            "formats": [{"name": "Vinyl", "quantity": 2, "descriptions": ["LP", "Album"]}],
            "labels": [
                {"name": "Label A", "catalog_number": "A-1"},
                {"name": "Label B", "catalog_number": "B-2"}
            ],
            "identifiers": [{"kind": "Matrix / Runout", "value": "ABC", "description": "Side A"}],
            "credits": [{"artist_id": 7, "name": "Person", "role": "Engineer", "tracks": "A1"}],
            "genres": ["Electronic"],
            "styles": ["Ambient", "Drone"],
            "future_field": {"retained": true}
        })
        .to_string();
        let report = library.ingest_discogs_cc0_jsonl(Cursor::new(format!("{line}\n")), |_id| {
            Ok(Some(entity.clone()))
        })?;
        assert_eq!(report.counts(), (1, 1, 0));
        let genre: String = library.conn.query_row(
            "SELECT value_json FROM metadata_claims WHERE field = 'discogs.genres'",
            [],
            |row| row.get(0),
        )?;
        let styles: String = library.conn.query_row(
            "SELECT value_json FROM metadata_claims WHERE field = 'discogs.styles'",
            [],
            |row| row.get(0),
        )?;
        assert_ne!(genre, styles);
        assert!(styles.contains("Drone"));
        Ok(())
    }

    #[test]
    fn acoustic_and_rym_evidence_cannot_claim_release_identity() -> Result<()> {
        let candidate = RecordingCandidate::acoustid("recording-id", 0.95)?;
        assert_eq!(candidate.scope(), EvidenceScope::Recording);
        let library = Library::open_in_memory()?;
        let entity = EntityId::new();
        library.append_rym_curation(
            EntityKind::Release,
            &entity,
            CurationSource::UserProvidedExport,
            &CurationValue::Genres(vec!["Ambient".into()]),
            Some("my-export.csv".into()),
        )?;
        let field: String = library.conn.query_row(
            "SELECT field FROM metadata_claims WHERE source_provider = 'rateyourmusic'",
            [],
            |row| row.get(0),
        )?;
        assert!(field.starts_with("curation.rym."));
        Ok(())
    }
}
