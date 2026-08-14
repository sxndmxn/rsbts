//! Durable plans and resumable provider-job state machines.

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::Library;
use crate::failpoints;
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct PlanId(String);

impl PlanId {
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        uuid::Uuid::parse_str(&value)
            .map_err(|error| Error::Operation(format!("invalid plan ID: {error}")))?;
        Ok(Self(value.to_ascii_lowercase()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for PlanId {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de> Deserialize<'de> for PlanId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PlanKind {
    Match,
    Audit,
    ProviderRefresh,
    TagProjection,
    PathProjection,
    ArtworkProjection,
    Removal,
    Purge,
    Manifest,
    BackupRestore,
    AncillaryImport,
}

impl PlanKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Audit => "audit",
            Self::ProviderRefresh => "provider-refresh",
            Self::TagProjection => "tag-projection",
            Self::PathProjection => "path-projection",
            Self::ArtworkProjection => "artwork-projection",
            Self::Removal => "removal",
            Self::Purge => "purge",
            Self::Manifest => "manifest",
            Self::BackupRestore => "backup-restore",
            Self::AncillaryImport => "ancillary-import",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PlanState {
    Planned,
    Approved,
    Running,
    Paused,
    Complete,
    Failed,
    Cancelled,
}

impl PlanState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Approved => "approved",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "planned" => Ok(Self::Planned),
            "approved" => Ok(Self::Approved),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(Error::Operation(format!("invalid plan state: {value}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurablePlan {
    id: PlanId,
    kind: String,
    state: PlanState,
    request: serde_json::Value,
    preview: serde_json::Value,
    progress_current: u64,
    progress_total: Option<u64>,
    resume_cursor: Option<String>,
    cancel_requested: bool,
    error: Option<String>,
}

impl DurablePlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    #[must_use]
    pub const fn state(&self) -> PlanState {
        self.state
    }

    #[must_use]
    pub const fn cancel_requested(&self) -> bool {
        self.cancel_requested
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[must_use]
    pub const fn request(&self) -> &serde_json::Value {
        &self.request
    }

    #[must_use]
    pub const fn preview(&self) -> &serde_json::Value {
        &self.preview
    }

    #[must_use]
    pub const fn progress(&self) -> (u64, Option<u64>) {
        (self.progress_current, self.progress_total)
    }

    #[must_use]
    pub fn resume_cursor(&self) -> Option<&str> {
        self.resume_cursor.as_deref()
    }

    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEvent {
    sequence: u64,
    event_type: String,
    detail: serde_json::Value,
    created_at: DateTime<Utc>,
}

impl PlanEvent {
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    #[must_use]
    pub const fn detail(&self) -> &serde_json::Value {
        &self.detail
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderJob {
    id: PlanId,
    provider: String,
    operation: String,
    request: serde_json::Value,
    attempt_count: u32,
}

impl ProviderJob {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    #[must_use]
    pub const fn request(&self) -> &serde_json::Value {
        &self.request
    }
}

impl Library {
    /// Persist a preview without executing it. Approval is a separate transition.
    pub fn create_durable_plan(
        &self,
        kind: PlanKind,
        request: &serde_json::Value,
        preview: &serde_json::Value,
        progress_total: Option<u64>,
    ) -> Result<PlanId> {
        let id = PlanId::new();
        let now = Utc::now().to_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO durable_plans
             (id, kind, state, request_json, preview_json, progress_total,
              created_at, updated_at)
             VALUES (?1, ?2, 'planned', ?3, ?4, ?5, ?6, ?6)",
            params![
                id.as_str(),
                kind.as_str(),
                serde_json::to_string(request)?,
                serde_json::to_string(preview)?,
                progress_total,
                now
            ],
        )?;
        append_plan_event(
            &transaction,
            &id,
            "planned",
            &json!({"kind": kind.as_str(), "progress_total": progress_total}),
        )?;
        transaction.commit()?;
        failpoints::hit("db.create-durable-plan")?;
        Ok(id)
    }

    pub fn approve_durable_plan(&self, id: &PlanId) -> Result<()> {
        self.transition_plan_with_event(id, PlanState::Planned, PlanState::Approved, None)
    }

    pub fn start_durable_plan(&self, id: &PlanId) -> Result<()> {
        self.transition_plan_with_event(id, PlanState::Approved, PlanState::Running, None)
    }

    pub fn pause_durable_plan(&self, id: &PlanId, cursor: Option<&str>) -> Result<()> {
        self.transition_plan_with_event(id, PlanState::Running, PlanState::Paused, cursor)
    }

    pub fn resume_durable_plan(&self, id: &PlanId) -> Result<()> {
        self.transition_plan_with_event(id, PlanState::Paused, PlanState::Running, None)
    }

    pub fn request_plan_cancellation(&self, id: &PlanId) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE durable_plans SET cancel_requested = 1, updated_at = ?2
             WHERE id = ?1 AND state IN ('approved', 'running', 'paused')",
            params![id.as_str(), Utc::now().to_rfc3339()],
        )?;
        require_one(changed, "plan cannot be cancelled from its current state")?;
        append_plan_event(&transaction, id, "cancellation-requested", &json!({}))?;
        transaction.commit()?;
        failpoints::hit("db.plan-cancellation")
    }

    pub fn update_plan_progress(
        &self,
        id: &PlanId,
        current: u64,
        cursor: Option<&str>,
    ) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE durable_plans
             SET progress_current = ?2, resume_cursor = ?3, updated_at = ?4
             WHERE id = ?1 AND state = 'running'
               AND (progress_total IS NULL OR ?2 <= progress_total)",
            params![id.as_str(), current, cursor, Utc::now().to_rfc3339()],
        )?;
        require_one(changed, "invalid or stale plan progress update")?;
        append_plan_event(
            &transaction,
            id,
            "progress",
            &json!({"current": current, "cursor": cursor}),
        )?;
        transaction.commit()?;
        failpoints::hit("db.plan-progress")
    }

    pub fn finish_durable_plan(
        &self,
        id: &PlanId,
        outcome: PlanState,
        error: Option<&str>,
    ) -> Result<()> {
        if !matches!(
            outcome,
            PlanState::Complete | PlanState::Failed | PlanState::Cancelled
        ) {
            return Err(Error::Operation("plan outcome is not terminal".into()));
        }
        let transaction = self.conn.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE durable_plans
             SET state = ?2, error = ?3, updated_at = ?4, completed_at = ?4
             WHERE id = ?1 AND state IN ('approved', 'running', 'paused')",
            params![
                id.as_str(),
                outcome.as_str(),
                error,
                Utc::now().to_rfc3339()
            ],
        )?;
        require_one(changed, "plan cannot finish from its current state")?;
        append_plan_event(&transaction, id, outcome.as_str(), &json!({"error": error}))?;
        transaction.commit()?;
        failpoints::hit("db.plan-finish")
    }

    pub fn durable_plan(&self, id: &PlanId) -> Result<DurablePlan> {
        self.conn
            .query_row(
                "SELECT kind, state, request_json, preview_json, progress_current,
                        progress_total, resume_cursor, cancel_requested, error
                 FROM durable_plans WHERE id = ?1",
                [id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u64>(4)?,
                        row.get::<_, Option<u64>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, bool>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .map_err(Into::into)
            .and_then(
                |(kind, state, request, preview, current, total, cursor, cancel, error)| {
                    Ok(DurablePlan {
                        id: id.clone(),
                        kind,
                        state: PlanState::parse(&state)?,
                        request: serde_json::from_str(&request)?,
                        preview: serde_json::from_str(&preview)?,
                        progress_current: current,
                        progress_total: total,
                        resume_cursor: cursor,
                        cancel_requested: cancel,
                        error,
                    })
                },
            )
    }

    /// Return immutable event history in sequence order.
    pub fn plan_events(&self, id: &PlanId) -> Result<Vec<PlanEvent>> {
        let mut statement = self.conn.prepare(
            "SELECT sequence, event_type, detail_json, created_at
             FROM plan_events WHERE plan_id = ?1 ORDER BY sequence",
        )?;
        let rows = statement.query_map([id.as_str()], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (sequence, event_type, detail, created_at) = row?;
            Ok(PlanEvent {
                sequence,
                event_type,
                detail: serde_json::from_str(&detail)?,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|error| {
                        Error::Operation(format!("invalid stored plan-event timestamp: {error}"))
                    })?
                    .with_timezone(&Utc),
            })
        })
        .collect()
    }

    fn transition_plan_with_event(
        &self,
        id: &PlanId,
        from: PlanState,
        to: PlanState,
        cursor: Option<&str>,
    ) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        transition_plan(&transaction, id, from, to, cursor)?;
        append_plan_event(
            &transaction,
            id,
            to.as_str(),
            &json!({"from": from.as_str(), "cursor": cursor}),
        )?;
        transaction.commit()?;
        failpoints::hit("db.plan-transition")
    }

    /// Enqueue a cached provider request. Completed jobs are immutable and not rerun.
    pub fn enqueue_provider_job(
        &self,
        provider: &str,
        operation: &str,
        request: &serde_json::Value,
        available_at: DateTime<Utc>,
    ) -> Result<PlanId> {
        check_label(provider, "provider")?;
        check_label(operation, "provider operation")?;
        let request_json = serde_json::to_string(request)?;
        if let Some(existing) = self
            .conn
            .query_row(
                "SELECT id FROM provider_jobs
                 WHERE provider = ?1 AND operation = ?2 AND request_json = ?3
                   AND state IN ('queued', 'running', 'retry', 'complete')
                 ORDER BY created_at DESC LIMIT 1",
                params![provider, operation, request_json],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return PlanId::parse(existing);
        }
        let id = PlanId::new();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO provider_jobs
             (id, provider, operation, request_json, state, available_at,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?6)",
            params![
                id.as_str(),
                provider,
                operation,
                request_json,
                available_at.to_rfc3339(),
                now
            ],
        )?;
        failpoints::hit("db.enqueue-provider-job")?;
        Ok(id)
    }

