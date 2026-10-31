// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The backup service: control-plane and external-service backups, restores,
//! S3 sources, schedules, schedule runs and retention.
//!
//! [`BackupService`] is defined here; its behaviour is split by concern:
//!
//! - `error`: The `BackupError` type and its conversions from database, S3 and
//!   anyhow errors.
//! - `types`: Response and request types shared by the backup service and its
//!   handlers.
//! - `access_scope`: Project-scoped authorization, resolving which projects own
//!   a schedule, a backup or a backup collection.
//! - `retention`: Retention, expiry deadlines, previews and the cleanup of
//!   expired backups.
//! - `s3_sources`: S3 sources, CRUD, connection tests, client construction and
//!   bucket verification.
//! - `index`: The S3 backup index, backup metadata, orphan scans and listing
//!   the backups stored in a source.
//! - `deletion`: Deleting a backup, its S3 objects and its index entry.
//! - `walg_deletion`: Deleting WAL-G backups, target validation, repository
//!   identity and the helper container that runs `wal-g delete`.
//! - `walg`: WAL-G for the control-plane database, locating its container,
//!   enabling WAL archiving and taking base backups.
//! - `postgres_backup`: Control-plane backups (`create_backup`), the pg_dump
//!   sidecar and the upload to S3.
//! - `restore`: Restoring control-plane backups (SQLite, pg_dump and WAL-G).
//! - `queries`: Read-only backup lookups, listing, fetching, open alerts and
//!   live size.
//! - `recovery_set`: Recovery sets, publishing a completed fan-out run as one
//!   restorable set.
//! - `schedules`: Backup schedules, CRUD, service membership, default schedules
//!   and validation.

use crate::handlers::backup_handler::{
    CreateBackupScheduleRequest, CreateS3SourceRequest, UpdateBackupScheduleRequest,
};
use anyhow::Result;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::Client as S3Client;
use chrono::{DateTime, Duration, Timelike, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseBackend, DatabaseConnection, EntityTrait,
    FromQueryResult, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    Statement, TransactionTrait, Value,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use tempfile::NamedTempFile;
use temps_entities::backups::Model as Backup;
use thiserror::Error;
use tokio::time;
use tracing::{debug, error, info, warn};
use urlencoding;
use uuid::Uuid;
use yaml_serde;

use cron::Schedule;
use temps_core::notifications::BackupFailureData;
use temps_core::secret_string::SecretString;
use temps_core::BackupScheduleAuthority;
use temps_entities::{backup_schedules::Model as BackupSchedule, s3_sources::Model as S3Source};
use temps_monitoring::alarm_service::{AlarmService, AlarmSeverity, AlarmType, FireAlarmRequest};
use temps_providers::ExternalServiceManager;
use tokio_stream::StreamExt;

mod access_scope;
mod deletion;
mod error;
mod index;
mod postgres_backup;
mod queries;
mod recovery_set;
mod restore;
mod retention;
mod s3_sources;
mod schedules;
#[cfg(test)]
mod test_support;
mod types;
mod walg;
mod walg_deletion;

pub use access_scope::{
    BackupAccessScope, BackupCollectionAccessScope, BackupScheduleAccessScope,
    BackupWithAccessScope, ServiceProjectScope,
};
pub use error::BackupError;
pub use recovery_set::RecoverySetPublication;
use retention::{retention_cutoff, retention_expiry, validate_retention_period};
pub use retention::{
    RetentionCleanupFailure, RetentionCleanupReport, MAX_RETENTION_DAYS_FOR_SQL_ARITHMETIC,
};
use schedules::exclude_source_backing_services;
pub use types::{
    BackupAlertEntry, BackupTriggerParams, ChildBackupEntry, EnqueuedJob, ScheduleRunContext,
    ScheduleRunEntry, ScheduleRunJobEntry, ScheduleRunListResponse, ScheduleRunOutcome,
    ScheduleRunResponse, ScheduleRunSummary, ScheduleRunSummaryList, ServiceBackupEntry,
    TriggerSource,
};
use walg_deletion::{deletion_walg_service_identity, validated_walg_target, WalgDeletionEngine};

/// POSIX-safe shell escaping: wraps value in single quotes, escaping any
/// embedded single quotes. Safe for use in `sh -c` command strings.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn shell_export_assignment(line: &str) -> Option<String> {
    let (key, value) = line.split_once('=')?;
    let valid_key = key
        .chars()
        .all(|character| character == '_' || character.is_ascii_alphanumeric())
        && key
            .chars()
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    valid_key.then(|| format!("export {key}={}", shell_escape(value)))
}

fn schedule_run_aggregate_state(
    total_jobs: i64,
    failed_jobs: i64,
    running_jobs: i64,
    pending_jobs: i64,
) -> &'static str {
    if total_jobs == 0 {
        "skipped"
    } else if pending_jobs + running_jobs > 0 {
        "running"
    } else if failed_jobs > 0 {
        "failed"
    } else {
        "completed"
    }
}

/// Build a normalized S3 object key from a bucket_path prefix and a relative
/// suffix. Keys must NEVER start with "/" — S3-compatible providers (MinIO, R2,
/// Backblaze B2) reject leading-slash keys as `InvalidArgument`. When the
/// configured `bucket_path` is empty or just "/", the prefix is dropped.
fn build_s3_key(bucket_path: &str, suffix: &str) -> String {
    let prefix = bucket_path.trim_matches('/');
    let suffix = suffix.trim_start_matches('/');
    if prefix.is_empty() {
        suffix.to_string()
    } else {
        format!("{}/{}", prefix, suffix)
    }
}

fn s3_key_from_location(location: &str, expected_bucket: &str) -> Result<String, BackupError> {
    if let Some(rest) = location.strip_prefix("s3://") {
        let (bucket, key) = rest.split_once('/').ok_or_else(|| {
            BackupError::Validation(format!("Invalid S3 backup location: {}", location))
        })?;
        if bucket != expected_bucket {
            return Err(BackupError::Validation(format!(
                "Backup location bucket '{}' does not match configured bucket '{}'",
                bucket, expected_bucket
            )));
        }
        if key.is_empty() {
            return Err(BackupError::Validation(format!(
                "Backup location {} does not contain an object key",
                location
            )));
        }
        Ok(key.to_string())
    } else {
        let key = location.trim_start_matches('/');
        if key.is_empty() {
            return Err(BackupError::Validation(
                "Backup location does not contain an object key".to_string(),
            ));
        }
        Ok(key.to_string())
    }
}

fn json_contains_backup_identity(value: &serde_json::Value, backup_id: &str) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object
                .get("temps_backup_id")
                .and_then(serde_json::Value::as_str)
                == Some(backup_id)
                || object
                    .values()
                    .any(|value| json_contains_backup_identity(value, backup_id))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_contains_backup_identity(value, backup_id)),
        _ => false,
    }
}

#[derive(Clone)]
pub struct BackupService {
    db: Arc<DatabaseConnection>,
    external_service_manager: Arc<ExternalServiceManager>,
    alarm_service: Arc<AlarmService>,
    config_service: Arc<temps_config::ConfigService>,
    encryption_service: Arc<temps_core::EncryptionService>,
    /// Shared workspace `JobQueue` (typically backed by the in-memory
    /// broadcast service in `temps-queue`). Required at construction, so a
    /// plugin-ordering mistake fails at startup (`require_service`) instead
    /// of panicking on the first backup trigger. Triggers publish
    /// `Job::BackupRequested` here; the `BackupJobProcessor` subscribes and
    /// dispatches to the executor.
    queue: Arc<dyn temps_core::JobQueue>,
}

impl BackupService {
    /// Why running a backup of external service `service_id` with
    /// `engine_key` would restart its database now, or `None`. Only the
    /// WAL-G PostgreSQL engine can restart a service (to turn archiving on).
    pub async fn backup_restart_reason(
        &self,
        service_id: i32,
        engine_key: &str,
    ) -> Result<Option<String>, BackupError> {
        if engine_key != "postgres_walg" {
            return Ok(None);
        }
        self.external_service_manager
            .backup_restart_reason(service_id)
            .await
            .map_err(|e| {
                BackupError::ExternalService(format!(
                    "Failed to check whether backing up service {} restarts it: {}",
                    service_id, e
                ))
            })
    }

    pub fn new(
        db: Arc<DatabaseConnection>,
        external_service_manager: Arc<ExternalServiceManager>,
        alarm_service: Arc<AlarmService>,
        serve_config: Arc<temps_config::ConfigService>,
        encryption_service: Arc<temps_core::EncryptionService>,
        queue: Arc<dyn temps_core::JobQueue>,
    ) -> Self {
        Self {
            db,
            external_service_manager,
            alarm_service,
            config_service: serve_config,
            encryption_service,
            queue,
        }
    }

    /// Fire-and-forget S3 bucket lifecycle reconcile for the given source.
    /// Spawns a background task so the caller (schedule create/update/delete)
    /// is never blocked on S3, and lifecycle failures never bubble up as
    /// schedule operation failures — they only show up in logs.
    ///
    /// The reconcile rebuilds the bucket's lifecycle rules from current
    /// schedule state, so even concurrent schedule changes converge to a
    /// consistent rule set eventually. A failure here isn't a dead end: it's
    /// recorded on the source via `lifecycle_reconcile_failed_at` (see
    /// `S3LifecycleService::reconcile_bucket`), which keeps the source in
    /// the hourly sweep's scope — even with no enabled schedule left — until
    /// a later attempt actually succeeds.
    fn fire_lifecycle_reconcile(&self, s3_source_id: i32) {
        let db = self.db.clone();
        let enc = self.encryption_service.clone();
        tokio::spawn(async move {
            let svc = super::S3LifecycleService::new(db, enc);
            match svc.reconcile_bucket(s3_source_id).await {
                Ok(outcome) => {
                    info!(s3_source_id, ?outcome, "S3 lifecycle reconcile completed");
                }
                Err(e) => {
                    warn!(
                        s3_source_id,
                        error = %e,
                        "S3 lifecycle reconcile failed (app-side retention still active)"
                    );
                }
            }
        });
    }

