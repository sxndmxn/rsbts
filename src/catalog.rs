//! Normalized catalog identity, provenance, and resolution policy.
//!
//! Provider data is append-only evidence.  It does not become canonical data
//! until a caller explicitly previews and applies a versioned resolution.

use std::fmt;

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::db::Library;
use crate::operations::{append_plan_event, PlanId, PlanKind, PlanState};
use crate::{Error, Result};

const RESOLUTION_POLICY_VERSION: u32 = 1;

/// A stable internal catalog entity identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct EntityId(String);

impl EntityId {
    /// Generate a new random internal identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Validate an identifier read at an API boundary.
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        Uuid::parse_str(&value)
            .map_err(|error| Error::Catalog(format!("invalid entity ID: {error}")))?;
        Ok(Self(value.to_ascii_lowercase()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for EntityId {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de> Deserialize<'de> for EntityId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A finite confidence in the inclusive range zero through one.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Confidence(f64);

impl Confidence {
    pub fn new(value: f64) -> Result<Self> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(Error::Catalog(
                "confidence must be finite and between 0 and 1".into(),
            ))
        }
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Confidence {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Calendar date preserving its original precision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PartialDate(String);

impl PartialDate {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let valid = match value.len() {
            4 => value.parse::<u16>().is_ok_and(|year| year > 0),
            7 => NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d").is_ok(),
            10 => NaiveDate::parse_from_str(&value, "%Y-%m-%d").is_ok(),
            _ => false,
        };
        if valid {
            Ok(Self(value))
        } else {
            Err(Error::Catalog(
                "partial date must be YYYY, YYYY-MM, or YYYY-MM-DD".into(),
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PartialDate {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// The semantic state of a sourced field. Placeholder strings are forbidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ValueState<T> {
    Known(T),
    Unknown,
    Absent,
    NotApplicable,
    Conflict,
}

impl<T: Serialize> ValueState<T> {
    fn storage_parts(&self) -> Result<(&'static str, Option<String>)> {
        match self {
            Self::Known(value) => Ok(("known", Some(serde_json::to_string(value)?))),
            Self::Unknown => Ok(("unknown", None)),
            Self::Absent => Ok(("absent", None)),
            Self::NotApplicable => Ok(("not-applicable", None)),
            Self::Conflict => Ok(("conflict", None)),
        }
    }
}

/// Normalized entity families. File assets remain separate from these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EntityKind {
    ReleaseGroup,
    Release,
    Medium,
    ReleaseTrack,
    Recording,
    Work,
    Artist,
    Label,
    Asset,
}

impl EntityKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReleaseGroup => "release-group",
            Self::Release => "release",
            Self::Medium => "medium",
            Self::ReleaseTrack => "release-track",
            Self::Recording => "recording",
            Self::Work => "work",
            Self::Artist => "artist",
            Self::Label => "label",
            Self::Asset => "asset",
        }
    }
}

/// Licensing classification retained with every evidence record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum DataLicense {
    Cc0,
    CcBySa,
    UserOwned,
    SourceSpecific(String),
}

impl DataLicense {
    fn as_storage(&self) -> String {
        match self {
            Self::Cc0 => "CC0-1.0".into(),
            Self::CcBySa => "CC-BY-SA".into(),
            Self::UserOwned => "user-owned".into(),
            Self::SourceSpecific(value) => format!("source-specific:{value}"),
        }
    }
}

/// Immutable evidence for one entity field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataClaim {
    id: EntityId,
    entity_kind: EntityKind,
    entity_id: EntityId,
    field: String,
    value: ValueState<serde_json::Value>,
    source_kind: String,
    source_provider: Option<String>,
    source_reference: Option<String>,
    retrieved_at: DateTime<Utc>,
    confidence: Confidence,
    data_license: DataLicense,
    locked: bool,
}

impl MetadataClaim {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        entity_kind: EntityKind,
        entity_id: EntityId,
        field: impl Into<String>,
        value: ValueState<serde_json::Value>,
        source_kind: impl Into<String>,
        source_provider: Option<String>,
        source_reference: Option<String>,
        confidence: Confidence,
        data_license: DataLicense,
        locked: bool,
    ) -> Result<Self> {
        let field = checked_label(field.into(), "claim field")?;
        let source_kind = checked_label(source_kind.into(), "claim source kind")?;
        if locked && source_kind != "manual" {
            return Err(Error::Catalog(
                "only manual claims may be locked against refresh".into(),
            ));
        }
        Ok(Self {
            id: EntityId::new(),
            entity_kind,
            entity_id,
            field,
            value,
            source_kind,
            source_provider,
            source_reference,
            retrieved_at: Utc::now(),
            confidence,
            data_license,
            locked,
        })
    }

    #[must_use]
    pub const fn id(&self) -> &EntityId {
        &self.id
    }

    #[must_use]
    pub const fn value(&self) -> &ValueState<serde_json::Value> {
        &self.value
    }
}

/// A proposed canonical-field change. Applying it is always explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDiff {
    entity_kind: EntityKind,
    entity_id: EntityId,
    field: String,
    before: Option<ValueState<serde_json::Value>>,
    after: ValueState<serde_json::Value>,
    winning_claim_id: EntityId,
    policy_version: u32,
}

impl FieldDiff {
    #[must_use]
    pub const fn before(&self) -> Option<&ValueState<serde_json::Value>> {
        self.before.as_ref()
    }