    /// Atomically claim one ready request for a provider.
    pub fn claim_provider_job(&self, provider: &str) -> Result<Option<ProviderJob>> {
        let transaction = self.conn.unchecked_transaction()?;
        let ready = transaction
            .query_row(
                "SELECT id, operation, request_json, attempt_count
                 FROM provider_jobs
                 WHERE provider = ?1 AND state IN ('queued', 'retry') AND available_at <= ?2
                 ORDER BY available_at, id LIMIT 1",
                params![provider, Utc::now().to_rfc3339()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u32>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, operation, request, attempts)) = ready else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE provider_jobs
             SET state = 'running', attempt_count = attempt_count + 1,
                 started_at = ?2, updated_at = ?2
             WHERE id = ?1 AND state IN ('queued', 'retry')",
            params![id, Utc::now().to_rfc3339()],
        )?;
        require_one(changed, "provider job was claimed concurrently")?;
        transaction.commit()?;
        failpoints::hit("db.claim-provider-job")?;
        Ok(Some(ProviderJob {
            id: PlanId::parse(id)?,
            provider: provider.to_owned(),
            operation,
            request: serde_json::from_str(&request)?,
            attempt_count: attempts.saturating_add(1),
        }))
    }

    pub fn complete_provider_job(&self, job: &ProviderJob, snapshot_id: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE provider_jobs
             SET state = 'complete', response_snapshot_id = ?2,
                 completed_at = ?3, updated_at = ?3, last_error = NULL
             WHERE id = ?1 AND state = 'running'",
            params![job.id.as_str(), snapshot_id, Utc::now().to_rfc3339()],
        )?;
        require_one(changed, "provider job is not running")?;
        failpoints::hit("db.complete-provider-job")
    }

    pub fn fail_provider_job(
        &self,
        job: &ProviderJob,
        detail: &str,
        retriable: bool,
    ) -> Result<()> {
        let state = if retriable { "retry" } else { "failed" };
        let delay_seconds = 2_i64.saturating_pow(job.attempt_count.min(10));
        let available = Utc::now() + Duration::seconds(delay_seconds);
        let changed = self.conn.execute(
            "UPDATE provider_jobs
             SET state = ?2, available_at = ?3, last_error = ?4, updated_at = ?5
             WHERE id = ?1 AND state = 'running'",
            params![
                job.id.as_str(),
                state,
                available.to_rfc3339(),
                detail,
                Utc::now().to_rfc3339()
            ],
        )?;
        require_one(changed, "provider job is not running")?;
        failpoints::hit("db.fail-provider-job")
    }

    pub(crate) fn recover_interrupted_provider_jobs(&self) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE provider_jobs
                 SET state = 'retry', available_at = ?1, started_at = NULL,
                     last_error = 'interrupted while running', updated_at = ?1
                 WHERE state = 'running'",
                [Utc::now().to_rfc3339()],
            )
            .map_err(Into::into)
    }
}