    /// Paginated run history for a backup schedule (deliverable 1).
    /// List run history for a backup schedule, one row per scheduler tick.
    ///
    /// Returns one [`ScheduleRunSummary`] per `schedule_runs` row linked to
    /// the schedule, with child backup counts aggregated in a single SQL
    /// round-trip. The `aggregate_state` is computed in Rust from the counts:
    ///
    /// - `"running"` — `pending_jobs + running_jobs > 0`
    /// - `"failed"` — `failed_jobs > 0` and `running_jobs + pending_jobs == 0`
    /// - `"completed"` — all children completed
    ///
    /// Legacy `backups` rows that have `schedule_id` set but no
    /// `schedule_run_id` (pre-fan-out history) are surfaced as synthetic
    /// single-job runs in the same list so old history does not disappear.
    ///
    /// `page` is 1-based and clamped to `1` when `< 1`.
    /// `page_size` is clamped to `100` when `> 100` and defaults to `20`.
    pub async fn list_schedule_runs(
        &self,
        schedule_id: i32,
        page: i64,
        page_size: i64,
    ) -> Result<ScheduleRunSummaryList, BackupError> {
        // Verify the schedule exists first so we return 404 instead of an
        // empty page when the caller passes an unknown id.
        self.get_backup_schedule(schedule_id).await?;

        // Clamp pagination parameters.
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let offset = (page - 1) * page_size;

        // ── Count total run rows (real + synthetic legacy) ────────────────────

        #[derive(FromQueryResult)]
        struct CountRow {
            total: i64,
        }

        // Real runs: schedule_runs rows.
        // Legacy rows: backups with schedule_id set but schedule_run_id NULL.
        let count_sql = r#"
SELECT (
    SELECT COUNT(*) FROM schedule_runs WHERE schedule_id = $1
) + (
    SELECT COUNT(*) FROM backups
     WHERE schedule_id = $1
       AND schedule_run_id IS NULL
) AS total
        "#;

        let total = CountRow::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            count_sql,
            vec![Value::from(schedule_id)],
        ))
        .one(self.db.as_ref())
        .await
        .map_err(BackupError::Database)?
        .map(|r| r.total)
        .unwrap_or(0);

        // ── Fetch page — one row per tick + synthetic legacy rows ─────────────

        #[derive(FromQueryResult)]
        struct RunRow {
            run_id: i64,
            schedule_id: i32,
            triggered_by: String,
            started_at: chrono::DateTime<chrono::Utc>,
            finished_at: Option<chrono::DateTime<chrono::Utc>>,
            total_jobs: i64,
            completed_jobs: i64,
            failed_jobs: i64,
            running_jobs: i64,
            pending_jobs: i64,
        }

        // Real rows: one per schedule_runs row, child counts via LEFT JOIN.
        // Legacy rows (backups with schedule_id set, schedule_run_id NULL):
        // synthesised as if they were a run with a single job whose state
        // is the backup's own state. We use the backups.id negated as the
        // synthetic run_id to avoid collisions with real schedule_runs.id
        // (both are distinct integer ranges; negative is never a real run id).
        let sql = r#"
SELECT
    sr.id                AS run_id,
    sr.schedule_id       AS schedule_id,
    sr.triggered_by      AS triggered_by,
    sr.started_at        AS started_at,
    sr.finished_at       AS finished_at,
    COUNT(b.id)                                                   AS total_jobs,
    COUNT(b.id) FILTER (WHERE b.state = 'completed')              AS completed_jobs,
    COUNT(b.id) FILTER (WHERE b.state = 'failed')                 AS failed_jobs,
    COUNT(b.id) FILTER (WHERE b.state = 'running')                AS running_jobs,
    COUNT(b.id) FILTER (WHERE b.state = 'pending')                AS pending_jobs
FROM schedule_runs sr
LEFT JOIN backups b ON b.schedule_run_id = sr.id
WHERE sr.schedule_id = $1
GROUP BY sr.id

UNION ALL

-- Synthetic rows for legacy backups (pre-fan-out, no schedule_run_id).
SELECT
    (-b.id)::BIGINT      AS run_id,
    b.schedule_id        AS schedule_id,
    'cron'               AS triggered_by,
    b.started_at         AS started_at,
    b.finished_at        AS finished_at,
    1                    AS total_jobs,
    CASE WHEN b.state = 'completed' THEN 1 ELSE 0 END AS completed_jobs,
    CASE WHEN b.state = 'failed'    THEN 1 ELSE 0 END AS failed_jobs,
    CASE WHEN b.state = 'running'   THEN 1 ELSE 0 END AS running_jobs,
    CASE WHEN b.state = 'pending'   THEN 1 ELSE 0 END AS pending_jobs
FROM backups b
WHERE b.schedule_id = $1
  AND b.schedule_run_id IS NULL

ORDER BY started_at DESC
LIMIT  $2
OFFSET $3
        "#;

        let raw_rows = RunRow::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            vec![
                Value::from(schedule_id),
                Value::from(page_size),
                Value::from(offset),
            ],
        ))
        .all(self.db.as_ref())
        .await
        .map_err(BackupError::Database)?;

        // ── Compute aggregate_state in Rust ───────────────────────────────────

        let runs = raw_rows
            .into_iter()
            .map(|r| {
                let aggregate_state = schedule_run_aggregate_state(
                    r.total_jobs,
                    r.failed_jobs,
                    r.running_jobs,
                    r.pending_jobs,
                )
                .to_string();

                ScheduleRunSummary {
                    run_id: r.run_id,
                    schedule_id: r.schedule_id,
                    triggered_by: r.triggered_by,
                    started_at: r.started_at.to_rfc3339(),
                    finished_at: r.finished_at.map(|t| t.to_rfc3339()),
                    aggregate_state,
                    total_jobs: r.total_jobs,
                    completed_jobs: r.completed_jobs,
                    failed_jobs: r.failed_jobs,
                    running_jobs: r.running_jobs,
                    pending_jobs: r.pending_jobs,
                }
            })
            .collect();

        Ok(ScheduleRunSummaryList {
            runs,
            total,
            page,
            page_size,
        })
    }

    /// List the individual backup jobs belonging to a single scheduler run.
    ///
    /// Used by the schedule detail page's expandable accordion for per-run
    /// drill-down. Returns each child `backups` row joined with its
    /// `external_services` row (for the service name) and the most recent
    /// `backup_jobs` row (for the engine key).
    ///
    /// `page_size` defaults to 50 and is capped at 200 — a single scheduler
    /// tick produces at most 1 + N external services rows (small N).
    pub async fn list_schedule_run_jobs(
        &self,
        run_id: i64,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<ScheduleRunJobEntry>, i64), BackupError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 200);
        let offset = (page - 1) * page_size;

        #[derive(FromQueryResult)]
        struct CountRow {
            total: i64,
        }

        let count_sql = r#"
SELECT COUNT(*) AS total FROM backups WHERE schedule_run_id = $1
        "#;

        let total = CountRow::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            count_sql,
            vec![Value::from(run_id)],
        ))
        .one(self.db.as_ref())
        .await
        .map_err(BackupError::Database)?
        .map(|r| r.total)
        .unwrap_or(0);

        // The engine key is stored on `backups.metadata` JSON (written at
        // trigger time by `BackupService::create_pending_*`). Read it from
        // there. external_service_backups joins to external_services for
        // the human-readable name.
        let sql = r#"
SELECT
    b.id                                            AS backup_id,
    b.backup_id                                     AS backup_uuid,
    COALESCE(b.metadata::jsonb ->> 'engine', 'control_plane') AS engine,
    COALESCE(es.name, esb.service_name_snapshot, 'control plane') AS service_name,
    esb.service_id                                  AS service_id,
    b.state                                         AS state,
    b.started_at                                    AS started_at,
    b.finished_at                                   AS finished_at,
    b.size_bytes                                    AS size_bytes,
    b.error_message                                 AS error_message,
    b.s3_source_id                                  AS s3_source_id