    #[must_use]
    pub const fn after(&self) -> &ValueState<serde_json::Value> {
        &self.after
    }

    #[must_use]
    pub const fn entity_kind(&self) -> EntityKind {
        self.entity_kind
    }

    #[must_use]
    pub const fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    #[must_use]
    pub fn field(&self) -> &str {
        &self.field
    }
}

/// Reviewable field-level provider refresh. Applying it changes canonical
/// catalog values only; media tags require a separate tag-projection plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRefreshPlan {
    id: PlanId,
    entity_kind: EntityKind,
    entity_id: EntityId,
    diffs: Vec<FieldDiff>,
    claim_set_sha256: String,
}

impl ProviderRefreshPlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    #[must_use]
    pub fn diffs(&self) -> &[FieldDiff] {
        &self.diffs
    }
}

/// Persisted raw provider response identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotReceipt {
    id: EntityId,
    sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum IdentityLevel {
    Recording,
    ReleaseGroup,
    Release,
}

impl IdentityLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::ReleaseGroup => "release-group",
            Self::Release => "release",
        }
    }
}

impl SnapshotReceipt {
    #[must_use]
    pub const fn id(&self) -> &EntityId {
        &self.id
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

impl Library {
    /// Store the exact bytes used for provider resolution with their license.
    #[allow(clippy::too_many_arguments)]
    pub fn store_provider_snapshot(
        &self,
        provider: &str,
        request_key: &str,
        entity: Option<(EntityKind, &EntityId)>,
        license: &DataLicense,
        payload: &[u8],
        complete: bool,
    ) -> Result<SnapshotReceipt> {
        checked_label(provider.to_owned(), "provider")?;
        checked_label(request_key.to_owned(), "request key")?;
        let sha256 = format!("{:x}", Sha256::digest(payload));
        let existing = self
            .conn
            .query_row(
                "SELECT id FROM provider_snapshots
                 WHERE provider = ?1 AND request_key = ?2 AND content_sha256 = ?3",
                params![provider, request_key, sha256],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let id = existing.map_or_else(EntityId::new, EntityId);
        let (entity_type, entity_id) = entity.map_or((None, None), |(kind, id)| {
            (Some(kind.as_str()), Some(id.as_str()))
        });
        self.conn.execute(
            "INSERT OR IGNORE INTO provider_snapshots
             (id, provider, request_key, entity_type, entity_id, retrieved_at,
              data_license, content_sha256, compression, payload, complete)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'none', ?9, ?10)",
            params![
                id.as_str(),
                provider,
                request_key,
                entity_type,
                entity_id,
                Utc::now().to_rfc3339(),
                license.as_storage(),
                sha256,
                payload,
                complete
            ],
        )?;
        Ok(SnapshotReceipt { id, sha256 })
    }

    /// Append evidence. Existing claims are never updated in place.
    pub fn append_metadata_claim(&self, claim: &MetadataClaim) -> Result<()> {
        let (value_state, value_json) = claim.value.storage_parts()?;
        self.conn.execute(
            "INSERT INTO metadata_claims
             (id, entity_type, entity_id, field, value_state, value_json,
              source_kind, source_provider, source_reference, retrieved_at,
              confidence, data_license, locked)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                claim.id.as_str(),
                claim.entity_kind.as_str(),
                claim.entity_id.as_str(),
                claim.field,
                value_state,
                value_json,
                claim.source_kind,
                claim.source_provider,
                claim.source_reference,
                claim.retrieved_at.to_rfc3339(),
                claim.confidence.get(),
                claim.data_license.as_storage(),
                claim.locked
            ],
        )?;
        Ok(())
    }

    /// Select the winning active claim under policy v1 without changing canonical data.
    pub fn preview_claim_resolution(
        &self,
        entity_kind: EntityKind,
        entity_id: &EntityId,
        field: &str,
    ) -> Result<Option<FieldDiff>> {
        checked_label(field.to_owned(), "claim field")?;
        let winner = self
            .conn
            .query_row(
                "SELECT id, value_state, value_json FROM metadata_claims
                 WHERE entity_type = ?1 AND entity_id = ?2 AND field = ?3
                   AND superseded_by IS NULL
                 ORDER BY locked DESC,
                          CASE source_kind WHEN 'manual' THEN 0 ELSE 1 END,
                          confidence DESC, retrieved_at DESC, id DESC
                 LIMIT 1",
                params![entity_kind.as_str(), entity_id.as_str(), field],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((claim_id, state, value)) = winner else {
            return Ok(None);
        };
        let after = decode_value_state(&state, value.as_deref())?;
        let before = self
            .conn
            .query_row(
                "SELECT value_state, value_json FROM canonical_values
                 WHERE entity_type = ?1 AND entity_id = ?2 AND field = ?3",
                params![entity_kind.as_str(), entity_id.as_str(), field],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?
            .map(|(state, value)| decode_value_state(&state, value.as_deref()))
            .transpose()?;
        Ok(Some(FieldDiff {
            entity_kind,
            entity_id: entity_id.clone(),
            field: field.to_owned(),
            before,
            after,
            winning_claim_id: EntityId::parse(claim_id)?,
            policy_version: RESOLUTION_POLICY_VERSION,
        }))
    }

    /// Apply a previously reviewed field-level diff.
    pub fn apply_claim_resolution(&self, diff: &FieldDiff) -> Result<()> {
        if diff.policy_version != RESOLUTION_POLICY_VERSION {
            return Err(Error::Catalog("resolution policy version changed".into()));
        }
        let current =
            self.preview_claim_resolution(diff.entity_kind, &diff.entity_id, &diff.field)?;
        if current.as_ref() != Some(diff) {
            return Err(Error::Catalog(
                "claim set changed after preview; review the field again".into(),
            ));
        }
        let (state, value) = diff.after.storage_parts()?;
        self.conn.execute(
            "INSERT INTO canonical_values
             (entity_type, entity_id, field, value_state, value_json,
              winning_claim_id, policy_version, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(entity_type, entity_id, field) DO UPDATE SET
               value_state = excluded.value_state,
               value_json = excluded.value_json,
               winning_claim_id = excluded.winning_claim_id,
               policy_version = excluded.policy_version,
               resolved_at = excluded.resolved_at",
            params![
                diff.entity_kind.as_str(),
                diff.entity_id.as_str(),
                diff.field,
                state,
                value,
                diff.winning_claim_id.as_str(),
                diff.policy_version,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Build and persist a field-level diff across every claimed field for one entity.
    pub fn plan_provider_refresh(
        &self,
        entity_kind: EntityKind,
        entity_id: &EntityId,
    ) -> Result<ProviderRefreshPlan> {
        self.build_provider_refresh(entity_kind, entity_id, true)
    }

    /// Build the identical diff without writing a durable-plan row.
    pub fn preview_provider_refresh(
        &self,
        entity_kind: EntityKind,
        entity_id: &EntityId,
    ) -> Result<ProviderRefreshPlan> {
        self.build_provider_refresh(entity_kind, entity_id, false)
    }

    fn build_provider_refresh(
        &self,
        entity_kind: EntityKind,
        entity_id: &EntityId,
        persist: bool,
    ) -> Result<ProviderRefreshPlan> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT field FROM metadata_claims
             WHERE entity_type = ?1 AND entity_id = ?2 AND superseded_by IS NULL
             ORDER BY field LIMIT 10001",
        )?;
        let fields = statement
            .query_map(params![entity_kind.as_str(), entity_id.as_str()], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if fields.len() > 10_000 {
            return Err(Error::Catalog(
                "provider refresh exceeds the 10,000-field safety bound".into(),
            ));
        }
        let mut diffs = Vec::new();
        for field in fields {
            if let Some(diff) = self.preview_claim_resolution(entity_kind, entity_id, &field)? {
                if diff.before.as_ref() != Some(&diff.after) {
                    diffs.push(diff);
                }
            }
        }
        let request = serde_json::json!({
            "entity_kind": entity_kind,
            "entity_id": entity_id,
        });
        let claim_set_sha256 = self.active_claim_set_digest(entity_kind, entity_id)?;
        let preview = serde_json::json!({
            "policy_version": RESOLUTION_POLICY_VERSION,
            "diffs": diffs,
            "claim_set_sha256": claim_set_sha256,
            "media_tags_unchanged": true,
        });
        let id = if persist {
            self.create_durable_plan(
                PlanKind::ProviderRefresh,
                &request,
                &preview,
                Some(diffs.len() as u64),
            )?
        } else {
            PlanId::new()
        };
        Ok(ProviderRefreshPlan {
            id,
            entity_kind,
            entity_id: entity_id.clone(),
            diffs,
            claim_set_sha256,
        })
    }

    pub fn approve_provider_refresh(&self, plan: &ProviderRefreshPlan) -> Result<()> {
        self.approve_durable_plan(plan.id())
    }

    /// Apply only the exact reviewed diffs. A changed claim set invalidates the
    /// entire plan before any canonical value is written.
    pub fn execute_provider_refresh(&self, plan: &ProviderRefreshPlan) -> Result<()> {
        if self.durable_plan(plan.id())?.state() != PlanState::Approved {
            return Err(Error::Catalog(
                "provider refresh must be approved before execution".into(),
            ));
        }
        self.start_durable_plan(plan.id())?;
        if self.active_claim_set_digest(plan.entity_kind, &plan.entity_id)? != plan.claim_set_sha256
        {
            let detail = "claim set changed after provider-refresh preview";
            let _ = self.finish_durable_plan(plan.id(), PlanState::Failed, Some(detail));
            return Err(Error::Catalog(detail.into()));
        }
        for diff in &plan.diffs {
            let current =
                self.preview_claim_resolution(plan.entity_kind, &plan.entity_id, diff.field())?;
            if current.as_ref() != Some(diff) {
                let detail = format!("claim set changed for field {:?}", diff.field());
                let _ = self.finish_durable_plan(plan.id(), PlanState::Failed, Some(&detail));
                return Err(Error::Catalog(detail));
            }
        }
        let now = Utc::now().to_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        for diff in &plan.diffs {
            let (state, value) = diff.after.storage_parts()?;
            transaction.execute(
                "INSERT INTO canonical_values
                 (entity_type, entity_id, field, value_state, value_json,
                  winning_claim_id, policy_version, resolved_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(entity_type, entity_id, field) DO UPDATE SET
                   value_state = excluded.value_state,
                   value_json = excluded.value_json,
                   winning_claim_id = excluded.winning_claim_id,
                   policy_version = excluded.policy_version,
                   resolved_at = excluded.resolved_at",
                params![
                    diff.entity_kind.as_str(),
                    diff.entity_id.as_str(),
                    diff.field,
                    state,
                    value,
                    diff.winning_claim_id.as_str(),
                    diff.policy_version,
                    now,
                ],
            )?;
        }
        transaction.execute(
            "UPDATE durable_plans SET state = 'complete',
                    progress_current = progress_total, updated_at = ?2, completed_at = ?2
             WHERE id = ?1 AND state = 'running'",
            params![plan.id().as_str(), now],
        )?;
        append_plan_event(
            &transaction,
            plan.id(),
            "complete",
            &serde_json::json!({"applied_fields": plan.diffs.len()}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn active_claim_set_digest(
        &self,
        entity_kind: EntityKind,
        entity_id: &EntityId,
    ) -> Result<String> {
        let mut statement = self.conn.prepare(
            "SELECT id, field FROM metadata_claims
             WHERE entity_type = ?1 AND entity_id = ?2 AND superseded_by IS NULL
             ORDER BY field, id",
        )?;
        let rows = statement
            .query_map(params![entity_kind.as_str(), entity_id.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
        let mut hasher = Sha256::new();
        for row in rows {
            let (id, field) = row?;
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
            hasher.update((id.len() as u64).to_le_bytes());
            hasher.update(id.as_bytes());
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    /// Supersede a claim without destroying the earlier evidence.
    pub fn supersede_claim(&self, old: &EntityId, replacement: &EntityId) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE metadata_claims SET superseded_by = ?2
             WHERE id = ?1 AND superseded_by IS NULL",
            params![old.as_str(), replacement.as_str()],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(Error::Catalog(
                "claim does not exist or was already superseded".into(),
            ))
        }
    }

    /// Save a reviewable manual identity decision. Replacing a live lock is explicit.
    pub fn save_manual_match_lock(
        &self,
        asset_id: &EntityId,
        level: IdentityLevel,
        entity_id: &EntityId,
        evidence: &serde_json::Value,
    ) -> Result<()> {
        let evidence = serde_json::to_string(evidence)?;
        let changed = self.conn.execute(
            "INSERT INTO manual_match_locks
             (file_asset_id, identity_level, entity_id, evidence_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(file_asset_id, identity_level) DO UPDATE SET
               entity_id = excluded.entity_id,
               evidence_json = excluded.evidence_json,
               created_at = excluded.created_at,
               revoked_at = NULL
             WHERE manual_match_locks.revoked_at IS NOT NULL",
            params![
                asset_id.as_str(),
                level.as_str(),
                entity_id.as_str(),
                evidence,
                Utc::now().to_rfc3339()
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(Error::Catalog(
                "an active manual lock exists; revoke it before replacing the decision".into(),
            ))
        }
    }

    pub fn revoke_manual_match_lock(
        &self,
        asset_id: &EntityId,
        level: IdentityLevel,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE manual_match_locks SET revoked_at = ?3
             WHERE file_asset_id = ?1 AND identity_level = ?2 AND revoked_at IS NULL",
            params![asset_id.as_str(), level.as_str(), Utc::now().to_rfc3339()],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(Error::Catalog("manual match lock is not active".into()))
        }
    }
}

fn checked_label(value: String, description: &str) -> Result<String> {
    if value.trim().is_empty() || value.chars().any(char::is_control) || value.len() > 512 {
        Err(Error::Catalog(format!("invalid {description}")))
    } else {
        Ok(value)
    }
}

fn decode_value_state(state: &str, value: Option<&str>) -> Result<ValueState<serde_json::Value>> {
    match state {
        "known" => value
            .ok_or_else(|| Error::Catalog("known claim has no value".into()))
            .and_then(|value| {
                serde_json::from_str(value)
                    .map(ValueState::Known)
                    .map_err(Error::from)
            }),
        "unknown" => Ok(ValueState::Unknown),
        "absent" => Ok(ValueState::Absent),
        "not-applicable" => Ok(ValueState::NotApplicable),
        "conflict" => Ok(ValueState::Conflict),
        other => Err(Error::Catalog(format!("unknown value state: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn invariant_types_reject_invalid_values() -> Result<()> {
        assert!(Confidence::new(f64::NAN).is_err());
        assert!(Confidence::new(1.1).is_err());
        assert!(PartialDate::parse("2024-02-30").is_err());
        assert_eq!(PartialDate::parse("2024-02")?.as_str(), "2024-02");
        assert!(EntityId::parse("not-a-uuid").is_err());
        Ok(())
    }

    #[test]
    fn claims_require_review_before_canonical_materialization() -> Result<()> {
        let library = Library::open_in_memory()?;
        let entity = EntityId::new();
        let provider_claim = MetadataClaim::new(
            EntityKind::Release,
            entity.clone(),
            "title",
            ValueState::Known(json!("Provider title")),
            "provider",
            Some("musicbrainz".into()),
            Some("snapshot-id".into()),
            Confidence::new(0.99)?,
            DataLicense::Cc0,
            false,
        )?;
        library.append_metadata_claim(&provider_claim)?;
        let diff = library
            .preview_claim_resolution(EntityKind::Release, &entity, "title")?
            .ok_or_else(|| Error::Catalog("missing diff".into()))?;
        assert!(diff.before().is_none());
        assert_eq!(diff.after(), &ValueState::Known(json!("Provider title")));

        let canonical_count: u32 =
            library
                .conn
                .query_row("SELECT COUNT(*) FROM canonical_values", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(canonical_count, 0);
        library.apply_claim_resolution(&diff)?;
        let canonical_count: u32 =
            library
                .conn
                .query_row("SELECT COUNT(*) FROM canonical_values", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(canonical_count, 1);

        let manual = MetadataClaim::new(
            EntityKind::Release,
            entity.clone(),
            "title",
            ValueState::Known(json!("My title")),
            "manual",
            None,
            None,
            Confidence::new(1.0)?,
            DataLicense::UserOwned,
            true,
        )?;
        library.append_metadata_claim(&manual)?;
        let manual_diff = library
            .preview_claim_resolution(EntityKind::Release, &entity, "title")?
            .ok_or_else(|| Error::Catalog("missing manual diff".into()))?;
        assert_eq!(manual_diff.after(), &ValueState::Known(json!("My title")));
        Ok(())
    }

    #[test]
    fn provider_snapshots_are_content_addressed_and_license_scoped() -> Result<()> {
        let library = Library::open_in_memory()?;
        let first = library.store_provider_snapshot(
            "musicbrainz",
            "release:abc",
            None,
            &DataLicense::Cc0,
            br#"{"id":"abc"}"#,
            true,
        )?;
        let second = library.store_provider_snapshot(
            "musicbrainz",
            "release:abc",
            None,
            &DataLicense::Cc0,
            br#"{"id":"abc"}"#,
            true,
        )?;
        assert_eq!(first, second);
        Ok(())
    }

    #[test]
    fn provider_refresh_is_an_atomic_reviewed_diff_and_never_a_tag_write() -> Result<()> {
        let library = Library::open_in_memory()?;
        let entity = EntityId::new();
        for (field, value) in [("title", json!("New title")), ("country", json!("GB"))] {
            library.append_metadata_claim(&MetadataClaim::new(
                EntityKind::Release,
                entity.clone(),
                field,
                ValueState::Known(value),
                "provider",
                Some("musicbrainz".into()),
                Some("snapshot-v2".into()),
                Confidence::new(0.99)?,
                DataLicense::Cc0,
                false,
            )?)?;
        }
        let plan = library.plan_provider_refresh(EntityKind::Release, &entity)?;
        assert_eq!(plan.diffs().len(), 2);
        assert!(library.execute_provider_refresh(&plan).is_err());
        library.approve_provider_refresh(&plan)?;
        library.execute_provider_refresh(&plan)?;
        assert_eq!(
            library.durable_plan(plan.id())?.state(),
            PlanState::Complete
        );
        let operation_count: u32 = library.conn.query_row(
            "SELECT COUNT(*) FROM operation_journal WHERE plan_id = ?1",
            [plan.id().as_str()],
            |row| row.get(0),
        )?;
        assert_eq!(operation_count, 0, "refresh must not write media files");

        let stale = library.plan_provider_refresh(EntityKind::Release, &entity)?;
        library.approve_provider_refresh(&stale)?;
        library.append_metadata_claim(&MetadataClaim::new(
            EntityKind::Release,
            entity,
            "title",
            ValueState::Known(json!("Changed after preview")),
            "provider",
            Some("musicbrainz".into()),
            Some("snapshot-v3".into()),
            Confidence::new(1.0)?,
            DataLicense::Cc0,
            false,
        )?)?;
        assert!(library.execute_provider_refresh(&stale).is_err());
        assert_eq!(library.durable_plan(stale.id())?.state(), PlanState::Failed);
        Ok(())
    }
}