fn transition_plan(
    connection: &rusqlite::Connection,
    id: &PlanId,
    from: PlanState,
    to: PlanState,
    cursor: Option<&str>,
) -> Result<()> {
    let changed = connection.execute(
        "UPDATE durable_plans SET state = ?3, resume_cursor = COALESCE(?4, resume_cursor),
         updated_at = ?5 WHERE id = ?1 AND state = ?2 AND cancel_requested = 0",
        params![
            id.as_str(),
            from.as_str(),
            to.as_str(),
            cursor,
            Utc::now().to_rfc3339()
        ],
    )?;
    require_one(changed, "invalid or stale plan transition")
}

pub(crate) fn append_plan_event(
    connection: &rusqlite::Connection,
    id: &PlanId,
    event_type: &str,
    detail: &serde_json::Value,
) -> Result<()> {
    check_label(event_type, "plan event type")?;
    connection.execute(
        "INSERT INTO plan_events (plan_id, sequence, event_type, detail_json, created_at)
         SELECT ?1, COALESCE(MAX(sequence), -1) + 1, ?2, ?3, ?4
         FROM plan_events WHERE plan_id = ?1",
        params![
            id.as_str(),
            event_type,
            serde_json::to_string(detail)?,
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn require_one(changed: usize, detail: &str) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(Error::Operation(detail.into()))
    }
}

fn check_label(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().any(char::is_control) || value.len() > 256 {
        Err(Error::Operation(format!("invalid {field}")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn exercise_transition_sequence(actions: &[u8]) -> Result<()> {
        let library = Library::open_in_memory()?;
        let id = library.create_durable_plan(
            PlanKind::Audit,
            &json!({"property": true}),
            &json!({"assets": actions.len()}),
            Some(actions.len() as u64),
        )?;
        for (index, action) in actions.iter().enumerate() {
            match action % 8 {
                0 => {
                    let _result = library.approve_durable_plan(&id);
                }
                1 => {
                    let _result = library.start_durable_plan(&id);
                }
                2 => {
                    let _result = library.pause_durable_plan(&id, Some("cursor"));
                }
                3 => {
                    let _result = library.resume_durable_plan(&id);
                }
                4 => {
                    let _result = library.update_plan_progress(&id, index as u64, Some("cursor"));
                }
                5 => {
                    let _result = library.request_plan_cancellation(&id);
                }
                6 => {
                    let _result = library.finish_durable_plan(&id, PlanState::Complete, None);
                }
                _ => {
                    let _result =
                        library.finish_durable_plan(&id, PlanState::Failed, Some("failure"));
                }
            }
        }
        let _plan = library.durable_plan(&id)?;
        let events = library.plan_events(&id)?;
        for (sequence, event) in events.iter().enumerate() {
            if event.sequence() != sequence as u64 {
                return Err(Error::Operation(
                    "plan-event sequence is not contiguous".into(),
                ));
            }
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn arbitrary_journal_transition_sequences_remain_valid(
            actions in proptest::collection::vec(any::<u8>(), 0..128)
        ) {
            prop_assert!(exercise_transition_sequence(&actions).is_ok());
        }
    }

    #[test]
    fn durable_plans_enforce_preview_approval_execution_order() -> Result<()> {
        let library = Library::open_in_memory()?;
        let id = library.create_durable_plan(
            PlanKind::Audit,
            &json!({"deep": true}),
            &json!({"assets": 10}),
            Some(10),
        )?;
        assert!(library.start_durable_plan(&id).is_err());
        library.approve_durable_plan(&id)?;
        library.start_durable_plan(&id)?;
        library.update_plan_progress(&id, 4, Some("asset-4"))?;
        library.pause_durable_plan(&id, Some("asset-4"))?;
        library.resume_durable_plan(&id)?;
        library.finish_durable_plan(&id, PlanState::Complete, None)?;
        assert_eq!(library.durable_plan(&id)?.state(), PlanState::Complete);
        Ok(())
    }

    #[test]
    fn provider_jobs_resume_without_repeating_completed_requests() -> Result<()> {
        let library = Library::open_in_memory()?;
        let request = json!({"release": "abc"});
        let id = library.enqueue_provider_job("test", "lookup-release", &request, Utc::now())?;
        let job = library
            .claim_provider_job("test")?
            .ok_or_else(|| Error::Operation("job was not claimable".into()))?;
        assert_eq!(job.id(), &id);
        assert_eq!(library.recover_interrupted_provider_jobs()?, 1);
        let resumed = library
            .claim_provider_job("test")?
            .ok_or_else(|| Error::Operation("job did not resume".into()))?;
        let snapshot = library.store_provider_snapshot(
            "test",
            "release:abc",
            None,
            &crate::catalog::DataLicense::UserOwned,
            b"{}",
            true,
        )?;
        library.complete_provider_job(&resumed, snapshot.id().as_str())?;
        assert!(library.claim_provider_job("test")?.is_none());
        assert_eq!(
            library.enqueue_provider_job("test", "lookup-release", &request, Utc::now())?,
            id
        );
        Ok(())
    }
}