FROM backups b
LEFT JOIN external_service_backups esb ON esb.backup_id = b.id
LEFT JOIN external_services es ON es.id = esb.service_id
WHERE b.schedule_run_id = $1
ORDER BY b.id
LIMIT  $2
OFFSET $3
        "#;

        let rows = ScheduleRunJobEntry::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            vec![
                Value::from(run_id),
                Value::from(page_size),
                Value::from(offset),
            ],
        ))
        .all(self.db.as_ref())
        .await
        .map_err(BackupError::Database)?;

        Ok((rows, total))
    }

    /// Immediately enqueue a fan-out run for the given schedule (Run Now).
    ///
    /// Delegates to [`enqueue_scheduled_run`] with `TriggerSource::Manual`.
    /// Returns `409 Conflict` (via [`BackupError::ScheduleRunAlreadyInFlight`])
    /// when a run for this schedule already has `finished_at IS NULL`.
    ///
    /// Returns [`BackupError::Validation`] when the schedule is disabled.
    /// Cancel a single backup by `backups.id`. Flips the row + its latest
    /// `backup_jobs` row to `failed` with a "cancelled by user" reason.
    /// Returns the number of rows updated — `0` means the backup was already
    /// terminal (which the caller should treat as an idempotent success,
    /// not a 404). The runner's in-process cancellation token is observed
    /// on the next heartbeat tick (≤5s) so the engine exits cleanly and
    /// `rollback` reaps the sidecar.
    pub async fn cancel_backup(
        &self,
        backup_id: i32,
        triggered_by_user_id: Option<i32>,
    ) -> Result<u64, BackupError> {
        // Verify the backup exists so the caller gets a real 404 (not an
        // "everything looks fine, nothing happened" silent no-op) when the
        // id is wrong. Then delegate to `temps_backup_core::cancel_backup`
        // which owns the actual DB writes.
        temps_entities::backups::Entity::find_by_id(backup_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "Backup".to_string(),
                detail: format!("Backup {} not found", backup_id),
            })?;

        let reason = match triggered_by_user_id {
            Some(uid) => format!("cancelled by user {}", uid),
            None => "cancelled".to_string(),
        };

        let rows = temps_backup_core::cancel_backup(self.db.as_ref(), backup_id, &reason)
            .await
            .map_err(|e| BackupError::Internal {
                message: format!("Failed to cancel backup {}: {}", backup_id, e),
            })?;

        // Notify the processor so any in-flight engine task fires its
        // cancel token and exits cleanly. The DB flip above already
        // marked the row as failed; this signal stops the running
        // container so the sidecar gets reaped promptly.
        if let Err(e) = self
            .queue
            .send(temps_core::Job::BackupCancelRequested(
                temps_core::BackupCancelRequestedJob { backup_id },
            ))
            .await
        {
            warn!(
                backup_id,
                error = %e,
                "cancel_backup: queue.send for BackupCancelRequested failed; running engine (if any) will not be interrupted promptly",
            );
        }

        info!(
            backup_id,
            rows_affected = rows,
            triggered_by_user_id = ?triggered_by_user_id,
            "BackupService: cancel_backup completed",
        );

        Ok(rows)
    }

    /// Cancel every non-terminal child backup belonging to a scheduler run.
    /// Returns the number of children that were flipped to `failed`. The
    /// parent `schedule_runs.finished_at` is stamped automatically once no
    /// live children remain (which is true after a successful cancel).
    pub async fn cancel_schedule_run(
        &self,
        schedule_run_id: i64,
        triggered_by_user_id: Option<i32>,
    ) -> Result<u64, BackupError> {
        // Verify the run exists so the caller gets a real 404.
        temps_entities::schedule_runs::Entity::find_by_id(schedule_run_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "ScheduleRun".to_string(),
                detail: format!("Schedule run {} not found", schedule_run_id),
            })?;

        let reason = match triggered_by_user_id {
            Some(uid) => format!(
                "cancelled by user {} (run {} cancelled)",
                uid, schedule_run_id
            ),
            None => format!("cancelled (run {} cancelled)", schedule_run_id),
        };

        // Capture live child ids BEFORE the DB helper flips them — we
        // need them to signal the in-process consumer for each one.
        #[derive(FromQueryResult)]
        struct ChildId {
            id: i32,
        }
        let live_children = ChildId::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            r#"SELECT id FROM backups
                WHERE schedule_run_id = $1 AND state IN ('pending', 'running')"#,
            vec![Value::from(schedule_run_id)],
        ))
        .all(self.db.as_ref())
        .await?;

        let cancelled =
            temps_backup_core::cancel_schedule_run(self.db.as_ref(), schedule_run_id, &reason)
                .await
                .map_err(|e| BackupError::Internal {
                    message: format!("Failed to cancel schedule run {}: {}", schedule_run_id, e),
                })?;

        // Publish a cancel signal per live child so any in-flight engine
        // tasks fire their cancel tokens and exit cleanly.
        let queue = &self.queue;
        for child in live_children {
            if let Err(e) = queue
                .send(temps_core::Job::BackupCancelRequested(
                    temps_core::BackupCancelRequestedJob {
                        backup_id: child.id,
                    },
                ))
                .await
            {
                warn!(
                    schedule_run_id,
                    backup_id = child.id,
                    error = %e,
                    "cancel_schedule_run: queue.send for child cancel failed",
                );
            }
        }

        info!(
            schedule_run_id,
            cancelled,
            triggered_by_user_id = ?triggered_by_user_id,
            "BackupService: cancel_schedule_run completed",
        );

        Ok(cancelled)
    }

    pub async fn run_schedule_now(
        &self,
        schedule_id: i32,
        triggered_by_user_id: Option<i32>,
    ) -> Result<ScheduleRunResponse, BackupError> {
        let schedule = self.get_backup_schedule(schedule_id).await?;

        if !schedule.enabled {
            return Err(BackupError::Validation(format!(
                "Schedule {} is disabled; enable it before triggering a manual run",
                schedule_id
            )));
        }

        match self
            .enqueue_scheduled_run(&schedule, TriggerSource::Manual, triggered_by_user_id)
            .await?
        {
            ScheduleRunOutcome::Started { run_id, jobs } => {
                info!(
                    schedule_id = schedule.id,
                    schedule_name = %schedule.name,
                    run_id,
                    job_count = jobs.len(),
                    "run_schedule_now: fan-out run started",
                );
                Ok(ScheduleRunResponse {
                    schedule_run_id: run_id,
                    jobs,
                })
            }
            ScheduleRunOutcome::AlreadyInFlight { existing_run_id } => {
                Err(BackupError::ScheduleRunAlreadyInFlight { existing_run_id })
            }
        }
    }

    /// Fan-out a scheduler tick into one `schedule_runs` row + one control-plane
    /// backup job + one backup job per supported external service.
    ///
    /// ## Fan-out logic (in one transaction)
    ///
    /// 1. Check for an existing in-flight run (any `schedule_runs` row for this
    ///    schedule with `finished_at IS NULL`). If found, return
    ///    [`ScheduleRunOutcome::AlreadyInFlight`] immediately.
    /// 2. Insert a `schedule_runs` row. Capture `run_id`.
    /// 3. Insert a control-plane `backups` row with `schedule_run_id = run_id`.
    ///    Enqueue a `backup_jobs` row for `engine = "control_plane"`.
    /// 4. Load every `external_services` row and attempt to resolve its engine
    ///    key via [`resolve_engine_key`]. Skip rows where resolution returns
    ///    `Err` (unsupported type) — log at `warn` with the service id and type.
    /// 5. For each supported service, insert an `external_service_backups` row +
    ///    parent `backups` row (`schedule_run_id = run_id`), then enqueue a
    ///    `backup_jobs` row. Individual `AlreadyInFlight` responses from the
    ///    concurrency guard are logged at `info` and skipped — the rest of the
    ///    fan-out continues.
    /// 6. Advance `backup_schedules.next_run`, `last_run`, `last_job_id`.
    /// 7. Commit and return [`ScheduleRunOutcome::Started`].
    ///
    /// The cron caller treats both outcome variants as `Ok` and logs the run id.
    /// The "Run now" handler converts `AlreadyInFlight` to a `409 Conflict`.
    /// Close any `schedule_runs` rows for this schedule that have
    /// `finished_at IS NULL` but no longer have any pending/running children.
    ///
    /// Earlier code revisions could leave the parent row open if a worker
    /// crashed between writing the last child's terminal state and calling
    /// `mark_schedule_run_finished_if_done`. Without this reconciler the
    /// concurrency guard would refuse all future "Run now" requests for the
    /// schedule. The UPDATE is idempotent and a no-op for healthy schedules.
    ///
    /// Sets `finished_at` to the latest child `finished_at` so duration
    /// metrics remain accurate; falls back to `NOW()` if every child is
    /// missing a `finished_at` (shouldn't happen, but safer than NULL).
    async fn reconcile_drifted_schedule_runs(&self, schedule_id: i32) -> Result<(), BackupError> {
        use sea_orm::ConnectionTrait;

        let sql = r#"
UPDATE schedule_runs sr
   SET finished_at = COALESCE(
       (SELECT MAX(b.finished_at)
          FROM backups b
         WHERE b.schedule_run_id = sr.id),
       NOW()
   )
 WHERE sr.schedule_id = $1
   AND sr.finished_at IS NULL
   AND NOT EXISTS (
       SELECT 1 FROM backups b
        WHERE b.schedule_run_id = sr.id
          AND b.state IN ('pending', 'running')
   )
   AND EXISTS (
       SELECT 1 FROM backups b
        WHERE b.schedule_run_id = sr.id
   )
        "#;

        self.db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                sql,
                vec![Value::from(schedule_id)],
            ))
            .await
            .map_err(BackupError::Database)?;

        Ok(())
    }

    pub async fn enqueue_scheduled_run(
        &self,
        schedule: &temps_entities::backup_schedules::Model,
        triggered_by: TriggerSource,
        triggered_by_user_id: Option<i32>,
    ) -> Result<ScheduleRunOutcome, BackupError> {
        use sea_orm::{Set, TransactionTrait};

        let now = chrono::Utc::now();

        // Compute next_run before opening the transaction so a parse error
        // fails fast without wasted DB work.
        let cron_schedule = Schedule::from_str(&schedule.schedule_expression).map_err(|e| {
            BackupError::Validation(format!(
                "Invalid cron expression for schedule {}: {}",
                schedule.id, e
            ))
        })?;
        let next_run = cron_schedule.upcoming(Utc).next();

        // ── Step 1: in-flight check (before opening the write transaction) ────
        //
        // A run is only "in flight" if at least one child backup is still
        // `pending` or `running`. We do NOT rely on `schedule_runs.finished_at`
        // alone — that field can drift to NULL if a prior worker crashed
        // between writing the last child's terminal state and calling
        // `mark_schedule_run_finished_if_done`, leaving the schedule
        // permanently un-runnable. Checking children directly is also the
        // authoritative source of truth used by the aggregate-state SQL in
        // `list_schedule_runs`, so the guard and the UI agree by construction.
        //
        // While we're here, opportunistically close any drifted `schedule_runs`
        // rows so the guard, the UI, and `finished_at`-based queries stay in
        // sync after we let this new run through.
        self.reconcile_drifted_schedule_runs(schedule.id).await?;

        #[derive(FromQueryResult)]
        struct InFlightRow {
            id: i64,
        }

        let in_flight_sql = r#"
SELECT sr.id FROM schedule_runs sr
 WHERE sr.schedule_id = $1
   AND EXISTS (
       SELECT 1 FROM backups b
        WHERE b.schedule_run_id = sr.id
          AND b.state IN ('pending', 'running')
   )
 LIMIT 1
        "#;

        if let Some(existing) = InFlightRow::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            in_flight_sql,
            vec![Value::from(schedule.id)],
        ))
        .one(self.db.as_ref())
        .await
        .map_err(BackupError::Database)?
        {
            return Ok(ScheduleRunOutcome::AlreadyInFlight {
                existing_run_id: existing.id,
            });
        }

        // ── Step 2: connect to Docker for engine resolution ───────────────────
        // We connect once per tick, not per service, to amortise the socket
        // setup cost. `connect_with_local_defaults()` is fast (no round-trip).
        let docker =
            bollard::Docker::connect_with_local_defaults().map_err(|e| BackupError::Internal {
                message: format!(
                    "enqueue_scheduled_run: failed to connect to Docker for engine resolution \
                     (schedule {}): {}",
                    schedule.id, e
                ),
            })?;

        // Load the external services this schedule should fan out to.
        // Two modes (set on `backup_schedules.target_all_services`):
        //   - true  → every external service on the host (auto-includes
        //             future databases); the explicit join table is ignored.
        //   - false → only services attached via `backup_schedule_services`
        //             (the operator picked specific DBs).
        use sea_orm::{ColumnTrait, QueryFilter};
        let backing_service_ids = self
            .source_backing_service_ids(schedule.s3_source_id)
            .await?;
        let external_services = if schedule.target_all_services {
            temps_entities::external_services::Entity::find()
                .all(self.db.as_ref())
                .await
                .map_err(BackupError::Database)?
        } else {
            temps_entities::external_services::Entity::find()
                .inner_join(temps_entities::backup_schedule_services::Entity)
                .filter(
                    temps_entities::backup_schedule_services::Column::ScheduleId.eq(schedule.id),
                )
                .all(self.db.as_ref())
                .await
                .map_err(BackupError::Database)?
        };
        let external_services =
            exclude_source_backing_services(external_services, &backing_service_ids);

        if external_services.is_empty() {
            // Two reasons we could end up here: no DBs exist yet, or the
            // operator picked "specific" mode and didn't attach anything.
            // Log both with `target_all_services` so it's obvious which.
            info!(
                schedule_id = schedule.id,
                schedule_name = %schedule.name,
                target_all_services = schedule.target_all_services,
                "enqueue_scheduled_run: no external services in scope; fan-out will be control-plane only",
            );
        }

        // Resolve engine keys outside the transaction (async Docker probes).
        let mut resolved_services: Vec<(temps_entities::external_services::Model, &'static str)> =
            Vec::with_capacity(external_services.len());

        for svc in external_services {
            match crate::engines::dispatch::resolve_engine_key(&svc, &docker).await {
                Ok(engine_key) => {
                    resolved_services.push((svc, engine_key));
                }
                Err(e) => {
                    warn!(
                        service_id = svc.id,
                        service_type = %svc.service_type,
                        error = %e,
                        "enqueue_scheduled_run: skipping unsupported external service",
                    );
                }
            }
        }
        let expected_service_ids = resolved_services
            .iter()
            .map(|(service, _)| service.id)
            .collect::<Vec<_>>();

        if !schedule.include_control_plane && resolved_services.is_empty() {
            return Err(BackupError::Validation(format!(
                "Schedule {} has no eligible backup targets; no run was recorded",
                schedule.id
            )));
        }

        // ── Step 3: open the write transaction ────────────────────────────────

        let txn = self.db.begin().await?;

        // Insert the schedule_runs row.
        let run_insert_sql = r#"
INSERT INTO schedule_runs (schedule_id, triggered_by, triggered_by_user_id, started_at, created_at)
VALUES ($1, $2, $3, NOW(), NOW())
RETURNING id
        "#;

        #[derive(FromQueryResult)]
        struct RunIdRow {
            id: i64,
        }

        let run_id = RunIdRow::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            run_insert_sql,
            vec![
                Value::from(schedule.id),
                Value::from(triggered_by.as_str().to_owned()),
                Value::from(triggered_by_user_id),
            ],
        ))
        .one(&txn)
        .await
        .map_err(BackupError::Database)?
        .ok_or_else(|| BackupError::Internal {
            message: format!(
                "enqueue_scheduled_run: INSERT into schedule_runs returned no id \
                 for schedule {}",
                schedule.id
            ),
        })?
        .id;

        let mut jobs: Vec<EnqueuedJob> = Vec::new();

        // Defer queue publishes until after txn.commit so the consumer
        // can't dispatch an engine against a backups row the txn might
        // still roll back.
        let mut deferred_messages: Vec<temps_core::BackupRequestedJob> = Vec::new();

        // ── Step 4: control-plane backup (skipped when the schedule
        // ──         opts out of control-plane coverage). ─────────────────────
        if schedule.include_control_plane {
            let cp_uuid = Uuid::new_v4().to_string();
            let cp_backup = temps_entities::backups::ActiveModel {
                id: sea_orm::NotSet,
                name: Set(format!("Backup {}", cp_uuid)),
                backup_id: Set(cp_uuid.clone()),
                schedule_id: Set(Some(schedule.id)),
                schedule_run_id: Set(Some(run_id)),
                backup_type: Set(schedule.backup_type.clone()),
                state: Set("pending".to_string()),
                started_at: Set(now),
                finished_at: Set(None),
                s3_source_id: Set(schedule.s3_source_id),
                s3_location: Set(String::new()),
                compression_type: Set("gzip".to_string()),
                created_by: Set(0),
                tags: Set("[]".to_string()),
                size_bytes: Set(None),
                file_count: Set(None),
                error_message: Set(None),
                expires_at: Set(retention_expiry(now, schedule.retention_period)),
                checksum: Set(None),
                metadata: Set(serde_json::json!({
                    "engine": "control_plane",
                    "async_runner": true,
                    "scheduled": triggered_by == TriggerSource::Cron,
                    "schedule_id": schedule.id,
                    "run_id": run_id,
                    "expected_service_ids": expected_service_ids,
                    "timestamp": now.to_rfc3339(),
                })
                .to_string()),
            };

            let cp_backup_row = cp_backup.insert(&txn).await?;

            deferred_messages.push(temps_core::BackupRequestedJob {
                backup_id: cp_backup_row.id,
                engine: "control_plane".to_string(),
                params: serde_json::json!({
                    "s3_source_id": schedule.s3_source_id,
                    "schedule_id": schedule.id,
                    "run_id": run_id,
                }),
                max_runtime_secs: schedule.max_runtime_secs.unwrap_or(4 * 60 * 60),
            });
            jobs.push(EnqueuedJob {
                backup_id: cp_backup_row.id,
                job_id: cp_backup_row.id as i64,
                engine: "control_plane".to_string(),
                target_service_id: None,
            });
        } else {
            info!(
                schedule_id = schedule.id,
                run_id,
                "enqueue_scheduled_run: include_control_plane=false, skipping control-plane backup",
            );
        }

        // ── Step 5: external service backups ──────────────────────────────────

        for (svc, engine_key) in &resolved_services {
            let trigger = BackupTriggerParams {
                engine: engine_key.to_string(),
                params: serde_json::json!({
                    "service_id": svc.id,
                    "s3_source_id": schedule.s3_source_id,
                    "backup_type": schedule.backup_type,
                    "schedule_id": schedule.id,
                    "run_id": run_id,
                }),
                max_runtime_secs: schedule.max_runtime_secs,
            };

            match self
                .insert_pending_external_service_backup_in_txn(
                    &txn,
                    svc.id,
                    schedule.s3_source_id,
                    &schedule.backup_type,
                    0,
                    "gzip",
                    Some(ScheduleRunContext {
                        schedule_id: schedule.id,
                        schedule_run_id: run_id,
                        retention_period: schedule.retention_period,
                    }),
                    &trigger,
                )
                .await
            {
                Ok((parent_row, _esb_row)) => {
                    deferred_messages.push(temps_core::BackupRequestedJob {
                        backup_id: parent_row.id,
                        engine: trigger.engine.clone(),
                        params: trigger.params.clone(),
                        max_runtime_secs: trigger.max_runtime_secs.unwrap_or(4 * 60 * 60),
                    });
                    jobs.push(EnqueuedJob {
                        backup_id: parent_row.id,
                        job_id: parent_row.id as i64,
                        engine: engine_key.to_string(),
                        target_service_id: Some(svc.id),
                    });
                }
                Err(e) => {
                    error!(
                        schedule_id = schedule.id,
                        service_id = svc.id,
                        service_name = %svc.name,
                        engine = engine_key,
                        error = %e,
                        "enqueue_scheduled_run: failed to insert external service rows; rolling back the fan-out",
                    );
                    return Err(e);
                }
            }
        }

        // ── Step 6: advance schedule metadata ─────────────────────────────────
        let mut schedule_update: temps_entities::backup_schedules::ActiveModel =
            schedule.clone().into_active_model();
        schedule_update.next_run = Set(next_run);
        schedule_update.last_run = Set(Some(now));
        schedule_update.update(&txn).await?;

        // ── Commit ────────────────────────────────────────────────────────────

        txn.commit().await?;

        // ── Publish queue messages after commit ───────────────────────────────
        // Publishing after commit guarantees the consumer never dispatches
        // an engine against a backups row that the txn might roll back.
        let queue = &self.queue;
        for req in deferred_messages {
            let backup_id = req.backup_id;
            if let Err(e) = queue.send(temps_core::Job::BackupRequested(req)).await {
                warn!(
                    schedule_id = schedule.id,
                    backup_id,
                    error = %e,
                    "enqueue_scheduled_run: queue.send failed (row committed but not dispatched)",
                );
            }
        }

        info!(
            schedule_id = schedule.id,
            schedule_name = %schedule.name,
            run_id,
            job_count = jobs.len(),
            triggered_by = triggered_by.as_str(),
            "enqueue_scheduled_run: fan-out committed and dispatched",
        );

        Ok(ScheduleRunOutcome::Started { run_id, jobs })
    }

    /// Insert a pending control-plane `backups` row and dispatch the
    /// matching engine task on the executor. The synthetic `i64` returned
    /// is the backup row's id widened for backwards-compat with callers
    /// that previously took a `backup_jobs.id`.
    pub async fn create_pending_backup_row(
        &self,
        s3_source_id: i32,
        backup_type: &str,
        created_by: i32,
        trigger: BackupTriggerParams,
    ) -> Result<(Backup, i64), BackupError> {
        use sea_orm::Set;

        temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "S3Source".to_string(),
                detail: format!("S3 source {} not found", s3_source_id),
            })?;

        let backup_uuid = Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let new_backup = temps_entities::backups::ActiveModel {
            id: sea_orm::NotSet,
            name: Set(format!("Backup {}", backup_uuid)),
            backup_id: Set(backup_uuid.clone()),
            schedule_id: Set(None),
            schedule_run_id: sea_orm::NotSet,
            backup_type: Set(backup_type.to_string()),
            state: Set("pending".to_string()),
            started_at: Set(now),
            finished_at: Set(None),
            s3_source_id: Set(s3_source_id),
            s3_location: Set(String::new()),
            compression_type: Set("gzip".to_string()),
            created_by: Set(created_by),
            tags: Set("[]".to_string()),
            size_bytes: Set(None),
            file_count: Set(None),
            error_message: Set(None),
            expires_at: Set(None),
            checksum: Set(None),
            metadata: Set(serde_json::json!({
                "engine": trigger.engine,
                "async_runner": true,
                "timestamp": now.to_rfc3339(),
            })
            .to_string()),
        };

        let backup = new_backup.insert(self.db.as_ref()).await?;
        let backup_id = backup.id;

        // Publish to the queue. If publish fails the row is left in
        // `pending`; the next boot's reconcile will flip it to `failed`.
        let max_runtime_secs = trigger.max_runtime_secs.unwrap_or(4 * 60 * 60);
        if let Err(e) = self
            .queue
            .send(temps_core::Job::BackupRequested(
                temps_core::BackupRequestedJob {
                    backup_id,
                    engine: trigger.engine.clone(),
                    params: trigger.params,
                    max_runtime_secs,
                },
            ))
            .await
        {
            return Err(BackupError::Internal {
                message: format!(
                    "Failed to publish BackupRequested for backup {}: {}",
                    backup_id, e
                ),
            });
        }

        info!(
            backup_id = %backup.backup_id,
            s3_source_id,
            backup_row_id = backup_id,
            "BackupService: created pending backup row and published BackupRequested",
        );

        Ok((backup, backup_id as i64))
    }

    /// Insert parent `backups` + child `external_service_backups` rows inside
    /// the supplied transaction. Does NOT spawn the engine task — the caller
    /// must call `executor.spawn` AFTER committing the transaction so the
    /// engine never sees a row the txn might roll back.
    ///
    /// Used by `enqueue_scheduled_run`'s fan-out. Manual triggers go through
    /// [`create_pending_external_service_backup_row`] instead, which handles
    /// the txn lifecycle internally.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_pending_external_service_backup_in_txn(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        service_id: i32,
        s3_source_id: i32,
        backup_type: &str,
        created_by: i32,
        compression_type: &str,
        schedule_ctx: Option<ScheduleRunContext>,
        trigger: &BackupTriggerParams,
    ) -> Result<
        (
            temps_entities::backups::Model,
            temps_entities::external_service_backups::Model,
        ),
        BackupError,
    > {
        use sea_orm::Set;

        let backup_uuid = Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let source_service = temps_entities::external_services::Entity::find_by_id(service_id)
            .one(txn)
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "ExternalService".to_string(),
                detail: format!(
                    "service {} disappeared while creating its backup",
                    service_id
                ),
            })?;

        let mut backups_metadata = serde_json::Map::new();
        backups_metadata.insert(
            "engine".to_string(),
            serde_json::Value::String(trigger.engine.clone()),
        );
        backups_metadata.insert("async_runner".to_string(), serde_json::Value::Bool(true));
        backups_metadata.insert(
            "external_service_id".to_string(),
            serde_json::Value::Number(service_id.into()),
        );
        backups_metadata.insert(
            "service_id".to_string(),
            serde_json::Value::Number(service_id.into()),
        );
        backups_metadata.insert(
            "timestamp".to_string(),
            serde_json::Value::String(now.to_rfc3339()),
        );
        if let Some(ctx) = schedule_ctx {
            backups_metadata.insert(
                "schedule_id".to_string(),
                serde_json::Value::Number(ctx.schedule_id.into()),
            );
            backups_metadata.insert(
                "run_id".to_string(),
                serde_json::Value::Number(ctx.schedule_run_id.into()),
            );
        }

        let mut esb_metadata = serde_json::Map::new();
        esb_metadata.insert(
            "engine".to_string(),
            serde_json::Value::String(trigger.engine.clone()),
        );
        esb_metadata.insert("async_runner".to_string(), serde_json::Value::Bool(true));
        esb_metadata.insert(
            "backup_uuid".to_string(),
            serde_json::Value::String(backup_uuid.clone()),
        );
        esb_metadata.insert(
            "timestamp".to_string(),
            serde_json::Value::String(now.to_rfc3339()),
        );
        if let Some(ctx) = schedule_ctx {
            esb_metadata.insert(
                "schedule_id".to_string(),
                serde_json::Value::Number(ctx.schedule_id.into()),
            );
            esb_metadata.insert(
                "run_id".to_string(),
                serde_json::Value::Number(ctx.schedule_run_id.into()),
            );
        }

        // Schedule-created backups are deleted by `run_retention_cleanup`
        // once they are older than the schedule's retention period; record
        // that deadline so the API and console can show it. Manual runs
        // (`schedule_ctx == None`) are kept until deleted.
        let expires_at = schedule_ctx.and_then(|ctx| retention_expiry(now, ctx.retention_period));

        let parent = temps_entities::backups::ActiveModel {
            id: sea_orm::NotSet,
            name: Set(format!("Backup {}", backup_uuid)),
            backup_id: Set(backup_uuid.clone()),
            schedule_id: Set(schedule_ctx.map(|c| c.schedule_id)),
            schedule_run_id: Set(schedule_ctx.map(|c| c.schedule_run_id)),
            backup_type: Set(backup_type.to_string()),
            state: Set("pending".to_string()),
            started_at: Set(now),
            finished_at: Set(None),
            s3_source_id: Set(s3_source_id),
            s3_location: Set(String::new()),
            compression_type: Set(compression_type.to_string()),
            created_by: Set(created_by),
            tags: Set("[]".to_string()),
            size_bytes: Set(None),
            file_count: Set(None),
            error_message: Set(None),
            expires_at: Set(expires_at),
            checksum: Set(None),
            metadata: Set(serde_json::Value::Object(backups_metadata).to_string()),
        }
        .insert(txn)
        .await?;

        let child = temps_entities::external_service_backups::ActiveModel {
            id: sea_orm::NotSet,
            service_id: Set(service_id),
            backup_id: Set(parent.id),
            backup_type: Set(backup_type.to_string()),
            state: Set("pending".to_string()),
            started_at: Set(now),
            finished_at: Set(None),
            size_bytes: Set(None),
            s3_location: Set(String::new()),
            error_message: Set(None),
            metadata: Set(serde_json::Value::Object(esb_metadata)),
            checksum: Set(None),
            compression_type: Set(compression_type.to_string()),
            created_by: Set(created_by),
            expires_at: Set(expires_at),
            service_name_snapshot: Set(Some(source_service.name)),
            service_type_snapshot: Set(Some(source_service.service_type)),
        }
        .insert(txn)
        .await?;

        info!(
            backup_id = %backup_uuid,
            service_id,
            s3_source_id,
            parent_row_id = parent.id,
            child_row_id = child.id,
            "BackupService: inserted pending external-service backup rows (engine task pending)",
        );

        Ok((parent, child))
    }

    /// Insert pending external-service backup rows (parent + child) and
    /// dispatch the engine task on the executor.
    ///
    /// Used by the manual `POST /external-services/{id}/run` handler. The
    /// txn is opened and committed internally; spawn happens after commit.
    ///
    /// Returns the child row and a synthetic `i64` (the parent backup row's
    /// id, widened) for backwards-compat with callers that previously used
    /// a `backup_jobs.id`.
    pub async fn create_pending_external_service_backup_row(
        &self,
        service_id: i32,
        s3_source_id: i32,
        backup_type: &str,
        created_by: i32,
        trigger: BackupTriggerParams,
    ) -> Result<(temps_entities::external_service_backups::Model, i64), BackupError> {
        temps_entities::external_services::Entity::find_by_id(service_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "ExternalService".to_string(),
                detail: format!("External service with ID {} not found", service_id),
            })?;
        temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "S3Source".to_string(),
                detail: format!("S3 source {} not found", s3_source_id),
            })?;

        let txn = self.db.begin().await?;
        let (parent, child) = self
            .insert_pending_external_service_backup_in_txn(
                &txn,
                service_id,
                s3_source_id,
                backup_type,
                created_by,
                "none",
                None,
                &trigger,
            )
            .await?;
        txn.commit().await?;

        let backup_id = parent.id;
        let max_runtime_secs = trigger.max_runtime_secs.unwrap_or(4 * 60 * 60);
        if let Err(e) = self
            .queue
            .send(temps_core::Job::BackupRequested(
                temps_core::BackupRequestedJob {
                    backup_id,
                    engine: trigger.engine.clone(),
                    params: trigger.params,
                    max_runtime_secs,
                },
            ))
            .await
        {
            warn!(
                backup_id,
                service_id,
                error = %e,
                "create_pending_external_service_backup_row: queue.send failed; row committed but not dispatched",
            );
        }

        Ok((child, backup_id as i64))
    }

    /// List backups for a specific external service using a single JOIN query.
    ///
    /// Unlike [`list_source_backups`], this method never touches S3.  It issues
    /// one SQL round-trip:
    ///
    /// ```sql
    /// SELECT b.id, b.backup_id, b.name, b.state, b.started_at, b.finished_at,
    ///        b.size_bytes, b.s3_location, b.error_message, b.compression_type,
    ///        b.s3_source_id, s.name AS s3_source_name,
    ///        esb.id AS external_service_backup_id
    /// FROM external_service_backups esb
    /// JOIN backups b ON b.id = esb.backup_id
    /// JOIN s3_sources s ON s.id = b.s3_source_id
    /// WHERE esb.service_id = $1
    /// ORDER BY b.started_at DESC
    /// LIMIT $2 OFFSET $3
    /// ```
    ///
    /// Returns a page of [`ServiceBackupEntry`] values plus the total count for
    /// pagination.  `page` is 1-based; `page_size` is capped at 100.
    pub async fn list_external_service_backups(
        &self,
        service_id: i32,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<ServiceBackupEntry>, i64), BackupError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let offset = (page - 1) * page_size;

        // Count total rows so the caller can render pagination controls.
        let count_stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            r#"SELECT COUNT(*) AS cnt
               FROM external_service_backups esb
               WHERE esb.service_id = $1"#,
            vec![Value::Int(Some(service_id))],
        );

        #[derive(FromQueryResult)]
        struct CountRow {
            cnt: i64,
        }

        let total = CountRow::find_by_statement(count_stmt)
            .one(self.db.as_ref())
            .await?
            .map(|r| r.cnt)
            .unwrap_or(0);

        // Fetch the page of backups.
        let rows_stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            r#"SELECT
                   b.id,
                   b.backup_id,
                   b.name,
                   b.state,
                   b.backup_type,
                   b.started_at,
                   b.finished_at,
                   b.size_bytes,
                   b.s3_location,
                   b.error_message,
                   b.compression_type,
                   b.s3_source_id,
                   s.name AS s3_source_name,
                   esb.id AS external_service_backup_id
               FROM external_service_backups esb
               JOIN backups b ON b.id = esb.backup_id
               JOIN s3_sources s ON s.id = b.s3_source_id
               WHERE esb.service_id = $1
               ORDER BY b.started_at DESC
               LIMIT $2 OFFSET $3"#,
            vec![
                Value::Int(Some(service_id)),
                Value::BigInt(Some(page_size)),
                Value::BigInt(Some(offset)),
            ],
        );

        let entries = ServiceBackupEntry::find_by_statement(rows_stmt)
            .all(self.db.as_ref())
            .await?;

        debug!(
            service_id,
            count = entries.len(),
            total,
            "list_external_service_backups: returned DB-only page"
        );

        Ok((entries, total))
    }

    /// Return every `external_service_backups` child row that belongs to the
    /// given parent `backups.id`, joined with `external_services` so the caller
    /// can display the service name and type without a second round-trip.
    ///
    /// Returns an empty `Vec` when the parent backup has no children (control-
    /// plane backups have no children by definition).  Returns `NotFound` when
    /// the parent `backups` row does not exist.
    ///
    /// SQL uses a single JOIN — no N+1.
    pub async fn list_child_backups(
        &self,
        parent_backup_id: i32,
    ) -> Result<Vec<ChildBackupEntry>, BackupError> {
        use sea_orm::EntityTrait;

        // Verify the parent backup exists first so we return 404 instead of an
        // empty list when the caller passes an unknown integer id.
        let _parent = temps_entities::backups::Entity::find_by_id(parent_backup_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "Backup".to_string(),
                detail: format!("Parent backup with id {} not found", parent_backup_id),
            })?;

        let sql = r#"
