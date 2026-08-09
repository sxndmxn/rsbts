//! Streamed, resumable fixity runs with durable result history.

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{params, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::asset::digest_reader;
use crate::db::{file_identity, Library};
use crate::fsops::ReadRoot;
use crate::media::{decoded_audio_essence_hash_from_file, probe_media_from_file, MediaDescriptor};
use crate::operations::{append_plan_event, PlanId, PlanKind, PlanState};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum FixityMode {
    Quick,
    Deep,
}

impl FixityMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Deep => "deep",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "quick" => Ok(Self::Quick),
            "deep" => Ok(Self::Deep),
            _ => Err(Error::Operation(format!("unknown fixity mode: {value}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct FixityScheduleId(String);

impl FixityScheduleId {
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        uuid::Uuid::parse_str(&value)
            .map_err(|error| Error::Operation(format!("invalid fixity schedule ID: {error}")))?;
        Ok(Self(value.to_ascii_lowercase()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for FixityScheduleId {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de> Deserialize<'de> for FixityScheduleId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixitySchedule {
    id: FixityScheduleId,
    mode: FixityMode,
    interval_seconds: u64,
    enabled: bool,
    next_run_at: DateTime<Utc>,
    last_plan_id: Option<PlanId>,
    last_completed_at: Option<DateTime<Utc>>,
    last_failure_count: Option<u64>,
}

impl FixitySchedule {
    #[must_use]
    pub const fn id(&self) -> &FixityScheduleId {
        &self.id
    }

    #[must_use]
    pub const fn next_run_at(&self) -> DateTime<Utc> {
        self.next_run_at
    }

    #[must_use]
    pub const fn last_failure_count(&self) -> Option<u64> {
        self.last_failure_count
    }

    #[must_use]
    pub const fn mode(&self) -> FixityMode {
        self.mode
    }

    #[must_use]
    pub const fn interval_seconds(&self) -> u64 {
        self.interval_seconds
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledFixityRun {
    plan_id: PlanId,
    state: PlanState,
    checked: u64,
    failures: u64,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

impl ScheduledFixityRun {
    #[must_use]
    pub const fn plan_id(&self) -> &PlanId {
        &self.plan_id
    }

    #[must_use]
    pub const fn failures(&self) -> u64 {
        self.failures
    }

    #[must_use]
    pub const fn state(&self) -> PlanState {
        self.state
    }

    #[must_use]
    pub const fn checked(&self) -> u64 {
        self.checked
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum FixityResultState {
    Ok,
    Missing,
    Modified,
    Replaced,
    Unverified,
    Corrupt,
    Offline,
    PolicyDivergent,
    Unreadable,
}

impl FixityResultState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Modified => "modified",
            Self::Replaced => "replaced",
            Self::Unverified => "unverified",
            Self::Corrupt => "corrupt",
            Self::Offline => "offline",
            Self::PolicyDivergent => "policy-divergent",
            Self::Unreadable => "unreadable",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "ok" => Ok(Self::Ok),
            "missing" => Ok(Self::Missing),
            "modified" => Ok(Self::Modified),
            "replaced" => Ok(Self::Replaced),
            "unverified" => Ok(Self::Unverified),
            "corrupt" => Ok(Self::Corrupt),
            "offline" => Ok(Self::Offline),
            "policy-divergent" => Ok(Self::PolicyDivergent),
            "unreadable" => Ok(Self::Unreadable),
            _ => Err(Error::Operation(format!(
                "unknown fixity result state: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixityPlan {
    id: PlanId,
    mode: FixityMode,
    asset_count: u64,
    schedule_id: Option<FixityScheduleId>,
}

impl FixityPlan {
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    #[must_use]
    pub const fn asset_count(&self) -> u64 {
        self.asset_count
    }

    #[must_use]
    pub const fn schedule_id(&self) -> Option<&FixityScheduleId> {
        self.schedule_id.as_ref()
    }

    #[must_use]
    pub const fn mode(&self) -> FixityMode {
        self.mode
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixityResult {
    asset_id: String,
    path: PathBuf,
    state: FixityResultState,
    detail: Option<String>,
}

impl FixityResult {
    #[must_use]
    pub const fn state(&self) -> FixityResultState {
        self.state
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn asset_id(&self) -> &str {
        &self.asset_id
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixityProgress {
    plan_id: PlanId,
    checked: u64,
    failures: u64,
    total: u64,
    complete: bool,
    cursor: Option<String>,
    results: Vec<FixityResult>,
}

impl FixityProgress {
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.complete
    }

    #[must_use]
    pub fn results(&self) -> &[FixityResult] {
        &self.results
    }

    #[must_use]
    pub const fn checked(&self) -> u64 {
        self.checked
    }

    #[must_use]
    pub const fn failures(&self) -> u64 {
        self.failures
    }

    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }
}

#[derive(Debug)]
struct AssetEvidence {
    id: String,
    role: String,
    absolute_path: PathBuf,
    relative_path: PathBuf,
    root_path: PathBuf,
    root_state: String,
    verification_state: String,
    projection_state: String,
    expected_size: Option<u64>,
    expected_blake3: Option<String>,
    expected_sha256: Option<String>,
    expected_mtime: Option<String>,
    expected_identity: Option<String>,
    expected_media: Option<String>,
    expected_essence: Option<String>,
}

#[derive(Debug, Default)]
struct ObservedEvidence {
    size: Option<u64>,
    blake3: Option<String>,
    sha256: Option<String>,
    essence: Option<String>,
}

impl Library {
    /// Persist an audit preview; approval and execution remain separate.
    pub fn plan_fixity(&self, mode: FixityMode) -> Result<FixityPlan> {
        let asset_count =
            self.conn
                .query_row("SELECT COUNT(*) FROM assets WHERE managed = 1", [], |row| {
                    row.get::<_, u64>(0)
                })?;
        let request = json!({"mode": mode});
        let preview = json!({
            "mode": mode,
            "managed_assets": asset_count,
            "streamed": true,
            "resumable": true,
        });
        let id =
            self.create_durable_plan(PlanKind::Audit, &request, &preview, Some(asset_count))?;
        Ok(FixityPlan {
            id,
            mode,
            asset_count,
            schedule_id: None,
        })
    }

    /// Rehydrate a persisted fixity plan for approval, resumption, or result access.
    pub fn fixity_plan(&self, id: &PlanId) -> Result<FixityPlan> {
        #[derive(Deserialize)]
        struct Request {
            mode: FixityMode,
            #[serde(default)]
            schedule_id: Option<FixityScheduleId>,
        }

        let durable = self.durable_plan(id)?;
        if durable.kind() != "audit" {
            return Err(Error::Operation("plan is not a fixity audit".into()));
        }
        let request: Request = serde_json::from_value(durable.request().clone())?;
        Ok(FixityPlan {
            id: id.clone(),
            mode: request.mode,
            asset_count: durable.progress().1.unwrap_or(0),
            schedule_id: request.schedule_id,
        })
    }

    /// Create a persistent schedule. Enabling it is standing approval for each
    /// read-only fixity run, while each generated run remains an independent plan.
    pub fn schedule_fixity(
        &self,
        mode: FixityMode,
        interval: std::time::Duration,
        first_run_at: DateTime<Utc>,
    ) -> Result<FixityScheduleId> {
        let interval_seconds = i64::try_from(interval.as_secs()).map_err(|_error| {
            Error::Operation("fixity interval exceeds the supported range".into())
        })?;
        if interval_seconds == 0 {
            return Err(Error::Operation(
                "fixity interval must be at least one second".into(),
            ));
        }
        let id = FixityScheduleId::new();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO fixity_schedules
             (id, mode, interval_seconds, enabled, next_run_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?5)",
            params![
                id.as_str(),
                mode.as_str(),
                interval_seconds,
                first_run_at.to_rfc3339(),
                now
            ],
        )?;
        Ok(id)
    }

    pub fn set_fixity_schedule_enabled(&self, id: &FixityScheduleId, enabled: bool) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE fixity_schedules SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.as_str(), enabled, Utc::now().to_rfc3339()],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(Error::Operation("fixity schedule does not exist".into()))
        }
    }

    /// Materialize due schedule occurrences as approved, durable plans.
    pub fn plan_due_fixity(&self, now: DateTime<Utc>, limit: u32) -> Result<Vec<FixityPlan>> {
        if limit == 0 || limit > 256 {
            return Err(Error::Operation(
                "due fixity schedule limit must be between 1 and 256".into(),
            ));
        }
        let transaction = self.conn.unchecked_transaction()?;
        let due = load_due_schedules(&transaction, now, limit)?;
        let asset_count =
            transaction.query_row("SELECT COUNT(*) FROM assets WHERE managed = 1", [], |row| {
                row.get::<_, u64>(0)
            })?;
        let mut plans = Vec::with_capacity(due.len());
        for schedule in due {
            let next = advance_schedule(schedule.next_run_at, schedule.interval_seconds, now)?;
            let plan_id = PlanId::new();
            let request = json!({
                "mode": schedule.mode,
                "schedule_id": schedule.id,
                "scheduled_for": schedule.next_run_at,
            });
            let preview = json!({
                "mode": schedule.mode,
                "managed_assets": asset_count,
                "streamed": true,
                "resumable": true,
                "scheduled": true,
            });
            let timestamp = now.to_rfc3339();
            transaction.execute(
                "INSERT INTO durable_plans
                 (id, kind, state, request_json, preview_json, progress_total,
                  created_at, updated_at)
                 VALUES (?1, 'audit', 'approved', ?2, ?3, ?4, ?5, ?5)",
                params![
                    plan_id.as_str(),
                    serde_json::to_string(&request)?,
                    serde_json::to_string(&preview)?,
                    asset_count,
                    timestamp
                ],
            )?;
            append_plan_event(
                &transaction,
                &plan_id,
                "planned",
                &json!({
                    "kind": "audit",
                    "progress_total": asset_count,
                    "schedule_id": schedule.id,
                }),
            )?;
            append_plan_event(
                &transaction,
                &plan_id,
                "approved",
                &json!({"standing_approval": true, "schedule_id": schedule.id}),
            )?;
            let changed = transaction.execute(
                "UPDATE fixity_schedules
                 SET next_run_at = ?2, last_plan_id = ?3, updated_at = ?4
                 WHERE id = ?1 AND enabled = 1 AND next_run_at = ?5",
                params![
                    schedule.id.as_str(),
                    next.to_rfc3339(),
                    plan_id.as_str(),
                    timestamp,
                    schedule.next_run_at.to_rfc3339()
                ],
            )?;
            if changed != 1 {
                return Err(Error::Operation(
                    "fixity schedule changed while materializing its run".into(),
                ));
            }
            plans.push(FixityPlan {
                id: plan_id,
                mode: schedule.mode,
                asset_count,
                schedule_id: Some(schedule.id),
            });
        }
        transaction.commit()?;
        Ok(plans)
    }

    pub fn fixity_schedules_page(
        &self,
        after_id: Option<&FixityScheduleId>,
        limit: u32,
    ) -> Result<Vec<FixitySchedule>> {
        if limit == 0 || limit > 4096 {
            return Err(Error::Operation(
                "fixity schedule limit must be between 1 and 4096".into(),
            ));
        }
        let mut statement = self.conn.prepare(
            "SELECT id, mode, interval_seconds, enabled, next_run_at,
                    last_plan_id, last_completed_at, last_failure_count
             FROM fixity_schedules WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = statement
            .query_map(
                params![after_id.map_or("", FixityScheduleId::as_str), limit],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<u64>>(7)?,
                    ))
                },
            )?
            .map(|row| {
                let (id, mode, interval, enabled, next, plan, completed, failures) = row?;
                Ok(FixitySchedule {
                    id: FixityScheduleId::parse(id)?,
                    mode: FixityMode::parse(&mode)?,
                    interval_seconds: interval,
                    enabled,
                    next_run_at: parse_timestamp(&next)?,
                    last_plan_id: plan.map(PlanId::parse).transpose()?,
                    last_completed_at: completed.as_deref().map(parse_timestamp).transpose()?,
                    last_failure_count: failures,
                })
            })
            .collect();
        rows
    }

    /// Return immutable run summaries for one schedule in keyset order.
    pub fn scheduled_fixity_history(
        &self,
        schedule_id: &FixityScheduleId,
        after_plan_id: Option<&PlanId>,
        limit: u32,
    ) -> Result<Vec<ScheduledFixityRun>> {
        if limit == 0 || limit > 4096 {
            return Err(Error::Operation(
                "fixity history limit must be between 1 and 4096".into(),
            ));
        }
        let mut statement = self.conn.prepare(
            "SELECT id, state, checked_count, failure_count, started_at, completed_at
             FROM fixity_runs
             WHERE schedule_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3",
        )?;
        let rows = statement
            .query_map(
                params![
                    schedule_id.as_str(),
                    after_plan_id.map_or("", PlanId::as_str),
                    limit
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, u64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )?
            .map(|row| {
                let (id, state, checked, failures, started, completed) = row?;
                Ok(ScheduledFixityRun {
                    plan_id: PlanId::parse(id)?,
                    state: PlanState::parse(&state)?,
                    checked,
                    failures,
                    started_at: parse_timestamp(&started)?,
                    completed_at: completed.as_deref().map(parse_timestamp).transpose()?,
                })
            })
            .collect();
        rows
    }

    pub fn approve_fixity(&self, plan: &FixityPlan) -> Result<()> {
        self.approve_durable_plan(plan.id())
    }

    /// Process at most `page_size` assets and persist the cursor and results.
    pub fn run_fixity_page(&self, plan: &FixityPlan, page_size: u32) -> Result<FixityProgress> {
        if page_size == 0 || page_size > 4096 {
            return Err(Error::Operation(
                "fixity page size must be between 1 and 4096".into(),
            ));
        }
        if self.durable_plan(plan.id())?.cancel_requested() {
            return self.cancel_fixity(plan);
        }
        self.ensure_fixity_running(plan)?;
        let (cursor, checked, failures) = self.conn.query_row(
            "SELECT cursor_asset_id, checked_count, failure_count
             FROM fixity_runs WHERE id = ?1 AND state = 'running'",
            [plan.id().as_str()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            },
        )?;
        let assets = self.fixity_asset_page(cursor.as_deref(), page_size)?;
        let mut results: Vec<(FixityResult, ObservedEvidence)> = Vec::with_capacity(assets.len());
        let mut new_failures = 0_u64;
        for (index, asset) in assets.iter().enumerate() {
            if index % 64 == 0 && self.durable_plan(plan.id())?.cancel_requested() {
                let page_failures = results
                    .iter()
                    .filter(|(result, _)| result.state != FixityResultState::Ok)
                    .count() as u64;
                self.persist_fixity_page(
                    plan,
                    &results,
                    checked,
                    failures.saturating_add(page_failures),
                    false,
                )?;
                return self.cancel_fixity(plan);
            }
            let result = inspect_asset(asset, plan.mode);
            new_failures += u64::from(result.0.state != FixityResultState::Ok);
            results.push((result.0, result.1));
        }
        let complete = assets.len() < page_size as usize;
        let progress = self.persist_fixity_page(
            plan,
            &results,
            checked,
            failures.saturating_add(new_failures),
            complete,
        )?;
        Ok(progress)
    }

    /// Stream a stable keyset page from a prior run's durable history.
    pub fn fixity_results_page(
        &self,
        plan_id: &PlanId,
        after_asset_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<FixityResult>> {
        if limit == 0 || limit > 4096 {
            return Err(Error::Operation(
                "fixity result limit must be between 1 and 4096".into(),
            ));
        }
        let mut statement = self.conn.prepare(
            "SELECT fr.asset_id, a.absolute_path, fr.state, fr.detail
             FROM fixity_results fr JOIN assets a ON a.id = fr.asset_id
             WHERE fr.run_id = ?1 AND fr.asset_id > ?2
             ORDER BY fr.asset_id LIMIT ?3",
        )?;
        let results = statement
            .query_map(
                params![plan_id.as_str(), after_asset_id.unwrap_or(""), limit],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        PathBuf::from(row.get::<_, String>(1)?),
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )?
            .map(|row| {
                let (asset_id, path, state, detail) = row?;
                Ok(FixityResult {
                    asset_id,
                    path,
                    state: FixityResultState::parse(&state)?,
                    detail,
                })
            })
            .collect();
        results
    }

    fn ensure_fixity_running(&self, plan: &FixityPlan) -> Result<()> {
        match self.durable_plan(plan.id())?.state() {
            PlanState::Approved => {
                self.start_durable_plan(plan.id())?;
                self.conn.execute(
                    "INSERT OR IGNORE INTO fixity_runs
                     (id, plan_id, schedule_id, mode, state, started_at)
                     VALUES (?1, ?1, ?2, ?3, 'running', ?4)",
                    params![
                        plan.id().as_str(),
                        plan.schedule_id.as_ref().map(FixityScheduleId::as_str),
                        plan.mode.as_str(),
                        Utc::now().to_rfc3339()
                    ],
                )?;
            }
            PlanState::Running => {
                self.conn.execute(
                    "INSERT OR IGNORE INTO fixity_runs
                     (id, plan_id, schedule_id, mode, state, started_at)
                     VALUES (?1, ?1, ?2, ?3, 'running', ?4)",
                    params![
                        plan.id().as_str(),
                        plan.schedule_id.as_ref().map(FixityScheduleId::as_str),
                        plan.mode.as_str(),
                        Utc::now().to_rfc3339()
                    ],
                )?;
            }
            PlanState::Paused => {
                self.resume_durable_plan(plan.id())?;
                self.conn.execute(
                    "UPDATE fixity_runs SET state = 'running'
                     WHERE id = ?1 AND state = 'paused'",
                    [plan.id().as_str()],
                )?;
            }
            state => {
                return Err(Error::Operation(format!(
                    "fixity plan cannot run from state {state:?}"
                )));
            }
        }
        Ok(())
    }

    fn cancel_fixity(&self, plan: &FixityPlan) -> Result<FixityProgress> {
        let now = Utc::now().to_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO fixity_runs
             (id, plan_id, schedule_id, mode, state, started_at, completed_at)
             VALUES (?1, ?1, ?2, ?3, 'cancelled', ?4, ?4)",
            params![
                plan.id().as_str(),
                plan.schedule_id.as_ref().map(FixityScheduleId::as_str),
                plan.mode.as_str(),
                now
            ],
        )?;
        transaction.execute(
            "UPDATE fixity_runs SET state = 'cancelled', completed_at = ?2
             WHERE id = ?1 AND state IN ('running', 'paused')",
            params![plan.id().as_str(), now],
        )?;
        transaction.execute(
            "UPDATE durable_plans
             SET state = 'cancelled', updated_at = ?2, completed_at = ?2
             WHERE id = ?1 AND state IN ('approved', 'running', 'paused')",
            params![plan.id().as_str(), now],
        )?;
        append_plan_event(
            &transaction,
            plan.id(),
            "cancelled",
            &json!({"reason": "cancellation requested"}),
        )?;
        let (checked, failures, cursor) = transaction.query_row(
            "SELECT checked_count, failure_count, cursor_asset_id
             FROM fixity_runs WHERE id = ?1",
            [plan.id().as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if let Some(schedule_id) = &plan.schedule_id {
            transaction.execute(
                "UPDATE fixity_schedules
                 SET last_completed_at = ?2, last_failure_count = ?3, updated_at = ?2
                 WHERE id = ?1",
                params![schedule_id.as_str(), now, failures],
            )?;
        }
        transaction.commit()?;
        Ok(FixityProgress {
            plan_id: plan.id.clone(),
            checked,
            failures,
            total: plan.asset_count,
            complete: false,
            cursor,
            results: Vec::new(),
        })
    }

    fn fixity_asset_page(&self, cursor: Option<&str>, limit: u32) -> Result<Vec<AssetEvidence>> {
        let mut statement = self.conn.prepare(
            "SELECT a.id, a.role, a.absolute_path, a.relative_path, lr.path, lr.state,
                    a.verification_state, a.projection_state, a.byte_size,
                    a.blake3, a.sha256, a.mtime, a.entry_identity,
                    a.media_json, a.audio_essence_hash
             FROM assets a JOIN library_roots lr ON lr.id = a.root_id
             WHERE a.managed = 1 AND a.id > ?1
             ORDER BY a.id LIMIT ?2",
        )?;
        let assets = statement
            .query_map(params![cursor.unwrap_or(""), limit], |row| {
                Ok(AssetEvidence {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    absolute_path: PathBuf::from(row.get::<_, String>(2)?),
                    relative_path: PathBuf::from(row.get::<_, String>(3)?),
                    root_path: PathBuf::from(row.get::<_, String>(4)?),
                    root_state: row.get(5)?,
                    verification_state: row.get(6)?,
                    projection_state: row.get(7)?,
                    expected_size: row.get(8)?,
                    expected_blake3: row.get(9)?,
                    expected_sha256: row.get(10)?,
                    expected_mtime: row.get(11)?,
                    expected_identity: row.get(12)?,
                    expected_media: row.get(13)?,
                    expected_essence: row.get(14)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into);
        assets
    }

    fn persist_fixity_page(
        &self,
        plan: &FixityPlan,
        results: &[(FixityResult, ObservedEvidence)],
        previous_checked: u64,
        failure_count: u64,
        complete: bool,
    ) -> Result<FixityProgress> {
        let transaction = self.conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        for (result, observed) in results {
            transaction.execute(
                "INSERT OR REPLACE INTO fixity_results
                 (run_id, asset_id, state, observed_size, observed_blake3,
                  observed_sha256, observed_audio_essence_hash, detail, checked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    plan.id().as_str(),
                    result.asset_id,
                    result.state.as_str(),
                    observed.size,
                    observed.blake3,
                    observed.sha256,
                    observed.essence,
                    result.detail,
                    now
                ],
            )?;
        }
        let checked = previous_checked.saturating_add(results.len() as u64);
        let cursor = results.last().map(|(result, _)| result.asset_id.as_str());
        transaction.execute(
            "UPDATE fixity_runs
             SET state = ?2, cursor_asset_id = COALESCE(?3, cursor_asset_id),
                 checked_count = ?4, failure_count = ?5,
                 completed_at = CASE WHEN ?2 = 'complete' THEN ?6 ELSE NULL END
             WHERE id = ?1 AND state = 'running'",
            params![
                plan.id().as_str(),
                if complete { "complete" } else { "running" },
                cursor,
                checked,
                failure_count,
                now
            ],
        )?;
        transaction.execute(
            "UPDATE durable_plans
             SET state = ?2, progress_current = ?3,
                 resume_cursor = COALESCE(?4, resume_cursor), updated_at = ?5,
                 completed_at = CASE WHEN ?2 = 'complete' THEN ?5 ELSE NULL END
             WHERE id = ?1 AND state = 'running'",
            params![
                plan.id().as_str(),
                if complete { "complete" } else { "running" },
                checked,
                cursor,
                now
            ],
        )?;
        append_plan_event(
            &transaction,
            plan.id(),
            if complete { "complete" } else { "progress" },
            &json!({
                "current": checked,
                "cursor": cursor,
                "failures": failure_count,
            }),
        )?;
        if complete {
            if let Some(schedule_id) = &plan.schedule_id {
                transaction.execute(
                    "UPDATE fixity_schedules
                     SET last_completed_at = ?2, last_failure_count = ?3, updated_at = ?2
                     WHERE id = ?1",
                    params![schedule_id.as_str(), now, failure_count],
                )?;
            }
        }
        transaction.commit()?;
        Ok(FixityProgress {
            plan_id: plan.id.clone(),
            checked,
            failures: failure_count,
            total: plan.asset_count,
            complete,
            cursor: cursor.map(str::to_owned),
            results: results.iter().map(|(result, _)| result.clone()).collect(),
        })
    }
}

#[derive(Debug)]
struct DueSchedule {
    id: FixityScheduleId,
    mode: FixityMode,
    interval_seconds: i64,
    next_run_at: DateTime<Utc>,
}

fn load_due_schedules(
    transaction: &Transaction<'_>,
    now: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<DueSchedule>> {
    let mut statement = transaction.prepare(
        "SELECT id, mode, interval_seconds, next_run_at
         FROM fixity_schedules
         WHERE enabled = 1 AND next_run_at <= ?1
         ORDER BY next_run_at, id LIMIT ?2",
    )?;
    let rows = statement
        .query_map(params![now.to_rfc3339(), limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .map(|row| {
            let (id, mode, interval_seconds, next_run_at) = row?;
            Ok(DueSchedule {
                id: FixityScheduleId::parse(id)?,
                mode: FixityMode::parse(&mode)?,
                interval_seconds,
                next_run_at: parse_timestamp(&next_run_at)?,
            })
        })
        .collect();
    rows
}

fn advance_schedule(
    scheduled: DateTime<Utc>,
    interval_seconds: i64,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    if interval_seconds <= 0 {
        return Err(Error::Recovery(
            "fixity schedule contains a non-positive interval".into(),
        ));
    }
    let elapsed = now.signed_duration_since(scheduled).num_seconds().max(0);
    let steps = elapsed
        .checked_div(interval_seconds)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| Error::Operation("fixity schedule advance overflowed".into()))?;
    let seconds = interval_seconds
        .checked_mul(steps)
        .ok_or_else(|| Error::Operation("fixity schedule advance overflowed".into()))?;
    scheduled
        .checked_add_signed(TimeDelta::seconds(seconds))
        .ok_or_else(|| Error::Operation("fixity schedule exceeds the supported date range".into()))
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| Error::Recovery(format!("invalid stored fixity timestamp: {error}")))
}

#[expect(
    clippy::too_many_lines,
    reason = "the ordered fail-closed inspection reads as one safety protocol"
)]
fn inspect_asset(asset: &AssetEvidence, mode: FixityMode) -> (FixityResult, ObservedEvidence) {
    let result = |state, detail| FixityResult {
        asset_id: asset.id.clone(),
        path: asset.absolute_path.clone(),
        state,
        detail,
    };
    if asset.root_state == "offline" {
        return (
            result(
                FixityResultState::Offline,
                Some("library root is offline".into()),
            ),
            ObservedEvidence::default(),
        );
    }
    if asset.verification_state != "verified" {
        return (
            result(
                FixityResultState::Unverified,
                Some(format!(
                    "asset verification state is {}",
                    asset.verification_state
                )),
            ),
            ObservedEvidence::default(),
        );
    }
    let legacy_root = asset
        .absolute_path
        .parent()
        .unwrap_or(asset.absolute_path.as_path());
    let (anchored_root, anchored_relative) = if asset.root_state == "legacy" {
        (
            legacy_root,
            asset
                .absolute_path
                .strip_prefix(legacy_root)
                .unwrap_or(asset.absolute_path.as_path()),
        )
    } else {
        (asset.root_path.as_path(), asset.relative_path.as_path())
    };
    if anchored_root.join(anchored_relative) != asset.absolute_path {
        return (
            result(
                FixityResultState::PolicyDivergent,
                Some("root-relative and compatibility paths disagree".into()),
            ),
            ObservedEvidence::default(),
        );
    }
    let root = match ReadRoot::open(anchored_root) {
        Ok(root) => root,
        Err(error) => {
            return (
                result(FixityResultState::Unreadable, Some(error.to_string())),
                ObservedEvidence::default(),
            );
        }
    };
    let before = match root.entry_metadata(&asset.absolute_path) {
        Ok(metadata) => metadata,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return (
                result(FixityResultState::Missing, None),
                ObservedEvidence::default(),
            );
        }
        Err(error) => {
            return (
                result(FixityResultState::Unreadable, Some(error.to_string())),
                ObservedEvidence::default(),
            );
        }
    };
    let mut observed = ObservedEvidence {
        size: Some(before.len()),
        ..ObservedEvidence::default()
    };
    if asset
        .expected_identity
        .as_deref()
        .is_some_and(|expected| expected != file_identity(&before))
    {
        return (
            result(
                FixityResultState::Replaced,
                Some("filesystem entry identity changed".into()),
            ),
            observed,
        );
    }
    let modified = asset.expected_size.is_some_and(|size| size != before.len())
        || asset.expected_mtime.as_deref().is_some_and(|mtime| {
            before
                .modified()
                .ok()
                .map(chrono::DateTime::<Utc>::from)
                .is_some_and(|actual| actual.to_rfc3339() != mtime)
        });
    let media_diverged = if asset.role == "audio" || asset.expected_media.is_some() {
        let media = match root
            .open_file(&asset.absolute_path)
            .and_then(|file| probe_media_from_file(file, &asset.absolute_path))
        {
            Ok(media) => media,
            Err(error) => {
                return (
                    result(FixityResultState::Corrupt, Some(error.to_string())),
                    observed,
                );
            }
        };
        asset.expected_media.as_deref().is_some_and(|expected| {
            serde_json::from_str::<MediaDescriptor>(expected).map_or(true, |stored| stored != media)
        })
    } else {
        false
    };
    if asset.projection_state != "current" || media_diverged {
        return (
            result(
                FixityResultState::PolicyDivergent,
                Some("media properties or projection policy diverged".into()),
            ),
            observed,
        );
    }
    if mode == FixityMode::Deep {
        match root.open_file(&asset.absolute_path).and_then(digest_reader) {
            Ok(digests) => {
                observed.blake3 = Some(digests.blake3().to_owned());
                observed.sha256 = Some(digests.sha256().to_owned());
                if asset
                    .expected_blake3
                    .as_deref()
                    .is_some_and(|expected| expected != digests.blake3())
                    || asset
                        .expected_sha256
                        .as_deref()
                        .is_some_and(|expected| expected != digests.sha256())
                {
                    return (
                        result(
                            FixityResultState::Modified,
                            Some("whole-file digest changed".into()),
                        ),
                        observed,
                    );
                }
            }
            Err(error) => {
                return (
                    result(FixityResultState::Unreadable, Some(error.to_string())),
                    observed,
                );
            }
        }
        if let Some(expected) = &asset.expected_essence {
            match root
                .open_file(&asset.absolute_path)
                .and_then(|file| decoded_audio_essence_hash_from_file(file, &asset.absolute_path))
            {
                Ok(essence) => {
                    observed.essence = Some(essence.clone());
                    if &essence != expected {
                        return (
                            result(
                                FixityResultState::Corrupt,
                                Some("decoded audio essence changed".into()),
                            ),
                            observed,
                        );
                    }
                }
                Err(error) => {
                    return (
                        result(FixityResultState::Corrupt, Some(error.to_string())),
                        observed,
                    );
                }
            }
        }
    }
    match root.entry_metadata(&asset.absolute_path) {
        Ok(after) if file_identity(&after) == file_identity(&before) => {}
        Ok(_) => {
            return (
                result(
                    FixityResultState::Replaced,
                    Some("entry changed during fixity inspection".into()),
                ),
                observed,
            );
        }
        Err(error) => {
            return (
                result(FixityResultState::Unreadable, Some(error.to_string())),
                observed,
            );
        }
    }
    if modified {
        (
            result(
                FixityResultState::Modified,
                Some("size or modification time changed".into()),
            ),
            observed,
        )
    } else {
        (result(FixityResultState::Ok, None), observed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::db::OperationKind;
    use crate::{Album, AudioFormat, Item};

    fn wav(sample: i16) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&38_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&sample.to_le_bytes());
        bytes
    }

    fn add_item(library: &mut Library, path: &Path, index: u32) -> Result<()> {
        let metadata = std::fs::metadata(path)?;
        let operation = library.create_operation(OperationKind::ImportCopy, &[])?;
        let album = Album {
            id: None,
            album: format!("Album {index}"),
            albumartist: "Artist".into(),
            year: None,
            artpath: None,
            external_id: None,
            added: Utc::now(),
            extended: crate::ExtendedMetadata::default(),
        };
        let item = Item {
            id: None,
            album_id: None,
            path: path.to_path_buf(),
            title: format!("Track {index}"),
            artist: "Artist".into(),
            album: album.album.clone(),
            albumartist: Some("Artist".into()),
            genre: None,
            year: None,
            track: Some(1),
            disc: Some(1),
            format: AudioFormat::Wav,
            bitrate: 128,
            length: 0.001,
            file_size: Some(metadata.len()),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: metadata.modified()?.into(),
            singleton: false,
            extended: crate::ExtendedMetadata::default(),
        };
        library.commit_import(&operation, Some(&album), &[item])?;
        library.complete_operation(&operation)
    }

    #[test]
    fn deep_fixity_is_paged_resumable_and_retains_history() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut library = Library::open_in_memory()?;
        for index in 0..3 {
            let path = temporary.path().join(format!("track-{index}.wav"));
            std::fs::write(&path, wav(index as i16))?;
            add_item(&mut library, &path, index)?;
        }
        let plan = library.plan_fixity(FixityMode::Deep)?;
        assert_eq!(plan.asset_count(), 3);
        assert!(library.run_fixity_page(&plan, 2).is_err());
        library.approve_fixity(&plan)?;
        let first = library.run_fixity_page(&plan, 2)?;
        assert!(!first.complete());
        assert_eq!(first.results().len(), 2);
        let second = library.run_fixity_page(&plan, 2)?;
        assert!(second.complete());
        assert_eq!(second.results().len(), 1);
        assert_eq!(library.fixity_results_page(plan.id(), None, 10)?.len(), 3);
        assert_eq!(
            library.durable_plan(plan.id())?.state(),
            PlanState::Complete
        );
        assert!(library
            .fixity_results_page(plan.id(), None, 10)?
            .iter()
            .all(|result| result.state() == FixityResultState::Ok));
        Ok(())
    }

    #[test]
    fn fixity_distinguishes_replaced_and_supports_cancellation() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("track.wav");
        std::fs::write(&path, wav(1))?;
        let mut library = Library::open_in_memory()?;
        add_item(&mut library, &path, 1)?;
        let original = temporary.path().join("original.wav");
        std::fs::rename(&path, &original)?;
        std::fs::write(&path, wav(1))?;
        let plan = library.plan_fixity(FixityMode::Quick)?;
        library.approve_fixity(&plan)?;
        let progress = library.run_fixity_page(&plan, 10)?;
        assert_eq!(
            progress.results()[0].state(),
            FixityResultState::Replaced,
            "{:?}",
            progress.results()[0]
        );

        let cancelled = library.plan_fixity(FixityMode::Deep)?;
        library.approve_fixity(&cancelled)?;
        library.request_plan_cancellation(cancelled.id())?;
        assert!(!library.run_fixity_page(&cancelled, 1)?.complete());
        assert_eq!(
            library.durable_plan(cancelled.id())?.state(),
            PlanState::Cancelled
        );
        Ok(())
    }

    #[test]
    fn scheduled_fixity_materializes_once_and_preserves_run_history() -> Result<()> {
        let library = Library::open_in_memory()?;
        let now = Utc::now();
        let schedule_id = library.schedule_fixity(
            FixityMode::Deep,
            std::time::Duration::from_secs(3_600),
            now - TimeDelta::hours(2),
        )?;

        let plans = library.plan_due_fixity(now, 10)?;
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].schedule_id(), Some(&schedule_id));
        assert_eq!(
            library.durable_plan(plans[0].id())?.state(),
            PlanState::Approved
        );
        assert!(library.plan_due_fixity(now, 10)?.is_empty());

        let progress = library.run_fixity_page(&plans[0], 10)?;
        assert!(progress.complete());
        let schedules = library.fixity_schedules_page(None, 10)?;
        assert_eq!(schedules.len(), 1);
        assert!(schedules[0].next_run_at() > now);
        assert_eq!(schedules[0].last_failure_count(), Some(0));
        let history = library.scheduled_fixity_history(&schedule_id, None, 10)?;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].plan_id(), plans[0].id());
        assert_eq!(history[0].failures(), 0);
        Ok(())
    }
}