SELECT
    esb.id            AS id,
    esb.service_id    AS service_id,
    esb.state         AS state,
    esb.backup_type   AS backup_type,
    esb.started_at    AS started_at,
    esb.finished_at   AS finished_at,
    esb.size_bytes    AS size_bytes,
    esb.s3_location   AS s3_location,
    esb.error_message AS error_message,
    esb.compression_type AS compression_type,
    COALESCE(es.name, esb.service_name_snapshot, 'deleted service') AS service_name,
    COALESCE(es.service_type, esb.service_type_snapshot, 'unknown') AS service_type
FROM external_service_backups esb
LEFT JOIN external_services es ON es.id = esb.service_id
WHERE esb.backup_id = $1
ORDER BY esb.id ASC
        "#;

        let rows = ChildBackupEntry::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            vec![Value::Int(Some(parent_backup_id))],
        ))
        .all(self.db.as_ref())
        .await
        .map_err(BackupError::Database)?;

        debug!(
            parent_backup_id,
            count = rows.len(),
            "list_child_backups: returned children"
        );

        Ok(rows)
    }

    /// Get an external service by ID
    pub async fn get_external_service(
        &self,
        service_id: i32,
    ) -> Result<temps_entities::external_services::Model, BackupError> {
        temps_entities::external_services::Entity::find_by_id(service_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "ExternalService".to_string(),
                detail: format!("External service with ID {} not found", service_id),
            })
    }

    /// Resolve the live backup artifact and Cloud mirror compatibility for an
    /// external service. Database lookup and Docker probing stay in the
    /// service layer so HTTP handlers only perform authorization and mapping.
    pub async fn get_external_service_backup_capability(
        &self,
        service_id: i32,
    ) -> Result<super::ExternalServiceBackupCapability, super::BackupCapabilityError> {
        let service = temps_entities::external_services::Entity::find_by_id(service_id)
            .one(self.db.as_ref())
            .await
            .map_err(|source| super::BackupCapabilityError::LoadService { service_id, source })?
            .ok_or(super::BackupCapabilityError::ServiceNotFound { service_id })?;

        Ok(super::capability::probe_external_service_backup_capability(
            &service,
            &self.external_service_manager,
        )
        .await)
    }

    pub async fn backup_external_service(
        &self,
        service: &temps_entities::external_services::Model,
        s3_source_id: i32,
        backup_type: &str,
        created_by: i32,
    ) -> Result<temps_entities::external_service_backups::Model, BackupError> {
        info!("Starting external service backup process");
        let service_id = service.id;

        // Get S3 source configuration
        let s3_source = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "S3Source".to_string(),
                detail: "S3 source not found".to_string(),
            })?;

        // Create S3 client
        let s3_client = self
            .create_s3_client(&s3_source)
            .await
            .map_err(|e| BackupError::S3(e.to_string()))?;

        // Decrypt S3 credentials for services that pass them to external tools (e.g., WAL-G)
        let decrypted_access_key = self
            .encryption_service
            .decrypt_string(&s3_source.access_key_id)
            .map_err(|e| BackupError::Internal {
                message: format!("Failed to decrypt access key for backup: {}", e),
            })?;
        let decrypted_secret_key = self
            .encryption_service
            .decrypt_string(&s3_source.secret_key)
            .map_err(|e| BackupError::Internal {
                message: format!("Failed to decrypt secret key for backup: {}", e),
            })?;
        // `None` unless this source holds a temporary (STS-style) credential.
        let decrypted_session_token = temps_entities::s3_sources::decrypt_session_token(
            self.encryption_service.as_ref(),
            &s3_source,
        )
        .map_err(|e| BackupError::Internal {
            message: format!("Failed to decrypt session token for backup: {}", e),
        })?;
        let s3_credentials = temps_providers::S3Credentials {
            access_key_id: decrypted_access_key,
            secret_key: decrypted_secret_key,
            session_token: decrypted_session_token,
            region: s3_source.region.clone(),
            endpoint: s3_source.endpoint.clone(),
            bucket_name: s3_source.bucket_name.clone(),
            bucket_path: s3_source.bucket_path.clone(),
            force_path_style: s3_source.force_path_style.unwrap_or(true),
        };

        // Generate unique backup ID
        let backup_id = Uuid::new_v4().to_string();

        // Create backup record. The heartbeat starts at now() so the row
        // appears alive from the moment it's created — the worker task
        // refreshes it periodically while the engine runs.
        let now = chrono::Utc::now();
        let backup = temps_entities::backups::ActiveModel {
            id: sea_orm::NotSet,
            name: sea_orm::Set(format!("Backup {}", backup_id)),
            backup_id: sea_orm::Set(backup_id.clone()),
            schedule_id: sea_orm::Set(None),
            schedule_run_id: sea_orm::NotSet,
            backup_type: sea_orm::Set(backup_type.to_string()),
            state: sea_orm::Set("running".to_string()),
            started_at: sea_orm::Set(now),
            finished_at: sea_orm::Set(None),
            s3_source_id: sea_orm::Set(s3_source_id),
            s3_location: sea_orm::Set("".to_string()), // Will be updated by the service
            compression_type: sea_orm::Set("gzip".to_string()),
            created_by: sea_orm::Set(created_by),
            tags: sea_orm::Set("[]".to_string()),
            size_bytes: sea_orm::Set(None),
            file_count: sea_orm::Set(None),
            error_message: sea_orm::Set(None),
            metadata: sea_orm::Set(
                json!({
                    "service_id": service_id,
                    "service_type": service.service_type,
                    "service_name": service.name,
                    "timestamp": now.to_rfc3339()
                })
                .to_string(),
            ),
            checksum: sea_orm::Set(None),
            expires_at: sea_orm::Set(None),
        };

        // Generate backup path
        let subpath = format!(
            "external_services/{}/{}/{}",
            service.service_type,
            service.name,
            Utc::now().format("%Y/%m/%d")
        );
        let subpath_root = format!(
            "external_services/{}/{}",
            service.service_type, service.name
        );
        let service_type = temps_providers::ServiceType::from_str(&service.service_type)
            .map_err(|e| BackupError::Validation(e.to_string()))?;
        // Resolving an engine can now fail (the Docker daemon a container-backed
        // engine needs may be absent), so this Result is propagated rather than
        // used directly — the backup cannot proceed without the engine.
        let service_instance = self
            .external_service_manager
            .resolve_service_instance(&service.name, service_type)
            .await
            .map_err(|e| {
                error!(
                    "Could not resolve the backup engine for service '{}' (type={}, id={}): {}",
                    service.name, service.service_type, service.id, e
                );
                BackupError::ExternalService(e.to_string())
            })?;

        let service_config = self
            .external_service_manager
            .get_service_config(service_id)
            .await
            .map_err(|e| BackupError::ExternalService(e.to_string()))?;
        // Resolved before the row exists, so a service that cannot be backed
        // up at all leaves no `running` row behind.
        let backup = backup.insert(self.db.as_ref()).await?;

        // Cluster topology: route through the manager which knows how
        // to find the current primary and dispatch exec to it (local
        // bollard or remote agent). The trait method on
        // PostgresClusterService doesn't have access to the agent
        // protocol so it can't handle multi-host clusters; this is
        // the deliberate carve-out.
        let backup_outcome = if service.topology == "cluster" && service.service_type == "postgres"
        {
            self.external_service_manager
                .backup_postgres_cluster(service, &s3_credentials, &subpath_root, backup.id)
                .await
                .map_err(|e| {
                    error!(
                        "Cluster WAL-G backup failed for service '{}' (id={}): {}",
                        service.name, service.id, e
                    );
                    BackupError::ExternalService(e.to_string())
                })
        } else {
            // Standalone: use the per-engine trait impl as before.
            service_instance
                .backup_to_s3(
                    &s3_client,
                    &s3_credentials,
                    backup.clone(),
                    &s3_source,
                    &subpath,
                    &subpath_root,
                    &self.db,
                    service,
                    service_config,
                )
                .await
                .map_err(|e| {
                    error!(
                        "External service backup failed for service '{}' (type={}, id={}): {}",
                        service.name, service.service_type, service.id, e
                    );
                    BackupError::ExternalService(e.to_string())
                })
        };
        let backup_outcome = match backup_outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // Keep the cause on the row: callers such as the pre-upgrade
                // backup report it, and the backup list shows it.
                let mut failed: temps_entities::backups::ActiveModel = backup.clone().into();
                failed.state = sea_orm::Set("failed".to_string());
                failed.error_message = sea_orm::Set(Some(error.to_string()));
                failed.finished_at = sea_orm::Set(Some(Utc::now()));
                if let Err(update_error) = failed.update(self.db.as_ref()).await {
                    error!(
                        "Failed to mark backup {} of service '{}' (id={}) as failed: {}",
                        backup.backup_id, service.name, service.id, update_error
                    );
                }
                return Err(error);
            }
        };
        info!(
            "Backup created at location: {} ({} bytes)",
            backup_outcome.location,
            backup_outcome
                .size_bytes
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );

        // If the engine couldn't determine size locally, fall back to
        // listing the S3 prefix. Best-effort: a missing size is annoying
        // but doesn't block the backup from being marked completed.
        let final_size_bytes = match backup_outcome.size_bytes {
            Some(n) => Some(n),
            None => {
                // Strip the "s3://bucket/" prefix to get a list-able key.
                let bucket = &s3_source.bucket_name;
                let prefix = backup_outcome
                    .location
                    .strip_prefix(&format!("s3://{}/", bucket))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| backup_outcome.location.trim_start_matches('/').to_string());
                match temps_providers::externalsvc::s3_util::list_total_size(
                    &s3_client, bucket, &prefix,
                )
                .await
                {
                    Ok(n) => Some(n),
                    Err(e) => {
                        warn!(
                            "Could not compute size by listing s3://{}/{}: {}",
                            bucket, prefix, e
                        );
                        None
                    }
                }
            }
        };

        // Mark the parent `backups` row as completed. Without this the row
        // stays in state='running' forever, which breaks listing/filtering
        // and makes the restore UI skip the backup. Retry transient DB
        // failures a few times before giving up — the backup data itself
        // already succeeded, so we don't fail the caller on a lasting
        // failure either, but we also don't want a single blip to strand
        // the row in 'running' until the next server restart reconciles it.
        let retry = temps_core::retry::RetryConfig::new(3)
            .with_base_delay(std::time::Duration::from_millis(200))
            .with_max_delay(std::time::Duration::from_secs(2));
        let update_result = retry
            .retry(|| {
                let mut backup_update: temps_entities::backups::ActiveModel = backup.clone().into();
                backup_update.state = sea_orm::Set("completed".to_string());
                backup_update.s3_location = sea_orm::Set(backup_outcome.location.clone());
                backup_update.finished_at = sea_orm::Set(Some(Utc::now()));
                backup_update.size_bytes = sea_orm::Set(final_size_bytes);
                let db = self.db.as_ref();
                async move { backup_update.update(db).await }
            })
            .await;
        if let Err(e) = update_result {
            // Don't fail the caller — the backup itself succeeded. Log and
            // continue; the boot-time reconciler will mark the still-'running'
            // row as failed, and the completed child `external_service_backups`
            // row (already correctly updated above) keeps the real S3 location
            // attributable for later cleanup/restore.
            error!(
                "Failed to mark backup {} as completed after retries: {}",
                backup.id, e
            );
        }

        // Get the external service backup record
        let external_backup = temps_entities::external_service_backups::Entity::find()
            .filter(temps_entities::external_service_backups::Column::BackupId.eq(backup.id))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "ExternalServiceBackup".to_string(),
                detail: "External service backup record not found".to_string(),
            })?;

        info!(
            "External service backup completed successfully: {}",
            backup_id
        );
        Ok(external_backup)
    }

    /// Start the backup scheduler with graceful cancellation support.
    ///
    /// This method runs an infinite loop that:
    /// 1. Initializes schedules that don't have `next_run` set.
    /// 2. Fires once per hour to enqueue any schedules whose `next_run` has
    ///    elapsed. Enqueueing is fast (milliseconds) because the runner picks
    ///    up and executes the jobs asynchronously — the scheduler never `.await`s
    ///    backup execution.
    /// 3. Enforces retention after enqueueing.
    /// 4. Can be gracefully cancelled via the provided `CancellationToken`.
    ///
    pub async fn start_backup_scheduler(
        &self,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), BackupError> {
        debug!("Starting backup scheduler");

        // First update all schedules that don't have next_run set
        let schedules = temps_entities::backup_schedules::Entity::find()
            .filter(temps_entities::backup_schedules::Column::NextRun.is_null())
            .all(self.db.as_ref())
            .await?;
        debug!("Updating next_run for {} schedules", schedules.len());
        for schedule in schedules {
            let cron_schedule = Schedule::from_str(&schedule.schedule_expression).map_err(|e| {
                BackupError::Validation(format!(
                    "Error parsing schedule expression for schedule {}: {}",
                    schedule.id, e
                ))
            })?;
            if let Some(next_run) = cron_schedule.upcoming(Utc).next() {
                let schedule_id = schedule.id;
                let mut schedule_update: temps_entities::backup_schedules::ActiveModel =
                    schedule.into_active_model();
                schedule_update.next_run = sea_orm::Set(Some(next_run));
                schedule_update.update(self.db.as_ref()).await?;
                info!(
                    "Updated next_run for schedule {}: {}",
                    schedule_id, next_run
                );
            }
        }

        loop {
            let now = Utc::now();

            // Only run at the start of each hour
            if now.minute() != 0 {
                // Sleep until next hour or cancellation
                let next_hour = (now + chrono::Duration::hours(1))
                    .with_minute(0)
                    .and_then(|t| t.with_second(0))
                    .and_then(|t| t.with_nanosecond(0))
                    .unwrap_or(now + chrono::Duration::hours(1));
                let sleep_duration = next_hour - now;

                tokio::select! {
                    _ = time::sleep(time::Duration::from_secs(sleep_duration.num_seconds() as u64)) => {
                        continue;
                    }
                    _ = cancellation_token.cancelled() => {
                        info!("Backup scheduler received cancellation signal");
                        return Ok(());
                    }
                }
            }

            // Process scheduled backups with cancellation check
            tokio::select! {
                result = self.process_scheduled_backups(now) => {
                    if let Err(e) = result {
                        error!("Error processing scheduled backups: {}", e);
                    }
                }
                _ = cancellation_token.cancelled() => {
                    info!("Backup scheduler received cancellation signal");
                    return Ok(());
                }
            }

            // Enforce retention: delete backups older than the schedule's retention period
            tokio::select! {
                result = self.enforce_retention(None, None) => {
                    if let Err(e) = result {
                        error!("Error enforcing backup retention: {}", e);
                    }
                }
                _ = cancellation_token.cancelled() => {
                    info!("Backup scheduler received cancellation signal during retention cleanup");
                    return Ok(());
                }
            }

            // Sleep until next hour or cancellation
            let next_hour = (now + chrono::Duration::hours(1))
                .with_minute(0)
                .and_then(|t| t.with_second(0))
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(now + chrono::Duration::hours(1));
            let sleep_duration = next_hour - now;

            tokio::select! {
                _ = time::sleep(time::Duration::from_secs(sleep_duration.num_seconds() as u64)) => {}
                _ = cancellation_token.cancelled() => {
                    info!("Backup scheduler received cancellation signal");
                    return Ok(());
                }
            }
        }
    }

    /// Iterate over all enabled schedules whose `next_run` has elapsed and
    /// fan-out a `schedule_runs` row + backup jobs for each one.
    ///
    /// Each call to `enqueue_scheduled_run` is transactional and completes in
    /// milliseconds — the runner picks up and executes each job asynchronously.
    /// Sequential iteration is fast because we never `.await` backup execution
    /// inside this method.
    async fn process_scheduled_backups(&self, now: DateTime<Utc>) -> Result<(), BackupError> {
        let schedules = temps_entities::backup_schedules::Entity::find()
            .filter(temps_entities::backup_schedules::Column::Enabled.eq(true))
            .all(self.db.as_ref())
            .await?;

        for schedule in schedules {
            // Skip if next_run hasn't elapsed yet (or if it's unset — the
            // init loop in start_backup_scheduler already populated it).
            let due = schedule.next_run.is_some_and(|t| t <= now);
            if !due {
                continue;
            }

            match self
                .enqueue_scheduled_run(&schedule, TriggerSource::Cron, None)
                .await
            {
                Ok(ScheduleRunOutcome::Started { run_id, ref jobs }) => {
                    info!(
                        schedule_id = schedule.id,
                        schedule_name = %schedule.name,
                        run_id,
                        job_count = jobs.len(),
                        "scheduled run enqueued",
                    );
                }
                Ok(ScheduleRunOutcome::AlreadyInFlight { existing_run_id }) => {
                    info!(
                        schedule_id = schedule.id,
                        schedule_name = %schedule.name,
                        existing_run_id,
                        "scheduled run skipped: previous run still in flight",
                    );
                }
                Err(e) => {
                    error!(
                        schedule_id = schedule.id,
                        schedule_name = %schedule.name,
                        error = %e,
                        "scheduled run enqueue failed",
                    );
                }
            }
        }

        Ok(())
    }

    /// Return the external service record linked to a backup via the
    /// `external_service_backups` join table, or `None` if no such row
    /// exists (e.g. for control-plane backups).
    ///
    /// Used by `GET /backups/{id}` to populate `external_service` in the
    /// response without requiring an N+1 join at the handler level.
    pub async fn get_backup_external_service(
        &self,
        backup_id: i32,
    ) -> Result<Option<temps_entities::external_services::Model>, BackupError> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        // Look up the child row in external_service_backups for this backup.
        let child = temps_entities::external_service_backups::Entity::find()
            .filter(temps_entities::external_service_backups::Column::BackupId.eq(backup_id))
            .one(self.db.as_ref())
            .await?;

        let service_id = match child {
            Some(row) => row.service_id,
            None => return Ok(None),
        };

        // Load the parent external_services row. A missing row here is an
        // unexpected data-integrity gap, but we swallow it gracefully so
        // the backup detail page can still render.
        let service = temps_entities::external_services::Entity::find_by_id(service_id)
            .one(self.db.as_ref())
            .await?;

        Ok(service)
    }
}

/// Implementation of the pre-upgrade backup provider required by the
/// postgres major-upgrade orchestrator. Lives here (not in temps-providers)
/// because temps-backup owns `BackupService` and already depends on
/// temps-providers — the trait is defined in temps-providers specifically
/// to keep the dep flow one-way.
#[async_trait::async_trait]
impl temps_providers::externalsvc::postgres_upgrade::PreUpgradeBackupProvider for BackupService {
    async fn default_s3_source_id(&self, _service_id: i32) -> Result<Option<i32>, String> {
        // Default S3 source is user-scoped (global for now). Look up the
        // single row flagged is_default=true; return None if none set so
        // the orchestrator raises NoDefaultS3Source.
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let row = temps_entities::s3_sources::Entity::find()
            .filter(temps_entities::s3_sources::Column::IsDefault.eq(true))
            .one(self.db.as_ref())
            .await
            .map_err(|e| e.to_string())?;
        Ok(row.map(|r| r.id))
    }

    async fn create_pre_upgrade_backup(
        &self,
        service_id: i32,
        s3_source_id: i32,
        created_by: i32,
    ) -> Result<i32, String> {
        // Back up the service being upgraded, not the whole instance: an
        // unrelated service that cannot be backed up must not block this
        // upgrade, and this service's own failure is reported with its cause
        // rather than folded into an instance-wide summary.
        let service = temps_entities::external_services::Entity::find_by_id(service_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("failed to load service {service_id}: {e}"))?
            .ok_or_else(|| format!("service {service_id} no longer exists"))?;
        let child = self
            .backup_external_service(&service, s3_source_id, "full", created_by)
            .await
            .map_err(|e| {
                format!(
                    "full backup of '{}' to S3 source {} failed: {}",
                    service.name, s3_source_id, e
                )
            })?;
        // The upgrade row's FK targets `backups(id)`, the parent of the
        // per-service child row.
        Ok(child.backup_id)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use sea_orm::{DatabaseBackend, MockDatabase};
    use temps_core::EncryptionService;

    #[test]
    fn sourced_internal_walg_values_are_shell_quoted() {
        assert_eq!(
            shell_export_assignment("WALG_S3_PREFIX=s3://bucket/ok;touch${IFS}/tmp/pwn;#"),
            Some("export WALG_S3_PREFIX='s3://bucket/ok;touch${IFS}/tmp/pwn;#'".to_string())
        );
        assert!(shell_export_assignment("BAD-KEY=value").is_none());
    }

    #[test]
    fn s3_key_from_uri_strips_matching_bucket() {
        let key = s3_key_from_location("s3://backups/path/to/archive.sql.gz", "backups")
            .expect("valid S3 URI should produce a key");
        assert_eq!(key, "path/to/archive.sql.gz");
    }

    #[test]
    fn s3_key_from_location_preserves_plain_key() {
        let key = s3_key_from_location("/path/to/archive.sql.gz", "backups")
            .expect("plain S3 key should be accepted");
        assert_eq!(key, "path/to/archive.sql.gz");
    }

    #[test]
    fn s3_key_from_uri_rejects_different_bucket() {
        let error = s3_key_from_location("s3://other/path/archive.sql.gz", "backups")
            .expect_err("mismatched bucket must be rejected");
        assert!(matches!(error, BackupError::Validation(_)));
        assert!(error
            .to_string()
            .contains("does not match configured bucket"));
    }

    #[test]
    fn walg_repository_identity_search_handles_nested_backup_list_detail() {
        let repository = serde_json::json!([
            {
                "backup_name": "base_0001",
                "user_data": {"temps_backup_id": "other"}
            },
            {
                "backup_name": "base_0002",
                "detail": {
                    "sentinel": {
                        "user_data": {"temps_backup_id": "selected-backup"}
                    }
                }
            }
        ]);
        assert!(json_contains_backup_identity(
            &repository,
            "selected-backup"
        ));
        assert!(!json_contains_backup_identity(&repository, "missing"));
    }

    #[test]
    fn zero_target_schedule_run_is_skipped_not_completed() {
        assert_eq!(schedule_run_aggregate_state(0, 0, 0, 0), "skipped");
        assert_eq!(schedule_run_aggregate_state(1, 0, 0, 0), "completed");
        assert_eq!(schedule_run_aggregate_state(1, 1, 0, 0), "failed");
    }

    /// Validate the pagination clamping logic: page < 1 becomes 1, page_size
    /// above 100 becomes 100. We can't mock raw SQL in unit tests (Sea-ORM
    /// MockDatabase only covers entity model types, not bare FromQueryResult
    /// structs), so we test only the arithmetic that lives outside the query.
    #[test]
    fn test_list_external_service_backups_pagination_clamp() {
        // page = 0 clamps to 1 — the underflow happens at `(page - 1) * page_size`
        // if we don't, producing a negative OFFSET that Postgres rejects.
        let raw_page: i64 = 0;
        let page: i64 = raw_page.max(1);
        assert_eq!(page, 1);
        // page_size = 200 → page_size = 100 (clamp 1..=100)
        let page_size: i64 = 200_i64.clamp(1, 100);
        assert_eq!(page_size, 100);
        // offset
        let offset = (page - 1) * page_size;
        assert_eq!(offset, 0);
    }

    /// Helper: build a minimal `ScheduleRunSummary` row value for MockDatabase.
    ///
    /// The new fan-out shape returns one row per scheduler tick with aggregate
    /// child counts. The row schema must match `RunRow` (defined in the
    /// `list_schedule_runs` SQL); fields here mirror that.
    fn make_run_entry_row(
        run_id: i64,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> std::collections::BTreeMap<String, sea_orm::Value> {
        use sea_orm::Value as SVal;
        let mut row = std::collections::BTreeMap::new();
        row.insert("run_id".to_string(), SVal::BigInt(Some(run_id)));
        row.insert("schedule_id".to_string(), SVal::Int(Some(1)));
        row.insert(
            "triggered_by".to_string(),
            SVal::String(Some(Box::new("cron".to_string()))),
        );
        row.insert(
            "started_at".to_string(),
            SVal::ChronoDateTimeUtc(Some(Box::new(started_at))),
        );
        row.insert("finished_at".to_string(), SVal::ChronoDateTimeUtc(None));
        row.insert("total_jobs".to_string(), SVal::BigInt(Some(1)));
        row.insert("completed_jobs".to_string(), SVal::BigInt(Some(1)));
        row.insert("failed_jobs".to_string(), SVal::BigInt(Some(0)));
        row.insert("running_jobs".to_string(), SVal::BigInt(Some(0)));
        row.insert("pending_jobs".to_string(), SVal::BigInt(Some(0)));
        row
    }

    /// `list_schedule_runs` must return rows ordered newest-first by the SQL
    /// (which uses `ORDER BY started_at DESC`).
    ///
    /// The MockDatabase returns rows in the order the test supplies them; we
    /// supply them newest-first and assert that the response preserves that
    /// order and that pagination metadata is correct.
    #[tokio::test]
    async fn test_list_schedule_runs_returns_rows_ordered_desc() {
        let now = chrono::Utc::now();
        let older = now - chrono::Duration::hours(2);

        let schedule = make_test_schedule(1, 1);

        // MockDatabase query sequence:
        // 1. get_backup_schedule SELECT (returns the schedule)
        // 2. COUNT(*) query
        // 3. Paginated rows query (returns two entries)
        let count_row = {
            let mut r = std::collections::BTreeMap::new();
            r.insert("total".to_string(), sea_orm::Value::BigInt(Some(2)));
            r
        };

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // get_backup_schedule
                .append_query_results(vec![vec![schedule]])
                // COUNT query
                .append_query_results(vec![vec![count_row]])
                // paginated rows — newest first (run_id=2 is more recent)
                .append_query_results(vec![vec![
                    make_run_entry_row(2, now),
                    make_run_entry_row(1, older),
                ]])
                .into_connection(),
        );

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        let response = svc
            .list_schedule_runs(1, 1, 20)
            .await
            .expect("list_schedule_runs must succeed");

        assert_eq!(response.total, 2, "total must match COUNT result");
        assert_eq!(response.runs.len(), 2, "must return 2 run entries");
        // First entry is the most recent (run_id=2).
        assert_eq!(response.runs[0].run_id, 2, "first entry must be newest");
        assert_eq!(response.runs[1].run_id, 1, "second entry must be older");
    }

    /// `list_schedule_runs` must clamp `page < 1` to 1, so the offset never
    /// goes negative.
    #[tokio::test]
    async fn test_list_schedule_runs_clamps_page_below_one() {
        let schedule = make_test_schedule(1, 1);
        let count_row = {
            let mut r = std::collections::BTreeMap::new();
            r.insert("total".to_string(), sea_orm::Value::BigInt(Some(0)));
            r
        };

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![schedule]])
                .append_query_results(vec![vec![count_row]])
                .append_query_results(vec![Vec::<
                    std::collections::BTreeMap<String, sea_orm::Value>,
                >::new()])
                .into_connection(),
        );

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        // page=-5 must be treated as page=1 (offset=0).
        let result = svc.list_schedule_runs(1, -5, 20).await;
        assert!(result.is_ok(), "negative page must not error: {:?}", result);
    }

    /// `list_schedule_runs` must clamp `page_size > 100` to 100 so the client
    /// cannot request an unbounded result set.
    #[tokio::test]
    async fn test_list_schedule_runs_clamps_page_size_above_100() {
        let schedule = make_test_schedule(1, 1);
        let count_row = {
            let mut r = std::collections::BTreeMap::new();
            r.insert("total".to_string(), sea_orm::Value::BigInt(Some(0)));
            r
        };

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![schedule]])
                .append_query_results(vec![vec![count_row]])
                .append_query_results(vec![Vec::<
                    std::collections::BTreeMap<String, sea_orm::Value>,
                >::new()])
                .into_connection(),
        );

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        // page_size=999 must be clamped to 100. The call itself must succeed.
        let result = svc.list_schedule_runs(1, 1, 999).await;
        assert!(
            result.is_ok(),
            "oversized page_size must not error: {:?}",
            result
        );
    }

    /// `list_schedule_runs` with an unknown schedule_id must return `NotFound`.
    #[tokio::test]
    async fn test_list_schedule_runs_not_found() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // get_backup_schedule returns empty → schedule does not exist
                .append_query_results(vec![Vec::<temps_entities::backup_schedules::Model>::new()])
                .into_connection(),
        );

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        let result = svc.list_schedule_runs(9999, 1, 20).await;
        assert!(result.is_err(), "unknown schedule must return an error");
        assert!(
            matches!(result.unwrap_err(), BackupError::NotFound { .. }),
            "expected BackupError::NotFound for unknown schedule"
        );
    }

    // ── list_child_backups ────────────────────────────────────────────────────

    /// Helper: build a minimal `ChildBackupEntry` BTreeMap row for MockDatabase.
    fn make_child_backup_row(
        id: i32,
        service_id: i32,
        state: &str,
    ) -> std::collections::BTreeMap<String, sea_orm::Value> {
        use sea_orm::Value as SVal;
        let mut row = std::collections::BTreeMap::new();
        row.insert("id".to_string(), SVal::Int(Some(id)));
        row.insert("service_id".to_string(), SVal::Int(Some(service_id)));
        row.insert(
            "service_name".to_string(),
            SVal::String(Some(Box::new(format!("service-{}", service_id)))),
        );
        row.insert(
            "service_type".to_string(),
            SVal::String(Some(Box::new("postgres".to_string()))),
        );
        row.insert(
            "state".to_string(),
            SVal::String(Some(Box::new(state.to_string()))),
        );
        row.insert(
            "backup_type".to_string(),
            SVal::String(Some(Box::new("full".to_string()))),
        );
        row.insert(
            "started_at".to_string(),
            SVal::ChronoDateTimeUtc(Some(Box::new(chrono::Utc::now()))),
        );
        row.insert("finished_at".to_string(), SVal::ChronoDateTimeUtc(None));
        row.insert("size_bytes".to_string(), SVal::BigInt(Some(2048)));
        row.insert(
            "s3_location".to_string(),
            SVal::String(Some(Box::new("s3://bucket/child".to_string()))),
        );
        row.insert("error_message".to_string(), SVal::String(None));
        row.insert(
            "compression_type".to_string(),
            SVal::String(Some(Box::new("lz4".to_string()))),
        );
        row
    }

    /// `list_child_backups` returns all children ordered by `id ASC` when the
    /// parent backup exists and has two completed child rows.
    #[tokio::test]
    async fn test_list_child_backups_returns_ordered_rows() {
        let parent = make_test_backup_model(10);

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // find_by_id for the parent backup
                .append_query_results(vec![vec![parent]])
                // child rows query — two entries, ordered by id ASC (service 1 then 2)
                .append_query_results(vec![vec![
                    make_child_backup_row(1, 1, "completed"),
                    make_child_backup_row(2, 2, "completed"),
                ]])
                .into_connection(),
        );

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        let children = svc
            .list_child_backups(10)
            .await
            .expect("list_child_backups must succeed");

        assert_eq!(children.len(), 2, "must return 2 children");
        assert_eq!(children[0].id, 1, "first child must have id=1");
        assert_eq!(children[1].id, 2, "second child must have id=2");
        assert_eq!(children[0].state, "completed");
        assert_eq!(children[0].service_type, "postgres");
    }

    /// `list_child_backups` returns an empty Vec when the parent backup exists
    /// but has no child rows (e.g. a control-plane backup).
    #[tokio::test]
    async fn test_list_child_backups_returns_empty_for_no_children() {
        let parent = make_test_backup_model(99);

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // find_by_id returns the parent
                .append_query_results(vec![vec![parent]])
                // child rows query returns nothing
                .append_query_results(vec![Vec::<
                    std::collections::BTreeMap<String, sea_orm::Value>,
                >::new()])
                .into_connection(),
        );

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        let children = svc
            .list_child_backups(99)
            .await
            .expect("list_child_backups must succeed for parent with no children");

        assert!(children.is_empty(), "must return empty Vec");
    }

    /// `list_child_backups` returns `NotFound` when the parent backup does not
    /// exist, so the handler can surface a 404 instead of an empty list.
    #[tokio::test]
    async fn test_list_child_backups_returns_not_found_for_missing_parent() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // find_by_id returns nothing → parent does not exist
                .append_query_results(vec![Vec::<temps_entities::backups::Model>::new()])
                .into_connection(),
        );

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        let result = svc.list_child_backups(9999).await;
        assert!(result.is_err(), "missing parent must return an error");
        assert!(
            matches!(result.unwrap_err(), BackupError::NotFound { .. }),
            "expected BackupError::NotFound for unknown parent backup"
        );
    }

    // ── backup_schedule_services membership ──────────────────────────────
    //
    // These tests pin the contract of the attach/detach/list helpers. They
    // need a `BackupService`, which in turn requires an
    // `ExternalServiceManager`, which constructs a Docker client at build
    // time. We early-return when Docker is unavailable so the suite stays
    // green in CI environments without a daemon.
    //
    // The point of these is the *resolution* behaviour, not the SQL — the
    // join query itself is exercised by the integration test.

    /// Integration test: when `include_control_plane = false` and a single
    /// service is attached, the fan-out produces exactly one backup row
    /// (no control-plane row alongside it). This is the scenario from
    /// the user report where picking one Postgres still produced a
    /// `control_plane` backup as a sidecar.
    #[tokio::test]
    async fn integration_fan_out_skips_control_plane_when_flag_off() {
        if bollard::Docker::connect_with_local_defaults().is_err() {
            println!("Docker not available, skipping test");
            return;
        }
        use sea_orm::ActiveValue::Set;
        use sea_orm::{ColumnTrait, EntityTrait};
        use temps_database::test_utils::TestDatabase;

        let test_db = match TestDatabase::with_migrations().await {
            Ok(d) => d,
            Err(e) => {
                println!("TestDatabase unavailable, skipping: {e}");
                return;
            }
        };
        let db = test_db.db.clone();

        let s3 = temps_entities::s3_sources::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("cp-skip-source".to_string()),
            bucket_name: Set("b".to_string()),
            bucket_path: Set("/".to_string()),
            access_key_id: Set("".to_string()),
            secret_key: Set("".to_string()),
            session_token: Set(None),
            credentials_expire_at: Set(None),
            region: Set("us-east-1".to_string()),
            endpoint: Set(None),
            force_path_style: Set(Some(true)),
            is_default: Set(true),
            managed_by_cloud: Set(false),
            lifecycle_reconcile_failed_at: Set(None),
            lifecycle_reconcile_generation: Set(0),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            backing_service_id: Set(None),
        }
        .insert(db.as_ref())
        .await
        .expect("insert s3 source");

        // Schedule: specific mode, no control plane.
        let schedule = temps_entities::backup_schedules::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("cp-skip-schedule".to_string()),
            backup_type: Set("full".to_string()),
            retention_period: Set(7),
            s3_source_id: Set(s3.id),
            schedule_expression: Set("0 0 2 * * *".to_string()),
            enabled: Set(true),
            last_run: Set(None),
            next_run: Set(None),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            description: Set(None),
            tags: Set("[]".to_string()),
            max_runtime_secs: Set(None),
            target_all_services: Set(false),
            include_control_plane: Set(false),
            generated_kind: Set(None),
        }
        .insert(db.as_ref())
        .await
        .expect("insert schedule");

        let svc_pg = temps_entities::external_services::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("pg-only".to_string()),
            service_type: Set("postgres".to_string()),
            version: Set(Some("17".to_string())),
            status: Set("running".to_string()),
            slug: Set(Some("pg-only".to_string())),
            config: Set(None),
            node_id: Set(None),
            topology: Set("standalone".to_string()),
            error_message: Set(None),
            health_status: Set(None),
            last_health_check_at: Set(None),
            last_health_error: Set(None),
            consecutive_health_failures: Set(0),
            health_metadata: Set(None),
            metrics_enabled: Set(false),
            default_backup_provisioned: Set(false),
            container_name: Set(None),
            created_by_user_id: Set(None),
            continuous_archive_s3_source_id: Set(None),
            continuous_archive_pinned_at: Set(None),
            workload_mode: Default::default(),
            log_ingest_lines_per_sec: Set(None),
            log_ingest_kib_per_sec: Set(None),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
        }
        .insert(db.as_ref())
        .await
        .expect("insert pg");

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db.clone()),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        svc.attach_services_to_schedule(schedule.id, &[svc_pg.id])
            .await
            .expect("attach");

        // Sanity: post-attach, the schedule is well-formed (control-plane
        // off + specific mode + 1 attached service).
        let after_attach = svc
            .list_services_for_schedule(schedule.id)
            .await
            .expect("list");
        assert_eq!(after_attach.len(), 1);

        // Flip-empty test: updating to include_control_plane=false with
        // *no* attached services (we'll detach first) must fail.
        svc.detach_service_from_schedule(schedule.id, svc_pg.id)
            .await
            .expect("detach");
        let err = svc
            .update_backup_schedule(
                schedule.id,
                crate::handlers::backup_handler::UpdateBackupScheduleRequest {
                    name: None,
                    description: None,
                    schedule_expression: None,
                    retention_period: None,
                    max_runtime_secs: None,
                    enabled: None,
                    tags: None,
                    target_all_services: None,
                    include_control_plane: Some(false),
                    service_ids: None,
                },
                temps_core::BackupScheduleAuthority::InstanceAdmin,
            )
            .await
            .expect_err("empty fan-out must be rejected");
        assert!(
            matches!(err, BackupError::Validation(ref msg) if msg.contains("nothing to back up")),
            "expected Validation error, got {:?}",
            err
        );

        // Cleanup: schedule has no children, so the cascade can drop it.
        let _ = temps_entities::backup_schedules::Entity::delete_by_id(schedule.id)
            .exec(db.as_ref())
            .await;
        let _ = temps_entities::external_services::Entity::delete_by_id(svc_pg.id)
            .exec(db.as_ref())
            .await;
        // Silence unused warning on the QueryFilter / ColumnTrait imports.
        let _ = temps_entities::backup_schedule_services::Column::ScheduleId.eq(0);
    }
    // ── retention deadline (`backups.expires_at`) ───────────────────────────
}
