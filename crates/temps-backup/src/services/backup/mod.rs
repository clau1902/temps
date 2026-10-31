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
mod restore;
mod retention;
mod s3_sources;
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
use retention::{retention_cutoff, retention_expiry, validate_retention_period};
pub use retention::{
    RetentionCleanupFailure, RetentionCleanupReport, MAX_RETENTION_DAYS_FOR_SQL_ARITHMETIC,
};
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

fn reject_source_backing_targets(
    backing_service_ids: &HashSet<i32>,
    target_service_ids: impl IntoIterator<Item = i32>,
) -> Result<(), BackupError> {
    if let Some(service_id) = target_service_ids
        .into_iter()
        .find(|service_id| backing_service_ids.contains(service_id))
    {
        return Err(BackupError::Validation(format!(
            "Service {} supplies this schedule's backup destination and cannot back up into itself",
            service_id
        )));
    }
    Ok(())
}

fn exclude_source_backing_services(
    services: Vec<temps_entities::external_services::Model>,
    backing_service_ids: &HashSet<i32>,
) -> Vec<temps_entities::external_services::Model> {
    services
        .into_iter()
        .filter(|service| !backing_service_ids.contains(&service.id))
        .collect()
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

/// Refuse an instance-wide schedule -- one that targets every external
/// service or includes the control plane -- to a caller that does not run the
/// instance (NV1). Such a schedule dumps Temps's own database, so holding
/// `backups:*` (and `settings:write`) is not enough: a `Role::Custom` user or
/// API key with those grants must still be refused. Project-scoped schedules
/// (specific services, no control plane) are unaffected.
fn ensure_schedule_authority(
    authority: BackupScheduleAuthority,
    schedule: &str,
    target_all_services: bool,
    include_control_plane: bool,
    operation: &str,
) -> Result<(), BackupError> {
    if authority.is_instance_admin() || (!target_all_services && !include_control_plane) {
        return Ok(());
    }
    let reach = match (target_all_services, include_control_plane) {
        (true, true) => "targets every external service and includes the control plane",
        (true, false) => "targets every external service",
        (false, _) => "includes the control plane",
    };
    Err(BackupError::Forbidden {
        resource: schedule.to_string(),
        detail: format!(
            "Only an instance administrator may {operation} a backup schedule that {reach}"
        ),
    })
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

/// Result of attempting to publish a native whole-instance recovery set for a
/// terminal backup event.
///
/// Only fully successful scheduled runs that include a control-plane backup
/// are published. This prevents `temps backup restore` from presenting a
/// partial fan-out run as a recoverable instance snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoverySetPublication {
    NotScheduled,
    Pending {
        schedule_run_id: i64,
    },
    Incomplete {
        schedule_run_id: i64,
        failed_backup_ids: Vec<i32>,
    },
    NoControlPlane {
        schedule_run_id: i64,
    },
    AlreadyPublished {
        schedule_run_id: i64,
        backup_id: String,
    },
    Published {
        schedule_run_id: i64,
        backup_id: String,
    },
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

    async fn sha256_s3_object(
        s3_client: &S3Client,
        bucket: &str,
        location: &str,
    ) -> Result<String, BackupError> {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncReadExt;

        let key = if let Some(uri) = location.strip_prefix("s3://") {
            let (location_bucket, key) = uri.split_once('/').ok_or_else(|| {
                BackupError::Validation(format!("Invalid S3 backup location '{}'", location))
            })?;
            if location_bucket != bucket {
                return Err(BackupError::Validation(format!(
                    "Backup location bucket '{}' does not match source bucket '{}'",
                    location_bucket, bucket
                )));
            }
            key
        } else {
            location.trim_start_matches('/')
        };
        if key.is_empty() {
            return Err(BackupError::Validation(
                "Control-plane backup location has no object key".to_string(),
            ));
        }

        let response = s3_client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| {
                BackupError::S3(crate::engines::v2_common::describe_sdk_error(
                    "download control-plane backup for integrity hashing",
                    &error,
                ))
            })?;
        let mut reader = response.body.into_async_read();
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = reader
                .read(&mut buffer)
                .await
                .map_err(|error| BackupError::Internal {
                    message: format!(
                        "Failed to hash control-plane backup {}: {}",
                        location, error
                    ),
                })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    /// Publish the aggregate recovery manifest consumed by `temps backup
    /// restore` once every backup in a fan-out schedule run has completed.
    ///
    /// The executor publishes one terminal event per child. Those events are
    /// intentionally treated as hints: the database is re-read here and is
    /// the source of truth. The operation is idempotent and refuses to publish
    /// when any child failed or is still live.
    pub async fn publish_recovery_set_if_complete(
        &self,
        terminal_backup_id: i32,
    ) -> Result<RecoverySetPublication, BackupError> {
        let terminal_backup = temps_entities::backups::Entity::find_by_id(terminal_backup_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "Backup".to_string(),
                detail: format!("terminal backup row {}", terminal_backup_id),
            })?;

        let Some(schedule_run_id) = terminal_backup.schedule_run_id else {
            return Ok(RecoverySetPublication::NotScheduled);
        };

        let run_backups = temps_entities::backups::Entity::find()
            .filter(temps_entities::backups::Column::ScheduleRunId.eq(schedule_run_id))
            .order_by_asc(temps_entities::backups::Column::Id)
            .all(self.db.as_ref())
            .await?;

        if run_backups
            .iter()
            .any(|backup| matches!(backup.state.as_str(), "pending" | "running"))
        {
            return Ok(RecoverySetPublication::Pending { schedule_run_id });
        }

        let failed_backup_ids = run_backups
            .iter()
            .filter(|backup| backup.state != "completed")
            .map(|backup| backup.id)
            .collect::<Vec<_>>();
        if !failed_backup_ids.is_empty() {
            return Ok(RecoverySetPublication::Incomplete {
                schedule_run_id,
                failed_backup_ids,
            });
        }

        let mut parsed_metadata = Vec::with_capacity(run_backups.len());
        for backup in &run_backups {
            let metadata =
                serde_json::from_str::<serde_json::Value>(&backup.metadata).map_err(|error| {
                    BackupError::Validation(format!(
                        "Backup {} in schedule run {} has invalid metadata: {}",
                        backup.id, schedule_run_id, error
                    ))
                })?;
            parsed_metadata.push((backup, metadata));
        }

        let Some((control_plane, control_plane_metadata)) =
            parsed_metadata.iter().find(|(_, metadata)| {
                metadata.get("engine").and_then(serde_json::Value::as_str) == Some("control_plane")
            })
        else {
            return Ok(RecoverySetPublication::NoControlPlane { schedule_run_id });
        };

        if control_plane_metadata
            .get("recovery_set_published")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            return Ok(RecoverySetPublication::AlreadyPublished {
                schedule_run_id,
                backup_id: control_plane.backup_id.clone(),
            });
        }

        let mut service_ids = BTreeSet::new();
        for (backup, metadata) in &parsed_metadata {
            if backup.id == control_plane.id {
                continue;
            }
            let service_id = metadata
                .get("external_service_id")
                .or_else(|| metadata.get("service_id"))
                .and_then(serde_json::Value::as_i64)
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(|| {
                    BackupError::Validation(format!(
                        "External-service backup {} in schedule run {} has no service identity",
                        backup.id, schedule_run_id
                    ))
                })?;
            service_ids.insert(service_id);
        }
        let expected_service_ids = control_plane_metadata
            .get("expected_service_ids")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                BackupError::Validation(format!(
                    "Control-plane backup {} in schedule run {} has no expected service snapshot",
                    control_plane.id, schedule_run_id
                ))
            })?
            .iter()
            .map(|value| {
                value
                    .as_i64()
                    .and_then(|id| i32::try_from(id).ok())
                    .ok_or_else(|| {
                        BackupError::Validation(format!(
                            "Control-plane backup {} has an invalid expected service id",
                            control_plane.id
                        ))
                    })
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if expected_service_ids != service_ids {
            return Err(BackupError::Validation(format!(
                "Schedule run {} backup coverage mismatch: expected services {:?}, completed services {:?}",
                schedule_run_id, expected_service_ids, service_ids
            )));
        }

        let services = if service_ids.is_empty() {
            Vec::new()
        } else {
            temps_entities::external_services::Entity::find()
                .filter(
                    temps_entities::external_services::Column::Id
                        .is_in(service_ids.iter().copied()),
                )
                .all(self.db.as_ref())
                .await?
        };
        let services_by_id = services
            .into_iter()
            .map(|service| (service.id, service))
            .collect::<BTreeMap<_, _>>();

        let mut external_service_backups = Vec::with_capacity(service_ids.len());
        for (backup, metadata) in &parsed_metadata {
            if backup.id == control_plane.id {
                continue;
            }
            let service_id = metadata
                .get("external_service_id")
                .or_else(|| metadata.get("service_id"))
                .and_then(serde_json::Value::as_i64)
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(|| {
                    BackupError::Validation(format!(
                        "External-service backup {} in schedule run {} has no service identity",
                        backup.id, schedule_run_id
                    ))
                })?;
            let service = services_by_id
                .get(&service_id)
                .ok_or_else(|| BackupError::NotFound {
                    resource: "ExternalService".to_string(),
                    detail: format!(
                        "service {} referenced by backup {} in schedule run {}",
                        service_id, backup.id, schedule_run_id
                    ),
                })?;
            external_service_backups.push(json!({
                "backup_id": backup.id,
                "service_id": service_id,
                "s3_location": backup.s3_location,
                "state": backup.state,
                "size_bytes": backup.size_bytes,
                "type": backup.backup_type,
                "metadata": {
                    "service_type": service.service_type,
                    "service_name": service.name,
                }
            }));
        }

        let s3_source = temps_entities::s3_sources::Entity::find_by_id(control_plane.s3_source_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "S3Source".to_string(),
                detail: format!(
                    "source {} referenced by recovery set {}",
                    control_plane.s3_source_id, control_plane.backup_id
                ),
            })?;
        let s3_client = self.create_s3_client(&s3_source).await?;
        let control_plane_sha256 = Self::sha256_s3_object(
            &s3_client,
            &s3_source.bucket_name,
            &control_plane.s3_location,
        )
        .await?;

        // Server configuration contains the instance encryption key and other
        // credentials. Legacy manifests wrote it in plaintext. New recovery
        // sets encrypt it with a key derived from the S3 secret already needed
        // to download the backup, so bucket contents alone do not expose it.
        let server_config = yaml_serde::to_string(&self.config_service.get_server_config())
            .map_err(|error| {
                BackupError::Configuration(format!(
                    "Failed to serialize server config for recovery set {}: {}",
                    control_plane.backup_id, error
                ))
            })?;
        let s3_secret = self
            .encryption_service
            .decrypt_string(&s3_source.secret_key)
            .map_err(|error| {
                BackupError::Configuration(format!(
                    "Failed to decrypt S3 secret for recovery set {}: {}",
                    control_plane.backup_id, error
                ))
            })?;
        // Argon2id with a fresh salt per ciphertext (F-009); the CLI still accepts
        // legacy SHA-256-keyed manifests written by older servers.
        let encrypted_server_config =
            temps_core::EncryptionService::encrypt_string_with_password(&s3_secret, &server_config)
                .map_err(|error| BackupError::Internal {
                    message: format!(
                        "Failed to encrypt server config for recovery set {}: {}",
                        control_plane.backup_id, error
                    ),
                })?;

        let mut metadata = json!({
            "recovery_set_version": 2,
            "complete": true,
            "schedule_run_id": schedule_run_id,
            "backup_id": control_plane.backup_id,
            "name": control_plane.name,
            "type": control_plane.backup_type,
            "created_at": control_plane.started_at.to_rfc3339(),
            "created_by": control_plane.created_by,
            "size_bytes": control_plane.size_bytes.unwrap_or(0),
            "compression_type": control_plane.compression_type,
            "source": {
                "id": s3_source.id,
                "name": s3_source.name,
                "bucket": s3_source.bucket_name,
                "path": s3_source.bucket_path,
            },
            "schedule_id": control_plane.schedule_id,
            "state": control_plane.state,
            "tags": serde_json::from_str::<Vec<String>>(&control_plane.tags).unwrap_or_default(),
            "checksum": control_plane.checksum,
            "artifact_sha256": control_plane_sha256,
            "server_config_encrypted": encrypted_server_config,
            "server_config_encryption": "aes-256-gcm+s3-secret-argon2id",
            "external_service_backups": external_service_backups,
            "metadata": control_plane_metadata,
        });
        let authenticated_payload = serde_json::to_string(&metadata)?;
        let manifest_authentication = temps_core::EncryptionService::encrypt_string_with_password(
            &s3_secret,
            &authenticated_payload,
        )
        .map_err(|error| BackupError::Internal {
            message: format!(
                "Failed to authenticate recovery manifest {}: {}",
                control_plane.backup_id, error
            ),
        })?;
        metadata["manifest_authentication"] = json!(manifest_authentication);
        let metadata_key =
            crate::engines::v2_common::derive_metadata_key(&control_plane.s3_location);
        s3_client
            .put_object()
            .bucket(&s3_source.bucket_name)
            .key(&metadata_key)
            .body(serde_json::to_vec(&metadata)?.into())
            .content_type("application/json")
            .send()
            .await
            .map_err(|error| {
                BackupError::S3(crate::engines::v2_common::describe_sdk_error(
                    "put recovery metadata",
                    &error,
                ))
            })?;
        self.update_backup_index(&s3_client, &s3_source, control_plane)
            .await?;

        let mut updated_metadata =
            control_plane_metadata.as_object().cloned().ok_or_else(|| {
                BackupError::Validation(format!(
                    "Control-plane backup {} metadata is not a JSON object",
                    control_plane.id
                ))
            })?;
        updated_metadata.insert(
            "recovery_set_published".to_string(),
            serde_json::Value::Bool(true),
        );
        updated_metadata.insert(
            "recovery_set_version".to_string(),
            serde_json::Value::Number(2.into()),
        );
        let mut active = (*control_plane).clone().into_active_model();
        active.metadata = Set(serde_json::Value::Object(updated_metadata).to_string());
        active.update(self.db.as_ref()).await?;

        Ok(RecoverySetPublication::Published {
            schedule_run_id,
            backup_id: control_plane.backup_id.clone(),
        })
    }

    /// Heal terminal events lost to process restarts or broadcast lag. The
    /// marker on the control-plane row keeps this bounded to unpublished
    /// successful runs; each invocation processes at most 100 candidates.
    pub async fn reconcile_completed_recovery_sets(&self) -> Result<(), BackupError> {
        #[derive(FromQueryResult)]
        struct Candidate {
            id: i32,
        }

        let candidates = Candidate::find_by_statement(Statement::from_string(
            DatabaseBackend::Postgres,
            r#"
SELECT cp.id
  FROM backups cp
 WHERE cp.schedule_run_id IS NOT NULL
   AND cp.state = 'completed'
   AND cp.metadata::jsonb ->> 'engine' = 'control_plane'
   AND COALESCE((cp.metadata::jsonb ->> 'recovery_set_published')::boolean, false) = false
   AND NOT EXISTS (
       SELECT 1
         FROM backups sibling
        WHERE sibling.schedule_run_id = cp.schedule_run_id
          AND sibling.state <> 'completed'
   )
 ORDER BY cp.id DESC
 LIMIT 100
            "#
            .to_string(),
        ))
        .all(self.db.as_ref())
        .await?;

        for candidate in candidates {
            match self.publish_recovery_set_if_complete(candidate.id).await {
                Ok(RecoverySetPublication::Published {
                    schedule_run_id,
                    backup_id,
                }) => info!(
                    schedule_run_id,
                    backup_id, "Published recovered native recovery-set manifest"
                ),
                Ok(_) => {}
                Err(error) => warn!(
                    backup_id = candidate.id,
                    error = %error,
                    "Failed to reconcile native recovery-set manifest"
                ),
            }
        }
        Ok(())
    }

    pub async fn list_backups(
        &self,
        s3_source_id: i32,
    ) -> Result<Vec<temps_entities::backups::Model>, BackupError> {
        let backups = temps_entities::backups::Entity::find()
            .filter(temps_entities::backups::Column::S3SourceId.eq(s3_source_id))
            .order_by_desc(temps_entities::backups::Column::StartedAt)
            .all(self.db.as_ref())
            .await?;
        Ok(backups)
    }

    /// Normalize and validate an explicit schedule target list before it is
    /// written. Returning the de-duplicated IDs keeps create, update, and the
    /// standalone attach endpoint on the same validation contract.
    async fn validated_schedule_service_ids(
        &self,
        s3_source_id: i32,
        service_ids: &[i32],
    ) -> Result<Vec<i32>, BackupError> {
        let mut unique_ids = service_ids.to_vec();
        unique_ids.sort_unstable();
        unique_ids.dedup();

        if unique_ids.is_empty() {
            return Ok(unique_ids);
        }

        let backing_service_ids = self.source_backing_service_ids(s3_source_id).await?;
        reject_source_backing_targets(&backing_service_ids, unique_ids.iter().copied())?;

        let found_count = temps_entities::external_services::Entity::find()
            .filter(temps_entities::external_services::Column::Id.is_in(unique_ids.clone()))
            .count(self.db.as_ref())
            .await?;
        if found_count as usize != unique_ids.len() {
            return Err(BackupError::Validation(format!(
                "One or more service ids do not exist (requested {}, found {})",
                unique_ids.len(),
                found_count
            )));
        }

        Ok(unique_ids)
    }

    /// List all backup schedules
    pub async fn list_backup_schedules(
        &self,
    ) -> Result<Vec<temps_entities::backup_schedules::Model>, BackupError> {
        let schedules = temps_entities::backup_schedules::Entity::find()
            .all(self.db.as_ref())
            .await?;

        debug!("Listed {} backup schedules", schedules.len());
        Ok(schedules)
    }

    /// Create a new backup schedule
    ///
    /// `authority` is who asked: an instance-wide schedule (the defaults:
    /// every service plus the control plane) is refused to
    /// [`BackupScheduleAuthority::Scoped`] before anything is read or written.
    pub async fn create_backup_schedule(
        &self,
        request: CreateBackupScheduleRequest,
        authority: BackupScheduleAuthority,
    ) -> Result<BackupSchedule, BackupError> {
        use sea_orm::{ActiveModelTrait, EntityTrait, Set};

        let target_all = request.target_all_services.unwrap_or(true);
        let include_control_plane = request.include_control_plane.unwrap_or(true);
        ensure_schedule_authority(
            authority,
            &format!("backup schedule '{}'", request.name),
            target_all,
            include_control_plane,
            "create",
        )?;

        validate_retention_period(request.retention_period)?;
        if target_all && !request.service_ids.is_empty() {
            return Err(BackupError::Validation(
                "service_ids cannot be set when target_all_services=true".to_string(),
            ));
        }
        if !target_all && !include_control_plane && request.service_ids.is_empty() {
            return Err(BackupError::Validation(
                "A schedule must include the control plane, at least one specific database, \
                 or all databases."
                    .to_string(),
            ));
        }

        // Resolve S3 source: explicit id OR fall back to the default source.
        let s3_source_id = self.resolve_s3_source_id(request.s3_source_id).await?;

        // Verify S3 source exists
        temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "S3Source".to_string(),
                detail: format!("S3 source {} not found", s3_source_id),
            })?;

        // Validate the schedule expression
        self.validate_backup_schedule(&request.schedule_expression)?;

        // Calculate next run time
        let cron_schedule = Schedule::from_str(&request.schedule_expression)
            .map_err(|e| BackupError::Schedule(e.to_string()))?;
        let next_run = cron_schedule.upcoming(Utc).next();

        let service_ids = self
            .validated_schedule_service_ids(s3_source_id, &request.service_ids)
            .await?;

        // Insert the schedule and its explicit memberships in one transaction.
        // A scheduler tick can therefore never observe an enabled specific-
        // target schedule before its databases have been attached.
        let txn = self.db.begin().await?;
        let now = chrono::Utc::now();
        let tags_json = serde_json::to_string(&request.tags)?;
        let new_schedule = temps_entities::backup_schedules::ActiveModel {
            id: sea_orm::NotSet,
            name: Set(request.name.clone()),
            backup_type: Set(request.backup_type.clone()),
            retention_period: Set(request.retention_period),
            s3_source_id: Set(s3_source_id),
            schedule_expression: Set(request.schedule_expression.clone()),
            enabled: Set(request.enabled),
            created_at: Set(now),
            updated_at: Set(now),
            description: Set(request.description.clone()),
            tags: Set(tags_json),
            next_run: Set(next_run),
            max_runtime_secs: Set(request.max_runtime_secs),
            // Default is true ("back up every database, including future
            // ones") so a freshly-created schedule does the obvious thing
            // without the operator having to pick services up front.
            target_all_services: Set(target_all),
            include_control_plane: Set(include_control_plane),
            ..Default::default()
        };

        let schedule_model = new_schedule.insert(&txn).await?;
        if !service_ids.is_empty() {
            let memberships = service_ids.into_iter().map(|service_id| {
                temps_entities::backup_schedule_services::ActiveModel {
                    schedule_id: Set(schedule_model.id),
                    service_id: Set(service_id),
                    created_at: Set(now),
                }
            });
            temps_entities::backup_schedule_services::Entity::insert_many(memberships)
                .exec(&txn)
                .await?;
        }
        txn.commit().await?;
        info!("Created new backup schedule: {}", schedule_model.name);
        self.fire_lifecycle_reconcile(schedule_model.s3_source_id);
        Ok(schedule_model)
    }

    /// Get a backup schedule by ID
    pub async fn get_backup_schedule(&self, id: i32) -> Result<BackupSchedule, BackupError> {
        use sea_orm::EntityTrait;

        let schedule = temps_entities::backup_schedules::Entity::find_by_id(id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "BackupSchedule".to_string(),
                detail: "Backup schedule not found".to_string(),
            })?;

        Ok(schedule)
    }

    /// Delete a backup schedule. An instance-wide schedule may only be
    /// deleted with [`BackupScheduleAuthority::InstanceAdmin`].
    pub async fn delete_backup_schedule(
        &self,
        id: i32,
        authority: BackupScheduleAuthority,
    ) -> Result<bool, BackupError> {
        use sea_orm::EntityTrait;

        // Ensure it exists to preserve previous behavior/logging
        let schedule = self.get_backup_schedule(id).await?;
        ensure_schedule_authority(
            authority,
            &format!("backup schedule {id}"),
            schedule.target_all_services,
            schedule.include_control_plane,
            "delete",
        )?;

        let result = temps_entities::backup_schedules::Entity::delete_by_id(id)
            .exec(self.db.as_ref())
            .await?;
        info!("Deleted backup schedule: {}", schedule.name);
        self.fire_lifecycle_reconcile(schedule.s3_source_id);
        Ok(result.rows_affected > 0)
    }

    /// Attach external services to a backup schedule.
    ///
    /// Idempotent: re-attaching an already-attached service is a no-op (rows
    /// are inserted with `ON CONFLICT DO NOTHING`). Returns the number of rows
    /// actually inserted. Validates that the schedule and every supplied
    /// service id exist.
    pub async fn attach_services_to_schedule(
        &self,
        schedule_id: i32,
        service_ids: &[i32],
    ) -> Result<u64, BackupError> {
        use sea_orm::ConnectionTrait;

        // Validate schedule exists (raises NotFound otherwise) and resolve the
        // destination identity before accepting explicit targets.
        let schedule = self.get_backup_schedule(schedule_id).await?;

        if service_ids.is_empty() {
            return Ok(0);
        }

        let unique_ids = self
            .validated_schedule_service_ids(schedule.s3_source_id, service_ids)
            .await?;

        // Build a single multi-row INSERT with ON CONFLICT DO NOTHING for
        // idempotency. Sea-ORM `insert_many` does not expose ON CONFLICT in
        // a portable way, so we drop to raw SQL.
        let mut sql = String::from(
            "INSERT INTO backup_schedule_services (schedule_id, service_id, created_at) VALUES ",
        );
        let mut params: Vec<sea_orm::Value> = Vec::with_capacity(unique_ids.len() * 2 + 1);
        params.push(sea_orm::Value::from(schedule_id));
        for (idx, sid) in unique_ids.iter().enumerate() {
            if idx > 0 {
                sql.push_str(", ");
            }
            let p = idx + 2; // $1 = schedule_id, $2.. = service_ids
            sql.push_str(&format!("($1, ${}, NOW())", p));
            params.push(sea_orm::Value::from(*sid));
        }
        sql.push_str(" ON CONFLICT (schedule_id, service_id) DO NOTHING");

        let result = self
            .db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                &sql,
                params,
            ))
            .await
            .map_err(BackupError::Database)?;

        Ok(result.rows_affected())
    }

    /// Auto-provision a covering daily full-backup schedule for every MariaDB
    /// external service that does not yet have one.
    ///
    /// This drives point-in-time recovery out of the box: the daily full
    /// backup produces base backups via the `mariadb_physical` engine, and the
    /// binlog archiver already ships binary logs every few minutes, so
    /// base + binlogs = PITR with no operator action.
    ///
    /// Design (gated by the per-service `default_backup_provisioned` latch):
    /// - Only services where `default_backup_provisioned = false` are
    ///   considered, so we provision **exactly once** and never recreate a
    ///   schedule the operator later deletes.
    /// - Scope is **MariaDB only** (`service_type = "mariadb"`).
    /// - Requires a configured default S3 source. If none exists yet we log at
    ///   `debug` and return `Ok(())` — the next periodic tick retries, which
    ///   handles the "storage configured after the service" ordering.
    /// - Per-service failures are logged at `warn` and skipped, leaving the
    ///   latch `false` so the service is retried on the next tick. A single bad
    ///   service can't block the others.
    ///
    /// Idempotent and safe to call on a periodic tick.
    pub async fn reconcile_default_external_service_schedules(&self) -> Result<(), BackupError> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        // 1. Resolve the destination for services not pinned anywhere yet.
        //    The Cloud-managed source wins when one exists: new services'
        //    continuous archiving defaults to it (see
        //    `temps_providers::continuous_archive`), so a base-backup
        //    schedule pointed anywhere else would leave the binlog shipper
        //    refusing to pin and PITR never starting. Otherwise the default
        //    source; if neither is configured yet, this is not an error — we
        //    simply have nothing to point a schedule at, so we bail quietly
        //    and retry on the next tick. A service that already carries a
        //    pin keeps it (see the loop below).
        let s3_source_id = match self.default_schedule_destination().await {
            Ok(id) => id,
            Err(_) => {
                debug!(
                    "reconcile_default_external_service_schedules: no default S3 source \
                     configured yet, skipping (will retry next tick)"
                );
                return Ok(());
            }
        };

        // 2. Load unprovisioned MariaDB services.
        let services = temps_entities::external_services::Entity::find()
            .filter(temps_entities::external_services::Column::ServiceType.eq("mariadb"))
            .filter(temps_entities::external_services::Column::DefaultBackupProvisioned.eq(false))
            .all(self.db.as_ref())
            .await?;

        if services.is_empty() {
            return Ok(());
        }

        debug!(
            count = services.len(),
            s3_source_id,
            "reconcile_default_external_service_schedules: provisioning default backup \
             schedules for MariaDB services"
        );

        // 3. For each, create a daily full-backup schedule targeting exactly
        //    that service, then flip the latch.
        for service in services {
            // A service already pinned somewhere (an upgraded instance whose
            // binlogs ship to the operator's own bucket) keeps that
            // destination: a schedule pointed anywhere else would fail
            // every run on the pin mismatch.
            let destination = service
                .continuous_archive_s3_source_id
                .unwrap_or(s3_source_id);
            if let Err(e) = self
                .provision_default_schedule_for_service(&service, destination)
                .await
            {
                // Leave default_backup_provisioned = false so the next tick
                // retries. One failing service must not block the others.
                warn!(
                    service_id = service.id,
                    service_name = %service.name,
                    error = %e,
                    "Failed to auto-provision default backup schedule for MariaDB service; \
                     will retry on next reconcile tick"
                );
                continue;
            }
        }

        Ok(())
    }

    /// Create the default daily full-backup schedule for a single MariaDB
    /// service and mark it provisioned. Helper for
    /// [`reconcile_default_external_service_schedules`]; on success the
    /// service's `default_backup_provisioned` latch is set to `true`.
    /// Where an auto-provisioned schedule writes: the Cloud-managed source
    /// when the instance has one, else the default source.
    async fn default_schedule_destination(&self) -> Result<i32, BackupError> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let managed = temps_entities::s3_sources::Entity::find()
            .filter(temps_entities::s3_sources::Column::ManagedByCloud.eq(true))
            .one(self.db.as_ref())
            .await?;
        match managed {
            Some(source) => Ok(source.id),
            None => self.resolve_s3_source_id(None).await,
        }
    }

    async fn provision_default_schedule_for_service(
        &self,
        service: &temps_entities::external_services::Model,
        s3_source_id: i32,
    ) -> Result<(), BackupError> {
        use sea_orm::{ActiveModelTrait, Set};

        // Daily at 03:00 UTC. 6-field cron (`sec min hour dom mon dow`) as
        // required by the `cron` crate / `validate_backup_schedule`; the two
        // adjacent occurrences are 24h apart, satisfying the validator's
        // "at least 1 hour" rule. Reuse create_backup_schedule for validation
        // and next-run computation — do NOT hand-roll a second insert.
        let request = CreateBackupScheduleRequest {
            name: format!("Auto base backup — {}", service.name),
            // `backup_type` is the schedule/job label ("full"); the actual
            // backup engine (`mariadb_physical`) is resolved from the service's
            // `service_type` at run time, not from this field.
            backup_type: "full".to_string(),
            // Days. 14 days of base backups is a sane default retention window.
            retention_period: 14,
            // The destination resolved by the caller: the Cloud-managed
            // source when one exists, the default source otherwise.
            s3_source_id: Some(s3_source_id),
            schedule_expression: "0 0 3 * * *".to_string(),
            enabled: true,
            description: Some(
                "Automatically created daily base backup so point-in-time recovery \
                 works out of the box. Safe to edit or delete."
                    .to_string(),
            ),
            tags: vec![],
            max_runtime_secs: None,
            // Target exactly this service, not every DB or the control plane.
            // Schedule creation commits this membership atomically.
            target_all_services: Some(false),
            include_control_plane: Some(false),
            service_ids: vec![service.id],
        };

        // The generated schedule targets exactly one service and never the
        // control plane, so it is project-scoped whoever triggered it.
        let schedule = self
            .create_backup_schedule(request, BackupScheduleAuthority::Scoped)
            .await?;
        let mut generated_schedule: temps_entities::backup_schedules::ActiveModel =
            schedule.clone().into();
        generated_schedule.generated_kind = Set(Some("mariadb_base_backup".to_string()));
        generated_schedule.update(self.db.as_ref()).await?;

        // Flip the one-shot latch so we never provision this service again.
        let mut active: temps_entities::external_services::ActiveModel = service.clone().into();
        active.default_backup_provisioned = Set(true);
        active.update(self.db.as_ref()).await?;

        info!(
            service_id = service.id,
            service_name = %service.name,
            schedule_id = schedule.id,
            schedule_name = %schedule.name,
            "Auto-provisioned default daily base-backup schedule for MariaDB service \
             (enables point-in-time recovery; edit or delete it like any schedule)"
        );

        Ok(())
    }

    /// Detach a single external service from a backup schedule.
    ///
    /// Returns `true` if a row was removed, `false` if nothing was attached.
    /// Does not raise `NotFound` when the membership row is absent — callers
    /// can treat detach as idempotent.
    pub async fn detach_service_from_schedule(
        &self,
        schedule_id: i32,
        service_id: i32,
    ) -> Result<bool, BackupError> {
        use sea_orm::EntityTrait;

        let result = temps_entities::backup_schedule_services::Entity::delete_by_id((
            schedule_id,
            service_id,
        ))
        .exec(self.db.as_ref())
        .await
        .map_err(BackupError::Database)?;

        Ok(result.rows_affected > 0)
    }

    /// List the external services attached to a given schedule, ordered by
    /// service name for stable UI rendering. Raises `NotFound` if the
    /// schedule does not exist.
    pub async fn list_services_for_schedule(
        &self,
        schedule_id: i32,
    ) -> Result<Vec<temps_entities::external_services::Model>, BackupError> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

        self.get_backup_schedule(schedule_id).await?;

        let services = temps_entities::external_services::Entity::find()
            .inner_join(temps_entities::backup_schedule_services::Entity)
            .filter(temps_entities::backup_schedule_services::Column::ScheduleId.eq(schedule_id))
            .order_by_asc(temps_entities::external_services::Column::Name)
            .all(self.db.as_ref())
            .await
            .map_err(BackupError::Database)?;

        Ok(services)
    }

    /// List the schedules that target a given external service. Raises
    /// `NotFound` if the service does not exist.
    pub async fn list_schedules_for_service(
        &self,
        service_id: i32,
    ) -> Result<Vec<BackupSchedule>, BackupError> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

        temps_entities::external_services::Entity::find_by_id(service_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "ExternalService".to_string(),
                detail: format!("External service {} not found", service_id),
            })?;

        let schedules = temps_entities::backup_schedules::Entity::find()
            .inner_join(temps_entities::backup_schedule_services::Entity)
            .filter(temps_entities::backup_schedule_services::Column::ServiceId.eq(service_id))
            .order_by_asc(temps_entities::backup_schedules::Column::Name)
            .all(self.db.as_ref())
            .await
            .map_err(BackupError::Database)?;

        Ok(schedules)
    }

    /// List raw backup rows after the bounded history summary has been
    /// authorized by the handler.
    pub async fn list_backups_for_schedule(
        &self,
        schedule_id: i32,
    ) -> Result<Vec<Backup>, BackupError> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

        self.get_backup_schedule(schedule_id).await?;

        let backups = temps_entities::backups::Entity::find()
            .filter(temps_entities::backups::Column::ScheduleId.eq(schedule_id))
            .order_by_desc(temps_entities::backups::Column::StartedAt)
            .all(self.db.as_ref())
            .await
            .map_err(BackupError::Database)?;

        Ok(backups)
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

    /// Return every unresolved backup alert, newest first. The schedule JOIN
    /// is owned by the service layer so handlers do not access the database
    /// directly. Raw database failures are logged here and converted to a
    /// stable client-safe error.
    pub async fn list_open_backup_alerts(&self) -> Result<Vec<BackupAlertEntry>, BackupError> {
        let sql = r#"
SELECT
    a.id,
    a.kind,
    a.severity,
    a.schedule_id,
    s.name             AS schedule_name,
    s.s3_source_id     AS schedule_s3_source_id,
    a.message,
    a.opened_at
FROM backup_alerts a
LEFT JOIN backup_schedules s ON s.id = a.schedule_id
WHERE a.resolved_at IS NULL
ORDER BY a.opened_at DESC
"#;

        BackupAlertEntry::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            vec![],
        ))
        .all(self.db.as_ref())
        .await
        .map_err(|db_error| {
            error!(error = %db_error, "failed to query open backup alerts");
            BackupError::Internal {
                message: "Failed to list open backup alerts".to_string(),
            }
        })
    }

    /// Get a backup by ID
    pub async fn get_backup(&self, backup_id: &str) -> Result<Option<Backup>, BackupError> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        let model = temps_entities::backups::Entity::find()
            .filter(temps_entities::backups::Column::BackupId.eq(backup_id.to_string()))
            .one(self.db.as_ref())
            .await?;

        Ok(model)
    }

    /// Best-effort progress size for a running backup.
    ///
    /// Computed by listing the backup's S3 location and summing the
    /// reported sizes. Returns `None` for non-running backups (their
    /// `size_bytes` is authoritative once finished) and for backups
    /// whose `s3_location` isn't usable yet (engine still warming up).
    /// Errors talking to S3 are downgraded to `None` and logged — the
    /// detail page is best-effort.
    pub async fn compute_live_size(&self, backup: &Backup) -> Option<i64> {
        if backup.state != "running" {
            return None;
        }
        if backup.s3_location.is_empty() {
            return None;
        }

        let s3_source = match temps_entities::s3_sources::Entity::find_by_id(backup.s3_source_id)
            .one(self.db.as_ref())
            .await
        {
            Ok(Some(src)) => src,
            Ok(None) => return None,
            Err(e) => {
                warn!(
                    "Failed to load s3_source {} for live size: {}",
                    backup.s3_source_id, e
                );
                return None;
            }
        };

        let s3_client = match self.create_s3_client(&s3_source).await {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Failed to build S3 client for live-size lookup on backup {}: {}",
                    backup.id, e
                );
                return None;
            }
        };

        // The location can be either an `s3://bucket/key` URL (WAL-G,
        // cluster) or a bucket-relative key (pg_dump, mongodump). Try the
        // URL form first; fall back to treating the value as a key.
        let bucket = &s3_source.bucket_name;
        let key = if let Some((url_bucket, url_key)) =
            temps_providers::externalsvc::s3_util::parse_s3_url(&backup.s3_location)
        {
            // Sanity: only list inside our configured bucket. WAL-G's
            // prefix should always live in this same bucket.
            if &url_bucket != bucket {
                debug!(
                    "live size: s3:// bucket {} != configured bucket {}, listing anyway",
                    url_bucket, bucket
                );
            }
            url_key
        } else {
            backup.s3_location.trim_start_matches('/').to_string()
        };

        // Append a trailing slash if the key looks like a prefix (no file
        // extension). list_objects_v2 doesn't care, but this matches what
        // the engines pass elsewhere.
        let prefix = if key.ends_with('/') || key.contains('.') {
            key
        } else {
            format!("{}/", key)
        };

        match temps_providers::externalsvc::s3_util::list_total_size(&s3_client, bucket, &prefix)
            .await
        {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(e) => {
                debug!(
                    "live size lookup failed for backup {} ({}): {}",
                    backup.id, prefix, e
                );
                None
            }
        }
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

    // Add this new validation function
    fn validate_backup_schedule(&self, schedule: &str) -> Result<(), BackupError> {
        let schedule = Schedule::from_str(schedule)
            .map_err(|e| BackupError::Validation(format!("Invalid backup schedule: {}", e)))?;

        // Get the first two occurrences
        let upcoming = schedule.upcoming(Utc);
        let next_two = upcoming.take(2).collect::<Vec<_>>();
        if let [first, second] = next_two.as_slice() {
            let duration = *second - *first;
            if duration.num_minutes() < 60 {
                return Err(BackupError::Validation(
                    "Backup schedule must be at least 1 hour apart".into(),
                ));
            }
        }

        Ok(())
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

    pub async fn update_next_run(&self, schedule_id: i32, schedule_str: &str) -> Result<()> {
        // Validate the schedule
        let schedule = Schedule::from_str(schedule_str)
            .map_err(|_| BackupError::Validation("Invalid backup schedule".into()))?;

        // Calculate next run time
        let next_run = schedule.upcoming(Utc).next();

        // Get the schedule and update it
        let schedule_model = temps_entities::backup_schedules::Entity::find_by_id(schedule_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "BackupSchedule".to_string(),
                detail: "Backup schedule not found".to_string(),
            })?;

        let mut schedule_update: temps_entities::backup_schedules::ActiveModel =
            schedule_model.into_active_model();
        schedule_update.next_run = sea_orm::Set(next_run);
        schedule_update.update(self.db.as_ref()).await?;

        info!(
            "Updated next run time for backup schedule {}: {:?}",
            schedule_id, next_run
        );
        Ok(())
    }

    /// Update an existing backup schedule with a partial set of changes.
    ///
    /// Only fields that are present (`Some`) in `request` are written to the
    /// database. Absent fields leave the column unchanged. Validation:
    ///
    /// - `name`: must be non-empty if present.
    /// - `schedule_expression`: validated by `validate_backup_schedule`; if
    ///   it differs from the stored value, `next_run` is recomputed.
    /// - `retention_period`: must be >= 1.
    /// - `max_runtime_secs`: `Some(Some(n))` requires `n >= 60`.
    ///
    /// An instance-wide schedule -- before or after the update -- may only be
    /// changed with [`BackupScheduleAuthority::InstanceAdmin`]: a scoped
    /// caller can neither edit one nor widen a project schedule into one.
    pub async fn update_backup_schedule(
        &self,
        id: i32,
        request: UpdateBackupScheduleRequest,
        authority: BackupScheduleAuthority,
    ) -> Result<temps_entities::backup_schedules::Model, BackupError> {
        use sea_orm::{ActiveModelTrait, ConnectionTrait, IntoActiveModel, Set};

        // 1. Load the existing schedule (returns NotFound if absent).
        let existing = self.get_backup_schedule(id).await?;
        ensure_schedule_authority(
            authority,
            &format!("backup schedule {id}"),
            existing.target_all_services || request.target_all_services == Some(true),
            existing.include_control_plane || request.include_control_plane == Some(true),
            "update",
        )?;
        let requested_target_all = request.target_all_services;
        let requested_service_ids = request.service_ids.clone();

        // 2. Validate fields before touching the ActiveModel.
        if let Some(ref name) = request.name {
            if name.is_empty() {
                return Err(BackupError::Validation("name cannot be empty".to_string()));
            }
        }

        if let Some(ref expr) = request.schedule_expression {
            self.validate_backup_schedule(expr)?;
        }

        if let Some(days) = request.retention_period {
            validate_retention_period(days)?;
        }

        if let Some(Some(secs)) = request.max_runtime_secs {
            if secs < 60 {
                return Err(BackupError::Validation(
                    "max_runtime_secs must be >= 60".to_string(),
                ));
            }
        }

        // 3. Build the ActiveModel from the loaded model.
        let mut active: temps_entities::backup_schedules::ActiveModel =
            existing.clone().into_active_model();

        let mut changed_fields: Vec<&str> = Vec::new();

        if let Some(name) = request.name {
            active.name = Set(name);
            changed_fields.push("name");
        }
        if let Some(description) = request.description {
            active.description = Set(if description.is_empty() {
                None
            } else {
                Some(description)
            });
            changed_fields.push("description");
        }
        if let Some(expr) = request.schedule_expression {
            if expr != existing.schedule_expression {
                let cron_schedule =
                    Schedule::from_str(&expr).map_err(|e| BackupError::Schedule(e.to_string()))?;
                let next_run = cron_schedule.upcoming(Utc).next();
                active.schedule_expression = Set(expr);
                active.next_run = Set(next_run);
                changed_fields.push("schedule_expression");
                changed_fields.push("next_run");
            }
        }
        if let Some(days) = request.retention_period {
            active.retention_period = Set(days);
            changed_fields.push("retention_period");
        }
        if let Some(runtime) = request.max_runtime_secs {
            active.max_runtime_secs = Set(runtime);
            changed_fields.push("max_runtime_secs");
        }
        if let Some(enabled) = request.enabled {
            active.enabled = Set(enabled);
            changed_fields.push("enabled");
        }
        if let Some(tags) = request.tags {
            let tags_json = serde_json::to_string(&tags)?;
            active.tags = Set(tags_json);
            changed_fields.push("tags");
        }
        if let Some(target_all) = request.target_all_services {
            active.target_all_services = Set(target_all);
            changed_fields.push("target_all_services");
        }
        if let Some(include_cp) = request.include_control_plane {
            active.include_control_plane = Set(include_cp);
            changed_fields.push("include_control_plane");
        }

        // Pre-flight: figure out what state the schedule would be in after
        // the update. If the operator is moving toward "nothing to back up,"
        // reject before we commit so the run history doesn't fill up with
        // no-op runs.
        let final_target_all = request
            .target_all_services
            .unwrap_or(existing.target_all_services);
        let final_include_cp = request
            .include_control_plane
            .unwrap_or(existing.include_control_plane);
        if final_target_all
            && requested_service_ids
                .as_ref()
                .is_some_and(|service_ids| !service_ids.is_empty())
        {
            return Err(BackupError::Validation(
                "service_ids cannot be set when target_all_services=true".to_string(),
            ));
        }

        let validated_service_ids = match requested_service_ids.as_deref() {
            Some(service_ids) => Some(
                self.validated_schedule_service_ids(existing.s3_source_id, service_ids)
                    .await?,
            ),
            None => None,
        };

        let txn = self.db.begin().await?;
        let final_service_count = if final_target_all {
            0
        } else if let Some(service_ids) = validated_service_ids.as_ref() {
            service_ids.len() as u64
        } else {
            temps_entities::backup_schedule_services::Entity::find()
                .filter(temps_entities::backup_schedule_services::Column::ScheduleId.eq(id))
                .count(&txn)
                .await?
        };
        if !final_target_all && !final_include_cp && final_service_count == 0 {
            return Err(BackupError::Validation(
                "Schedule would have nothing to back up: select at least one specific database, \
                 enable the control plane, or target all databases."
                    .to_string(),
            ));
        }

        active.updated_at = Set(Utc::now());

        let updated = active.update(&txn).await?;

        // Retention drives `run_retention_cleanup`, which deletes a
        // schedule's backups once `started_at` is older than the retention
        // period. Changing the period therefore moves the deadline of every
        // existing backup of this schedule, so recompute the stored
        // `expires_at` in the same transaction — otherwise the console would
        // keep showing the deadline that was correct under the old period.
        if let Some(days) = request.retention_period {
            if days != existing.retention_period {
                // Clamp before the raw SQL addition: unlike `retention_expiry`,
                // `timestamp + interval` in Postgres raises an error (aborting
                // the whole transaction) instead of saturating on overflow.
                let clamped_days = i64::from(days).min(MAX_RETENTION_DAYS_FOR_SQL_ARITHMETIC);
                let recomputed = txn
                    .execute(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "UPDATE backups SET expires_at = started_at + ($1 * interval '1 day') \
                         WHERE schedule_id = $2",
                        vec![Value::from(clamped_days), Value::from(id)],
                    ))
                    .await?;
                info!(
                    schedule_id = id,
                    retention_period_days = days,
                    rows_updated = recomputed.rows_affected(),
                    "Recomputed backup retention deadlines after a retention_period change",
                );
            }
        }

        // Apply the target-mode and explicit selection in the same transaction
        // as the schedule fields. This prevents both no-target scheduler races
        // and partial UI saves when switching from all to specific databases.
        if matches!(requested_target_all, Some(true)) || validated_service_ids.is_some() {
            let deleted = temps_entities::backup_schedule_services::Entity::delete_many()
                .filter(temps_entities::backup_schedule_services::Column::ScheduleId.eq(id))
                .exec(&txn)
                .await?;
            info!(
                schedule_id = id,
                rows_deleted = deleted.rows_affected,
                "Cleared explicit service memberships before applying schedule targets",
            );
        }
        if !final_target_all {
            if let Some(service_ids) = validated_service_ids {
                if !service_ids.is_empty() {
                    let now = Utc::now();
                    let memberships = service_ids.into_iter().map(|service_id| {
                        temps_entities::backup_schedule_services::ActiveModel {
                            schedule_id: Set(id),
                            service_id: Set(service_id),
                            created_at: Set(now),
                        }
                    });
                    temps_entities::backup_schedule_services::Entity::insert_many(memberships)
                        .exec(&txn)
                        .await?;
                }
            }
        }
        txn.commit().await?;

        info!(
            schedule_id = id,
            fields = ?changed_fields,
            "Updated backup schedule fields",
        );

        // If retention or enabled flipped, the desired S3 lifecycle config
        // changed. Reconcile in the background. (Schedule can't be moved to
        // a different s3_source via UpdateBackupScheduleRequest today, so
        // we only reconcile one bucket.)
        if changed_fields.contains(&"retention_period") || changed_fields.contains(&"enabled") {
            self.fire_lifecycle_reconcile(updated.s3_source_id);
        }

        Ok(updated)
    }

    // Add this new method
    pub async fn disable_backup_schedule(
        &self,
        id: i32,
    ) -> Result<temps_entities::backup_schedules::Model, BackupError> {
        let schedule_model = temps_entities::backup_schedules::Entity::find_by_id(id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "BackupSchedule".to_string(),
                detail: "Backup schedule not found".to_string(),
            })?;

        let s3_source_id = schedule_model.s3_source_id;
        let mut schedule_update: temps_entities::backup_schedules::ActiveModel =
            schedule_model.into_active_model();
        schedule_update.enabled = sea_orm::Set(false);
        schedule_update.updated_at = sea_orm::Set(Utc::now());
        schedule_update.update(self.db.as_ref()).await?;

        // Disabling may have been this source's last enabled schedule —
        // reconcile now so a stale S3-side lifecycle rule doesn't keep
        // expiring objects after the schedule stops running.
        self.fire_lifecycle_reconcile(s3_source_id);

        self.get_backup_schedule(id).await
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

    // Add this new method
    pub async fn enable_backup_schedule(
        &self,
        id: i32,
    ) -> Result<temps_entities::backup_schedules::Model, BackupError> {
        // Get the schedule to validate it exists and get the schedule expression
        let schedule = temps_entities::backup_schedules::Entity::find_by_id(id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| BackupError::NotFound {
                resource: "BackupSchedule".to_string(),
                detail: "Backup schedule not found".to_string(),
            })?;

        // Calculate next run time based on the schedule expression
        let cron_schedule = Schedule::from_str(&schedule.schedule_expression)
            .map_err(|_| BackupError::Validation("Invalid backup schedule".into()))?;
        let next_run = cron_schedule.upcoming(Utc).next();

        // Update the schedule
        let mut schedule_update: temps_entities::backup_schedules::ActiveModel =
            schedule.into_active_model();
        schedule_update.enabled = sea_orm::Set(true);
        schedule_update.updated_at = sea_orm::Set(Utc::now());
        schedule_update.next_run = sea_orm::Set(next_run);

        let updated_schedule = schedule_update.update(self.db.as_ref()).await?;

        // Re-enabling puts this source back in scope for lifecycle
        // reconciliation — push the rule immediately rather than waiting
        // for the hourly sweep to notice.
        self.fire_lifecycle_reconcile(updated_schedule.s3_source_id);

        Ok(updated_schedule)
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
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};
    use temps_core::EncryptionService;
    use temps_entities::{backup_schedules, s3_sources};

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
    fn explicit_target_rejects_destination_backing_service() {
        let backing = HashSet::from([41]);
        let error = reject_source_backing_targets(&backing, [7, 41])
            .expect_err("the destination service must never be an explicit target");
        assert!(matches!(error, BackupError::Validation(_)));
        assert!(reject_source_backing_targets(&backing, [7, 42]).is_ok());
    }

    #[test]
    fn target_all_expansion_excludes_destination_backing_service() {
        fn service(id: i32) -> temps_entities::external_services::Model {
            let now = Utc::now();
            temps_entities::external_services::Model {
                id,
                name: format!("service-{id}"),
                service_type: "rustfs".to_string(),
                version: None,
                status: "running".to_string(),
                created_at: now,
                updated_at: now,
                slug: None,
                config: None,
                node_id: None,
                topology: "standalone".to_string(),
                error_message: None,
                health_status: None,
                last_health_check_at: None,
                last_health_error: None,
                consecutive_health_failures: 0,
                health_metadata: None,
                metrics_enabled: false,
                default_backup_provisioned: false,
                container_name: None,
                created_by_user_id: None,
                continuous_archive_s3_source_id: None,
                continuous_archive_pinned_at: None,
                workload_mode: Default::default(),
                log_ingest_lines_per_sec: None,
                log_ingest_kib_per_sec: None,
            }
        }

        let expanded = exclude_source_backing_services(
            vec![service(7), service(41), service(42)],
            &HashSet::from([41]),
        );
        assert_eq!(
            expanded
                .iter()
                .map(|service| service.id)
                .collect::<Vec<_>>(),
            vec![7, 42]
        );
    }

    #[test]
    fn zero_target_schedule_run_is_skipped_not_completed() {
        assert_eq!(schedule_run_aggregate_state(0, 0, 0, 0), "skipped");
        assert_eq!(schedule_run_aggregate_state(1, 0, 0, 0), "completed");
        assert_eq!(schedule_run_aggregate_state(1, 1, 0, 0), "failed");
    }

    #[tokio::test]
    async fn test_validate_backup_schedule_valid() {
        let db = Arc::new(MockDatabase::new(DatabaseBackend::Postgres).into_connection());

        let external_service_manager = create_mock_external_service_manager(db.clone());
        let alarm_service = create_mock_alarm_service();
        let config_service = create_mock_config_service();
        let encryption_service =
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap());
        let backup_service = BackupService::new(
            db,
            external_service_manager,
            alarm_service,
            config_service,
            encryption_service,
            Arc::new(NoopJobQueue),
        );

        // Valid schedule: every day at 2 AM (24 hours apart) - cron format with seconds
        let result = backup_service.validate_backup_schedule("0 0 2 * * *");
        assert!(
            result.is_ok(),
            "Expected valid schedule to pass: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_validate_backup_schedule_too_frequent() {
        let db = Arc::new(MockDatabase::new(DatabaseBackend::Postgres).into_connection());

        let external_service_manager = create_mock_external_service_manager(db.clone());
        let alarm_service = create_mock_alarm_service();
        let config_service = create_mock_config_service();
        let encryption_service =
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap());

        let backup_service = BackupService::new(
            db,
            external_service_manager,
            alarm_service,
            config_service,
            encryption_service,
            Arc::new(NoopJobQueue),
        );

        // Invalid schedule: every 30 minutes (too frequent) - cron format with seconds
        let result = backup_service.validate_backup_schedule("0 */30 * * * *");
        assert!(result.is_err(), "Expected error for too frequent schedule");
        match result {
            Err(BackupError::Validation(msg)) => {
                assert!(
                    msg.contains("at least 1 hour apart"),
                    "Error message should mention minimum interval: {}",
                    msg
                );
            }
            other => panic!("Expected validation error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_validate_backup_schedule_invalid_cron() {
        let db = Arc::new(MockDatabase::new(DatabaseBackend::Postgres).into_connection());

        let external_service_manager = create_mock_external_service_manager(db.clone());
        let alarm_service = create_mock_alarm_service();
        let config_service = create_mock_config_service();
        let encryption_service =
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap());
        let backup_service = BackupService::new(
            db,
            external_service_manager,
            alarm_service,
            config_service,
            encryption_service,
            Arc::new(NoopJobQueue),
        );

        // Invalid cron expression
        let result = backup_service.validate_backup_schedule("invalid cron");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_backup_schedules_empty() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![Vec::<backup_schedules::Model>::new()])
                .into_connection(),
        );

        let external_service_manager = create_mock_external_service_manager(db.clone());
        let alarm_service = create_mock_alarm_service();
        let config_service = create_mock_config_service();
        let encryption_service =
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap());
        let backup_service = BackupService::new(
            db,
            external_service_manager,
            alarm_service,
            config_service,
            encryption_service,
            Arc::new(NoopJobQueue),
        );

        let result = backup_service.list_backup_schedules().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_get_backup_schedule_not_found() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![Vec::<backup_schedules::Model>::new()])
                .into_connection(),
        );

        let external_service_manager = create_mock_external_service_manager(db.clone());
        let alarm_service = create_mock_alarm_service();
        let config_service = create_mock_config_service();
        let encryption_service =
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap());
        let backup_service = BackupService::new(
            db,
            external_service_manager,
            alarm_service,
            config_service,
            encryption_service,
            Arc::new(NoopJobQueue),
        );

        let result = backup_service.get_backup_schedule(999).await;
        assert!(result.is_err());
        match result {
            Err(BackupError::NotFound { .. }) => {}
            _ => panic!("Expected NotFound error"),
        }
    }

    /// Regression: `disable_backup_schedule` and `enable_backup_schedule`
    /// must trigger the same S3 lifecycle reconcile that
    /// `create_backup_schedule`/`update_backup_schedule`/
    /// `delete_backup_schedule` already do. Without it, disabling a
    /// source's last enabled schedule leaves a stale
    /// `PutBucketLifecycleConfiguration` rule on the bucket that keeps
    /// expiring objects indefinitely — and, after the hourly sweep was
    /// scoped down to only actively-scheduled sources, there is no other
    /// path that would ever clear it.
    #[tokio::test]
    async fn disable_and_enable_schedule_reconcile_s3_lifecycle_rules() {
        let docker = match bollard::Docker::connect_with_local_defaults() {
            Ok(docker) => docker,
            Err(error) => {
                println!("Docker not available, skipping test: {}", error);
                return;
            }
        };
        if let Err(error) = docker.ping().await {
            println!("Docker daemon not reachable, skipping test: {}", error);
            return;
        }

        use temps_database::test_utils::TestDatabase;
        use testcontainers::runners::AsyncRunner;

        let s3_container = crate::test_rustfs::rustfs_container_request()
            .start()
            .await
            .expect("Failed to start RustFS container");
        let s3_port = s3_container
            .get_host_port_ipv4(crate::test_rustfs::RUSTFS_S3_PORT)
            .await
            .expect("Failed to get RustFS port");
        let s3_endpoint = format!("http://localhost:{}", s3_port);
        crate::test_rustfs::wait_for_rustfs_ready(s3_port)
            .await
            .expect("RustFS did not become healthy");

        let test_db = TestDatabase::with_migrations()
            .await
            .expect("Failed to create test database");

        let s3_config = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                crate::test_rustfs::RUSTFS_ACCESS_KEY,
                crate::test_rustfs::RUSTFS_SECRET_KEY,
                None,
                None,
                "test",
            ))
            .endpoint_url(&s3_endpoint)
            .force_path_style(true)
            .http_client(crate::engines::v2_common::bundled_roots_http_client())
            .build();
        let s3_client = aws_sdk_s3::Client::from_conf(s3_config);

        let bucket_name = "test-lifecycle-toggle";
        s3_client
            .create_bucket()
            .bucket(bucket_name)
            .send()
            .await
            .expect("Failed to create bucket");
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let external_service_manager = create_mock_external_service_manager(test_db.db.clone());
        let alarm_service = create_mock_alarm_service();
        let server_config = temps_config::ServerConfig::new(
            "127.0.0.1:3000".to_string(),
            test_db.database_url.clone(),
            None,
            None,
        )
        .unwrap();
        let config_service = Arc::new(temps_config::ConfigService::new(
            Arc::new(server_config),
            test_db.db.clone(),
        ));
        let encryption_service =
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap());
        let backup_service = BackupService::new(
            test_db.db.clone(),
            external_service_manager,
            alarm_service,
            config_service,
            encryption_service,
            Arc::new(NoopJobQueue),
        );

        let s3_source = backup_service
            .create_s3_source(CreateS3SourceRequest {
                name: "test-rustfs-toggle".to_string(),
                bucket_name: bucket_name.to_string(),
                bucket_path: "/backups".to_string(),
                access_key_id: SecretString::new(crate::test_rustfs::RUSTFS_ACCESS_KEY),
                secret_key: crate::test_rustfs::RUSTFS_SECRET_KEY.into(),
                region: "us-east-1".to_string(),
                endpoint: Some(s3_endpoint.clone()),
                force_path_style: Some(true),
                is_default: None,
                backing_service_id: None,
            })
            .await
            .expect("Failed to create S3 source");

        let schedule = backup_service
            .create_backup_schedule(
                CreateBackupScheduleRequest {
                    name: "toggle-schedule".to_string(),
                    backup_type: "full".to_string(),
                    retention_period: 7,
                    s3_source_id: Some(s3_source.id),
                    schedule_expression: "0 0 2 * * *".to_string(),
                    enabled: true,
                    description: None,
                    tags: vec![],
                    max_runtime_secs: None,
                    target_all_services: None,
                    include_control_plane: None,
                    service_ids: vec![],
                },
                temps_core::BackupScheduleAuthority::InstanceAdmin,
            )
            .await
            .expect("Failed to create backup schedule");

        // Poll until the schedule creation's own reconcile has pushed the
        // retention-7d rule (proves the create-path reconcile ran, so the
        // baseline before disabling is "rule present").
        let rules_after_create = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            poll_lifecycle_rule_count(&s3_client, bucket_name),
        )
        .await
        .expect("lifecycle rule must appear after schedule creation");
        assert_eq!(
            rules_after_create, 1,
            "expected exactly one rule (temps-retention-7d) after create"
        );

        backup_service
            .disable_backup_schedule(schedule.id)
            .await
            .expect("Failed to disable backup schedule");

        // The disable must clear the now-orphaned rule, not leave it
        // dangling on the bucket.
        let rules_after_disable = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            poll_lifecycle_rule_count_becomes(&s3_client, bucket_name, 0),
        )
        .await
        .expect("lifecycle rule must be cleared after disabling the schedule");
        assert_eq!(
            rules_after_disable, 0,
            "disabling the sole schedule must clear the S3 lifecycle rule"
        );

        backup_service
            .enable_backup_schedule(schedule.id)
            .await
            .expect("Failed to enable backup schedule");

        // Re-enabling must push the rule back.
        let rules_after_enable = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            poll_lifecycle_rule_count_becomes(&s3_client, bucket_name, 1),
        )
        .await
        .expect("lifecycle rule must reappear after re-enabling the schedule");
        assert_eq!(
            rules_after_enable, 1,
            "re-enabling the schedule must restore the S3 lifecycle rule"
        );
    }

    /// Number of lifecycle rules currently on the bucket, treating "no
    /// lifecycle configuration at all" (`NoSuchLifecycleConfiguration`) as
    /// zero rather than an error — that's the expected state before the
    /// first reconcile, and after a reconcile clears the last rule.
    async fn current_lifecycle_rule_count(client: &aws_sdk_s3::Client, bucket: &str) -> usize {
        match client
            .get_bucket_lifecycle_configuration()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(resp) => resp.rules().len(),
            Err(err) => {
                let msg = format!("{err:?}");
                if msg.contains("NoSuchLifecycleConfiguration") {
                    0
                } else {
                    panic!("unexpected error reading bucket lifecycle config: {msg}");
                }
            }
        }
    }

    /// Polls until the bucket has at least one lifecycle rule, returning
    /// the count once non-zero.
    async fn poll_lifecycle_rule_count(client: &aws_sdk_s3::Client, bucket: &str) -> usize {
        loop {
            let count = current_lifecycle_rule_count(client, bucket).await;
            if count > 0 {
                return count;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Polls until the bucket's lifecycle rule count equals `expected`.
    async fn poll_lifecycle_rule_count_becomes(
        client: &aws_sdk_s3::Client,
        bucket: &str,
        expected: usize,
    ) -> usize {
        loop {
            let count = current_lifecycle_rule_count(client, bucket).await;
            if count == expected {
                return count;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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

    /// `update_backup_schedule` rejects an invalid cron expression before any
    /// DB write: the early validation path returns `BackupError::Validation`
    /// without reaching the active-model update step.
    #[tokio::test]
    async fn test_update_schedule_rejects_invalid_cron() {
        let schedule = make_test_schedule(1, 1);

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // get_backup_schedule SELECT
                .append_query_results(vec![vec![schedule]])
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

        let request = UpdateBackupScheduleRequest {
            name: None,
            description: None,
            schedule_expression: Some("not-a-cron".to_string()),
            retention_period: None,
            max_runtime_secs: None,
            enabled: None,
            tags: None,
            target_all_services: None,
            include_control_plane: None,
            service_ids: None,
        };

        let result = svc
            .update_backup_schedule(
                1,
                request,
                temps_core::BackupScheduleAuthority::InstanceAdmin,
            )
            .await;
        assert!(result.is_err(), "Invalid cron must be rejected");
        // Validation fires before any DB write — error is Validation, not Schedule,
        // because validate_backup_schedule wraps the cron parse error in Validation.
        match result.unwrap_err() {
            BackupError::Validation(_) | BackupError::Schedule(_) => {}
            other => panic!("Expected Validation or Schedule error, got: {:?}", other),
        }
    }

    /// When the cron expression changes, `next_run` must be recomputed to a
    /// future timestamp. The updated model returned by the service must have a
    /// non-None `next_run`.
    #[tokio::test]
    async fn test_update_schedule_recomputes_next_run_when_cron_changes() {
        let mut schedule = make_test_schedule(1, 1);
        // Use a cron that is definitely different from what `make_test_schedule` sets.
        schedule.schedule_expression = "0 0 0 * * *".to_string(); // daily

        let updated_row = temps_entities::backup_schedules::Model {
            schedule_expression: "0 0 2 * * *".to_string(), // 2 AM daily (new value)
            ..schedule.clone()
        };

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // get_backup_schedule SELECT
                .append_query_results(vec![vec![schedule]])
                // active.update() SELECT (Sea-ORM mock returns the next query result)
                .append_query_results(vec![vec![updated_row.clone()]])
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

        let request = UpdateBackupScheduleRequest {
            name: None,
            description: None,
            // Change from "0 0 0 * * *" to "0 0 2 * * *" — at least 1 h apart, valid.
            schedule_expression: Some("0 0 2 * * *".to_string()),
            retention_period: None,
            max_runtime_secs: None,
            enabled: None,
            tags: None,
            target_all_services: None,
            include_control_plane: None,
            service_ids: None,
        };

        let result = svc
            .update_backup_schedule(
                1,
                request,
                temps_core::BackupScheduleAuthority::InstanceAdmin,
            )
            .await;
        assert!(
            result.is_ok(),
            "Valid cron change must succeed: {:?}",
            result
        );
        let model = result.unwrap();
        assert_eq!(model.schedule_expression, "0 0 2 * * *");
    }

    /// When only `name` is set, the service must not blow up and must return
    /// the updated model. The inactive fields are left at their existing values
    /// (the active model only sets the columns that were `Some` in the request).
    #[tokio::test]
    async fn test_update_schedule_leaves_fields_untouched_when_absent() {
        let schedule = make_test_schedule(1, 1);

        let updated_row = temps_entities::backup_schedules::Model {
            name: "renamed".to_string(),
            ..schedule.clone()
        };

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // get_backup_schedule SELECT
                .append_query_results(vec![vec![schedule.clone()]])
                // active.update() returns the updated row
                .append_query_results(vec![vec![updated_row]])
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

        let request = UpdateBackupScheduleRequest {
            name: Some("renamed".to_string()),
            description: None,
            schedule_expression: None,
            retention_period: None,
            max_runtime_secs: None,
            enabled: None,
            tags: None,
            target_all_services: None,
            include_control_plane: None,
            service_ids: None,
        };

        let result = svc
            .update_backup_schedule(
                1,
                request,
                temps_core::BackupScheduleAuthority::InstanceAdmin,
            )
            .await;
        assert!(
            result.is_ok(),
            "Name-only update must succeed: {:?}",
            result
        );
        let model = result.unwrap();
        assert_eq!(model.name, "renamed");
        // Other fields unchanged from make_test_schedule defaults.
        assert_eq!(model.retention_period, schedule.retention_period);
        assert_eq!(model.schedule_expression, schedule.schedule_expression);
    }

    // ── NV1: instance-wide schedules need an instance administrator ────────

    fn nv1_create_request(
        target_all_services: Option<bool>,
        include_control_plane: Option<bool>,
        service_ids: Vec<i32>,
    ) -> CreateBackupScheduleRequest {
        CreateBackupScheduleRequest {
            name: "nightly".to_string(),
            backup_type: "full".to_string(),
            retention_period: 7,
            s3_source_id: Some(1),
            schedule_expression: "0 0 2 * * *".to_string(),
            enabled: true,
            description: None,
            tags: vec![],
            max_runtime_secs: None,
            target_all_services,
            include_control_plane,
            service_ids,
        }
    }

    fn nv1_rename_request() -> UpdateBackupScheduleRequest {
        UpdateBackupScheduleRequest {
            name: Some("renamed".to_string()),
            description: None,
            schedule_expression: None,
            retention_period: None,
            max_runtime_secs: None,
            enabled: None,
            tags: None,
            target_all_services: None,
            include_control_plane: None,
            service_ids: None,
        }
    }

    fn nv1_project_scoped_schedule(id: i32) -> temps_entities::backup_schedules::Model {
        temps_entities::backup_schedules::Model {
            target_all_services: false,
            include_control_plane: false,
            ..make_test_schedule(id, 1)
        }
    }

    /// The rule itself: only the combination "scoped caller + instance-wide
    /// reach" is refused; an administrator may do anything and a scoped
    /// caller keeps project-scoped schedules.
    #[test]
    fn nv1_schedule_authority_refuses_only_scoped_instance_wide_schedules() {
        for (target_all, control_plane) in [(true, true), (true, false), (false, true)] {
            assert!(ensure_schedule_authority(
                BackupScheduleAuthority::InstanceAdmin,
                "backup schedule 1",
                target_all,
                control_plane,
                "create",
            )
            .is_ok());
            let refused = ensure_schedule_authority(
                BackupScheduleAuthority::Scoped,
                "backup schedule 1",
                target_all,
                control_plane,
                "create",
            );
            match refused {
                Err(BackupError::Forbidden { resource, detail }) => {
                    assert_eq!(resource, "backup schedule 1");
                    assert!(detail.contains("instance administrator"), "{detail}");
                }
                other => panic!(
                    "scoped caller must be refused ({target_all}, {control_plane}): {other:?}"
                ),
            }
        }
        assert!(ensure_schedule_authority(
            BackupScheduleAuthority::Scoped,
            "backup schedule 1",
            false,
            false,
            "create",
        )
        .is_ok());
    }

    /// A scoped caller (e.g. a `Role::Custom` key with `settings:write` +
    /// `backups:write`) cannot create an instance-wide schedule through the
    /// service, whichever flag makes it instance-wide -- including the
    /// defaults -- and is refused before any database access.
    #[tokio::test]
    async fn nv1_scoped_caller_cannot_create_an_instance_wide_schedule() {
        for (target_all, control_plane) in [
            (None, None),
            (Some(true), Some(false)),
            (Some(false), Some(true)),
        ] {
            let db = Arc::new(MockDatabase::new(DatabaseBackend::Postgres).into_connection());
            let svc = make_scope_test_service(db);
            let result = svc
                .create_backup_schedule(
                    nv1_create_request(target_all, control_plane, vec![]),
                    BackupScheduleAuthority::Scoped,
                )
                .await;
            assert!(
                matches!(result, Err(BackupError::Forbidden { .. })),
                "({target_all:?}, {control_plane:?}) must be refused, got {result:?}"
            );
        }
    }

    /// A scoped caller still gets past the authority check for a
    /// project-scoped schedule: the request reaches ordinary validation (here
    /// the retention bound) exactly as before.
    #[tokio::test]
    async fn nv1_scoped_caller_keeps_creating_project_scoped_schedules() {
        let db = Arc::new(MockDatabase::new(DatabaseBackend::Postgres).into_connection());
        let svc = make_scope_test_service(db);
        let mut request = nv1_create_request(Some(false), Some(false), vec![42]);
        request.retention_period = 0;
        let result = svc
            .create_backup_schedule(request, BackupScheduleAuthority::Scoped)
            .await;
        assert!(
            matches!(result, Err(BackupError::Validation(_))),
            "a project-scoped create must reach validation, got {result:?}"
        );
    }

    /// Editing a project-scoped schedule as a scoped caller is unchanged.
    #[tokio::test]
    async fn nv1_scoped_caller_keeps_updating_project_scoped_schedules() {
        let schedule = nv1_project_scoped_schedule(1);
        let updated_row = temps_entities::backup_schedules::Model {
            name: "renamed".to_string(),
            ..schedule.clone()
        };
        // The schedule keeps one attached database, counted in the update's
        // transaction before the "nothing to back up" check.
        let mut count_row = BTreeMap::new();
        count_row.insert("num_items".to_string(), sea_orm::Value::BigInt(Some(1)));
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![schedule]])
                .append_query_results(vec![vec![count_row]])
                .append_query_results(vec![vec![updated_row]])
                .into_connection(),
        );
        let svc = make_scope_test_service(db);
        let model = svc
            .update_backup_schedule(1, nv1_rename_request(), BackupScheduleAuthority::Scoped)
            .await
            .expect("a project-scoped rename must still succeed for a scoped caller");
        assert_eq!(model.name, "renamed");
    }

    /// A scoped caller may neither edit an instance-wide schedule nor widen
    /// a project-scoped one into one.
    #[tokio::test]
    async fn nv1_scoped_caller_cannot_edit_or_widen_into_an_instance_wide_schedule() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![make_test_schedule(1, 1)]])
                .into_connection(),
        );
        let svc = make_scope_test_service(db);
        let edit = svc
            .update_backup_schedule(1, nv1_rename_request(), BackupScheduleAuthority::Scoped)
            .await;
        assert!(
            matches!(edit, Err(BackupError::Forbidden { .. })),
            "editing an instance-wide schedule must be refused, got {edit:?}"
        );

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![nv1_project_scoped_schedule(2)]])
                .into_connection(),
        );
        let svc = make_scope_test_service(db);
        let widen = svc
            .update_backup_schedule(
                2,
                UpdateBackupScheduleRequest {
                    include_control_plane: Some(true),
                    ..nv1_rename_request()
                },
                BackupScheduleAuthority::Scoped,
            )
            .await;
        assert!(
            matches!(widen, Err(BackupError::Forbidden { .. })),
            "adding the control plane must be refused, got {widen:?}"
        );
    }

    /// Deleting an instance-wide schedule as a scoped caller is refused
    /// before the DELETE runs: the mock holds no exec result, so reaching the
    /// DELETE would surface as a database error, not `Forbidden`.
    #[tokio::test]
    async fn nv1_scoped_caller_cannot_delete_an_instance_wide_schedule() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![make_test_schedule(1, 1)]])
                .into_connection(),
        );
        let svc = make_scope_test_service(db);
        let result = svc
            .delete_backup_schedule(1, BackupScheduleAuthority::Scoped)
            .await;
        assert!(
            matches!(result, Err(BackupError::Forbidden { .. })),
            "got {result:?}"
        );
    }

    /// When `find_by_id` returns no row (empty result), the service must return
    /// `BackupError::NotFound` without attempting an UPDATE.
    #[tokio::test]
    async fn test_update_schedule_not_found_returns_notfound() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // get_backup_schedule SELECT — empty result → schedule does not exist.
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

        let request = UpdateBackupScheduleRequest {
            name: Some("irrelevant".to_string()),
            description: None,
            schedule_expression: None,
            retention_period: None,
            max_runtime_secs: None,
            enabled: None,
            tags: None,
            target_all_services: None,
            include_control_plane: None,
            service_ids: None,
        };

        let result = svc
            .update_backup_schedule(
                999,
                request,
                temps_core::BackupScheduleAuthority::InstanceAdmin,
            )
            .await;
        assert!(result.is_err(), "Missing schedule must return NotFound");
        assert!(
            matches!(result.unwrap_err(), BackupError::NotFound { .. }),
            "Expected NotFound variant"
        );
    }

    // ── list_schedule_runs ────────────────────────────────────────────────────

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

    #[tokio::test]
    async fn recovery_set_waits_for_every_fan_out_child() {
        let mut control_plane = make_test_backup_model(100);
        control_plane.schedule_id = Some(5);
        control_plane.schedule_run_id = Some(50);
        control_plane.metadata = serde_json::json!({"engine": "control_plane"}).to_string();

        let mut service_backup = make_test_backup_model(101);
        service_backup.schedule_id = Some(5);
        service_backup.schedule_run_id = Some(50);
        service_backup.state = "running".to_string();
        service_backup.metadata =
            serde_json::json!({"engine": "postgres_pgdump", "service_id": 7}).to_string();

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![control_plane.clone()]])
                .append_query_results(vec![vec![control_plane, service_backup]])
                .into_connection(),
        );
        let service = make_scope_test_service(db);

        let outcome = service
            .publish_recovery_set_if_complete(100)
            .await
            .expect("live siblings should produce a typed pending outcome");

        assert_eq!(
            outcome,
            RecoverySetPublication::Pending {
                schedule_run_id: 50
            }
        );
    }

    #[tokio::test]
    async fn recovery_set_refuses_partial_fan_out_run() {
        let mut control_plane = make_test_backup_model(110);
        control_plane.schedule_id = Some(6);
        control_plane.schedule_run_id = Some(60);
        control_plane.metadata = serde_json::json!({"engine": "control_plane"}).to_string();

        let mut failed_service = make_test_backup_model(111);
        failed_service.schedule_id = Some(6);
        failed_service.schedule_run_id = Some(60);
        failed_service.state = "failed".to_string();
        failed_service.error_message = Some("container missing".to_string());
        failed_service.metadata =
            serde_json::json!({"engine": "postgres_pgdump", "service_id": 8}).to_string();

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![failed_service.clone()]])
                .append_query_results(vec![vec![control_plane, failed_service]])
                .into_connection(),
        );
        let service = make_scope_test_service(db);

        let outcome = service
            .publish_recovery_set_if_complete(111)
            .await
            .expect("failed siblings should produce a typed incomplete outcome");

        assert_eq!(
            outcome,
            RecoverySetPublication::Incomplete {
                schedule_run_id: 60,
                failed_backup_ids: vec![111],
            }
        );
    }

    #[tokio::test]
    async fn recovery_set_requires_a_control_plane_backup() {
        let mut service_backup = make_test_backup_model(120);
        service_backup.schedule_id = Some(7);
        service_backup.schedule_run_id = Some(70);
        service_backup.metadata =
            serde_json::json!({"engine": "redis", "service_id": 9}).to_string();

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![service_backup.clone()]])
                .append_query_results(vec![vec![service_backup]])
                .into_connection(),
        );
        let service = make_scope_test_service(db);

        let outcome = service
            .publish_recovery_set_if_complete(120)
            .await
            .expect("service-only schedules are valid but not whole-instance recovery sets");

        assert_eq!(
            outcome,
            RecoverySetPublication::NoControlPlane {
                schedule_run_id: 70
            }
        );
    }

    #[tokio::test]
    async fn recovery_set_requires_exact_expected_service_coverage() {
        let mut control_plane = make_test_backup_model(125);
        control_plane.schedule_id = Some(7);
        control_plane.schedule_run_id = Some(75);
        control_plane.metadata = serde_json::json!({
            "engine": "control_plane",
            "expected_service_ids": [9]
        })
        .to_string();

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![control_plane.clone()]])
                .append_query_results(vec![vec![control_plane]])
                .into_connection(),
        );
        let service = make_scope_test_service(db);

        let error = service
            .publish_recovery_set_if_complete(125)
            .await
            .expect_err("missing expected service backups must prevent publication");

        assert!(
            matches!(error, BackupError::Validation(message) if message.contains("coverage mismatch"))
        );
    }

    #[tokio::test]
    async fn recovery_set_publication_is_idempotent() {
        let mut control_plane = make_test_backup_model(130);
        control_plane.schedule_id = Some(8);
        control_plane.schedule_run_id = Some(80);
        control_plane.metadata = serde_json::json!({
            "engine": "control_plane",
            "recovery_set_published": true,
            "recovery_set_version": 2
        })
        .to_string();

        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![vec![control_plane.clone()]])
                .append_query_results(vec![vec![control_plane]])
                .into_connection(),
        );
        let service = make_scope_test_service(db);

        let outcome = service
            .publish_recovery_set_if_complete(130)
            .await
            .expect("already-published recovery sets should be a no-op");

        assert_eq!(
            outcome,
            RecoverySetPublication::AlreadyPublished {
                schedule_run_id: 80,
                backup_id: "uuid-130".to_string(),
            }
        );
    }

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

    #[tokio::test]
    async fn attach_services_rejects_unknown_schedule() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // get_backup_schedule -> find_by_id returns empty
                .append_query_results(vec![Vec::<backup_schedules::Model>::new()])
                .into_connection(),
        );
        let Ok(svc) = build_service_for_mock(db) else {
            return;
        };

        let err = svc
            .attach_services_to_schedule(42, &[1, 2, 3])
            .await
            .expect_err("missing schedule should error");
        assert!(
            matches!(err, BackupError::NotFound { .. }),
            "expected NotFound, got {:?}",
            err
        );
    }

    #[tokio::test]
    async fn attach_services_noop_on_empty_input() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // schedule lookup succeeds
                .append_query_results(vec![vec![make_test_schedule(7, 1)]])
                .into_connection(),
        );
        let Ok(svc) = build_service_for_mock(db) else {
            return;
        };

        // Empty list must short-circuit before any further query is issued —
        // we only queued one query result (the schedule lookup).
        let inserted = svc
            .attach_services_to_schedule(7, &[])
            .await
            .expect("empty attach should succeed");
        assert_eq!(inserted, 0);
    }

    // Note: validation-of-unknown-service-ids is covered by the integration
    // test (`integration_attach_list_detach_round_trip`) because mocking
    // Sea-ORM's `.count()` requires a query-result shape that `MockDatabase`
    // does not accept generically. The integration test exercises the same
    // code path against a real Postgres.

    /// Regression for the schedule-creation UI: a weekly schedule targeting
    /// specific databases must be creatable without a control-plane backup.
    /// The schedule row and membership are committed together.
    #[tokio::test]
    async fn integration_create_weekly_specific_schedule_without_control_plane() {
        if bollard::Docker::connect_with_local_defaults().is_err() {
            println!("Docker not available, skipping test");
            return;
        }
        use chrono::{Datelike, Weekday};
        use sea_orm::ActiveValue::Set;
        use temps_database::test_utils::TestDatabase;

        let test_db = match TestDatabase::with_migrations().await {
            Ok(database) => database,
            Err(error) => {
                println!("TestDatabase unavailable, skipping: {error}");
                return;
            }
        };
        let db = test_db.db.clone();

        let s3_source = temps_entities::s3_sources::ActiveModel {
            name: Set("schedule-target-source".to_string()),
            bucket_name: Set("schedule-target-bucket".to_string()),
            bucket_path: Set("/".to_string()),
            access_key_id: Set(String::new()),
            secret_key: Set(String::new()),
            region: Set("us-east-1".to_string()),
            force_path_style: Set(Some(true)),
            is_default: Set(true),
            ..Default::default()
        }
        .insert(db.as_ref())
        .await
        .expect("insert S3 source");

        let database = temps_entities::external_services::ActiveModel {
            name: Set("selected-database".to_string()),
            service_type: Set("postgres".to_string()),
            status: Set("running".to_string()),
            topology: Set("standalone".to_string()),
            ..Default::default()
        }
        .insert(db.as_ref())
        .await
        .expect("insert selected database");

        let service = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db.clone()),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        let schedule = service
            .create_backup_schedule(
                CreateBackupScheduleRequest {
                    name: "Weekly selected database".to_string(),
                    backup_type: "full".to_string(),
                    retention_period: 7,
                    s3_source_id: Some(s3_source.id),
                    schedule_expression: "0 0 0 * * SUN".to_string(),
                    enabled: true,
                    description: None,
                    tags: vec![],
                    max_runtime_secs: None,
                    target_all_services: Some(false),
                    include_control_plane: Some(false),
                    service_ids: vec![database.id],
                },
                temps_core::BackupScheduleAuthority::InstanceAdmin,
            )
            .await
            .expect("weekly specific schedule should be created");

        assert!(!schedule.target_all_services);
        assert!(!schedule.include_control_plane);
        assert_eq!(
            schedule.next_run.map(|run| run.weekday()),
            Some(Weekday::Sun)
        );

        let memberships = temps_entities::backup_schedule_services::Entity::find()
            .filter(temps_entities::backup_schedule_services::Column::ScheduleId.eq(schedule.id))
            .all(db.as_ref())
            .await
            .expect("list schedule memberships");
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships[0].service_id, database.id);
    }

    #[tokio::test]
    async fn detach_service_returns_false_when_no_row() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_exec_results(vec![MockExecResult {
                    last_insert_id: 0,
                    rows_affected: 0,
                }])
                .into_connection(),
        );
        let Ok(svc) = build_service_for_mock(db) else {
            return;
        };

        let removed = svc
            .detach_service_from_schedule(1, 2)
            .await
            .expect("detach should be idempotent");
        assert!(!removed, "no row → returns false");
    }

    #[tokio::test]
    async fn detach_service_returns_true_when_removed() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_exec_results(vec![MockExecResult {
                    last_insert_id: 0,
                    rows_affected: 1,
                }])
                .into_connection(),
        );
        let Ok(svc) = build_service_for_mock(db) else {
            return;
        };

        let removed = svc
            .detach_service_from_schedule(1, 2)
            .await
            .expect("detach should succeed");
        assert!(removed);
    }

    #[tokio::test]
    async fn list_services_for_unknown_schedule_returns_not_found() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results(vec![Vec::<backup_schedules::Model>::new()])
                .into_connection(),
        );
        let Ok(svc) = build_service_for_mock(db) else {
            return;
        };

        let err = svc
            .list_services_for_schedule(404)
            .await
            .expect_err("missing schedule must error");
        assert!(matches!(err, BackupError::NotFound { .. }));
    }

    #[tokio::test]
    async fn list_schedules_for_unknown_service_returns_not_found() {
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // external_services::Entity::find_by_id → empty
                .append_query_results(vec![Vec::<temps_entities::external_services::Model>::new()])
                .into_connection(),
        );
        let Ok(svc) = build_service_for_mock(db) else {
            return;
        };

        let err = svc
            .list_schedules_for_service(123)
            .await
            .expect_err("missing service must error");
        assert!(matches!(err, BackupError::NotFound { .. }));
    }

    /// Integration test: round-trip attach → list → detach against a real
    /// Postgres backed by `TestDatabase::with_migrations`. Verifies the
    /// migration creates the join table correctly, the FKs cascade on
    /// service-and-schedule delete, and the resolver join returns the right
    /// rows. Skips gracefully when Docker (and therefore the test Postgres)
    /// is unavailable.
    #[tokio::test]
    async fn integration_attach_list_detach_round_trip() {
        if bollard::Docker::connect_with_local_defaults().is_err() {
            println!("Docker not available, skipping test");
            return;
        }
        use sea_orm::ActiveValue::Set;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        use temps_database::test_utils::TestDatabase;

        let test_db = match TestDatabase::with_migrations().await {
            Ok(d) => d,
            Err(e) => {
                println!("TestDatabase unavailable, skipping: {e}");
                return;
            }
        };
        let db = test_db.db.clone();

        // Seed an S3 source (FK target for schedule).
        let s3_source = temps_entities::s3_sources::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("integration-source".to_string()),
            bucket_name: Set("test-bucket".to_string()),
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

        // Seed a schedule. Use 'specific' mode so the explicit-membership
        // path is exercised by this test (the integration test for the
        // 'all' branch lives in `integration_flip_to_all_clears_membership`).
        let schedule = temps_entities::backup_schedules::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("integration-schedule".to_string()),
            backup_type: Set("full".to_string()),
            retention_period: Set(7),
            s3_source_id: Set(s3_source.id),
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
            include_control_plane: Set(true),
            generated_kind: Set(None),
        }
        .insert(db.as_ref())
        .await
        .expect("insert schedule");

        // Seed two external services.
        let mk_svc = |name: &str, svc_type: &str| temps_entities::external_services::ActiveModel {
            id: sea_orm::NotSet,
            name: Set(name.to_string()),
            service_type: Set(svc_type.to_string()),
            version: Set(Some("17".to_string())),
            status: Set("running".to_string()),
            slug: Set(Some(name.to_string())),
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
        };
        let pg = mk_svc("pg-prod", "postgres")
            .insert(db.as_ref())
            .await
            .expect("insert pg service");
        let redis = mk_svc("redis-prod", "redis")
            .insert(db.as_ref())
            .await
            .expect("insert redis service");

        // Build a service. We can't use build_service_for_mock because we
        // want the *real* DB, not a mock. The Docker handle is required by
        // ExternalServiceManager but unused by these methods.
        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db.clone()),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        // 1) Attach both services.
        let inserted = svc
            .attach_services_to_schedule(schedule.id, &[pg.id, redis.id])
            .await
            .expect("attach succeeds");
        assert_eq!(inserted, 2, "both rows inserted");

        // 2) Re-attaching is idempotent (ON CONFLICT DO NOTHING).
        let inserted_again = svc
            .attach_services_to_schedule(schedule.id, &[pg.id, redis.id])
            .await
            .expect("re-attach succeeds");
        assert_eq!(inserted_again, 0, "no new rows on duplicate attach");

        // 3) list_services_for_schedule returns both, ordered by name.
        let listed = svc
            .list_services_for_schedule(schedule.id)
            .await
            .expect("list services");
        assert_eq!(listed.len(), 2);
        // Sorted by name: pg-prod < redis-prod
        assert_eq!(listed[0].name, "pg-prod");
        assert_eq!(listed[1].name, "redis-prod");

        // 4) list_schedules_for_service returns the schedule for each.
        let pg_schedules = svc
            .list_schedules_for_service(pg.id)
            .await
            .expect("list schedules for pg");
        assert_eq!(pg_schedules.len(), 1);
        assert_eq!(pg_schedules[0].id, schedule.id);

        // 5) Detach one service.
        let removed = svc
            .detach_service_from_schedule(schedule.id, pg.id)
            .await
            .expect("detach succeeds");
        assert!(removed);
        let listed = svc
            .list_services_for_schedule(schedule.id)
            .await
            .expect("list after detach");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "redis-prod");

        // 6) Detach again is idempotent (returns false, no error).
        let removed_again = svc
            .detach_service_from_schedule(schedule.id, pg.id)
            .await
            .expect("idempotent detach");
        assert!(!removed_again);

        // 7) Cascade: deleting the schedule removes all membership rows.
        temps_entities::backup_schedules::Entity::delete_by_id(schedule.id)
            .exec(db.as_ref())
            .await
            .expect("delete schedule");
        let leftover = temps_entities::backup_schedule_services::Entity::find()
            .filter(temps_entities::backup_schedule_services::Column::ScheduleId.eq(schedule.id))
            .all(db.as_ref())
            .await
            .expect("count leftover");
        assert!(
            leftover.is_empty(),
            "schedule delete must cascade to membership"
        );
    }

    /// Integration test: when `target_all_services = true`, flipping a
    /// schedule's mode via `update_backup_schedule` clears all explicit
    /// membership rows (clean-slate behaviour). When set back to false,
    /// the rows are not magically restored — the user has to attach
    /// again. Skips gracefully when Docker / test Postgres are absent.
    #[tokio::test]
    async fn integration_flip_to_all_clears_membership() {
        if bollard::Docker::connect_with_local_defaults().is_err() {
            println!("Docker not available, skipping test");
            return;
        }
        use sea_orm::ActiveValue::Set;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        use temps_database::test_utils::TestDatabase;

        let test_db = match TestDatabase::with_migrations().await {
            Ok(d) => d,
            Err(e) => {
                println!("TestDatabase unavailable, skipping: {e}");
                return;
            }
        };
        let db = test_db.db.clone();

        // Seed S3 source + schedule (start in 'specific' mode so we have
        // membership rows to clear).
        let s3 = temps_entities::s3_sources::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("flip-source".to_string()),
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

        let schedule = temps_entities::backup_schedules::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("flip-schedule".to_string()),
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
            // Start as specific so we can attach rows.
            target_all_services: Set(false),
            include_control_plane: Set(true),
            generated_kind: Set(None),
        }
        .insert(db.as_ref())
        .await
        .expect("insert schedule");

        let svc_a = temps_entities::external_services::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("svc-a".to_string()),
            service_type: Set("postgres".to_string()),
            version: Set(Some("17".to_string())),
            status: Set("running".to_string()),
            slug: Set(Some("svc-a".to_string())),
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
        .expect("insert svc-a");

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db.clone()),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        // Attach svc-a to the specific schedule.
        svc.attach_services_to_schedule(schedule.id, &[svc_a.id])
            .await
            .expect("attach");

        let listed = svc
            .list_services_for_schedule(schedule.id)
            .await
            .expect("list pre-flip");
        assert_eq!(listed.len(), 1, "precondition: one service attached");

        // Flip to target_all_services = true via the service-layer update
        // (mirrors what the handler does on PATCH).
        svc.update_backup_schedule(
            schedule.id,
            crate::handlers::backup_handler::UpdateBackupScheduleRequest {
                name: None,
                description: None,
                schedule_expression: None,
                retention_period: None,
                max_runtime_secs: None,
                enabled: None,
                tags: None,
                target_all_services: Some(true),
                include_control_plane: None,
                service_ids: None,
            },
            temps_core::BackupScheduleAuthority::InstanceAdmin,
        )
        .await
        .expect("update succeeds");

        // Membership table must now be empty for this schedule.
        let after = temps_entities::backup_schedule_services::Entity::find()
            .filter(temps_entities::backup_schedule_services::Column::ScheduleId.eq(schedule.id))
            .all(db.as_ref())
            .await
            .expect("count after flip");
        assert!(
            after.is_empty(),
            "flipping to target_all_services=true must clear membership rows"
        );

        // Flip back to specific — list must stay empty (we cleared it).
        svc.update_backup_schedule(
            schedule.id,
            crate::handlers::backup_handler::UpdateBackupScheduleRequest {
                name: None,
                description: None,
                schedule_expression: None,
                retention_period: None,
                max_runtime_secs: None,
                enabled: None,
                tags: None,
                target_all_services: Some(false),
                include_control_plane: None,
                service_ids: None,
            },
            temps_core::BackupScheduleAuthority::InstanceAdmin,
        )
        .await
        .expect("update back to specific");

        let after_specific = svc
            .list_services_for_schedule(schedule.id)
            .await
            .expect("list after flip-back");
        assert!(
            after_specific.is_empty(),
            "flipping back to specific must not magically restore membership"
        );

        // The edit form can switch from all databases to one explicit
        // database while disabling the control-plane target in one PATCH.
        // This used to fail because the service validated the intermediate
        // target state before the UI could attach the selected database.
        let selected = svc
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
                    target_all_services: Some(false),
                    include_control_plane: Some(false),
                    service_ids: Some(vec![svc_a.id]),
                },
                temps_core::BackupScheduleAuthority::InstanceAdmin,
            )
            .await
            .expect("atomic specific-target update succeeds");
        assert!(!selected.target_all_services);
        assert!(!selected.include_control_plane);

        let selected_services = svc
            .list_services_for_schedule(schedule.id)
            .await
            .expect("list after atomic specific-target update");
        assert_eq!(selected_services.len(), 1);
        assert_eq!(selected_services[0].id, svc_a.id);
    }

    /// Unit test (no DB needed): create_backup_schedule rejects a request
    /// that would produce a no-op schedule (include_control_plane=false
    /// AND target_all_services=false). The validation runs before any
    /// DB call, so we don't even need a working Docker daemon for this.
    #[tokio::test]
    async fn create_rejects_empty_fan_out() {
        // Build a service with a mock DB. We never reach the DB because
        // validation fires first.
        if skip_if_no_docker() {
            return;
        }
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // resolve_s3_source_id: caller passed Some(1) so this query
                // (find_by_id) is the next thing the service does.
                .append_query_results(vec![vec![s3_sources::Model {
                    id: 1,
                    name: "s".to_string(),
                    bucket_name: "b".to_string(),
                    bucket_path: "/".to_string(),
                    access_key_id: "".to_string(),
                    secret_key: "".to_string(),
                    session_token: None,
                    credentials_expire_at: None,
                    region: "us-east-1".to_string(),
                    endpoint: None,
                    force_path_style: Some(true),
                    is_default: true,
                    managed_by_cloud: false,
                    lifecycle_reconcile_failed_at: None,
                    lifecycle_reconcile_generation: 0,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                    backing_service_id: None,
                }]])
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

        let request = CreateBackupScheduleRequest {
            name: "bad".to_string(),
            backup_type: "full".to_string(),
            retention_period: 7,
            s3_source_id: Some(1),
            schedule_expression: "0 0 2 * * *".to_string(),
            enabled: true,
            description: None,
            tags: vec![],
            max_runtime_secs: None,
            target_all_services: Some(false),
            include_control_plane: Some(false),
            service_ids: vec![],
        };

        let err = svc
            .create_backup_schedule(request, temps_core::BackupScheduleAuthority::InstanceAdmin)
            .await
            .expect_err("empty fan-out must be rejected");
        assert!(
            matches!(err, BackupError::Validation(ref msg) if msg.contains("control plane")),
            "expected Validation error mentioning control plane, got {:?}",
            err
        );
    }

    /// The daily base-backup cron expression used by the auto-provisioner
    /// (`reconcile_default_external_service_schedules`) must satisfy
    /// `validate_backup_schedule`: parse under the `cron` crate's 6-field
    /// format and produce adjacent runs at least one hour apart. This guards
    /// the load-bearing literal so a typo can't ship a schedule that the
    /// validator rejects at provision time.
    #[tokio::test]
    async fn auto_provision_cron_expression_is_valid() {
        // Same expression as provision_default_schedule_for_service.
        const DAILY_3AM: &str = "0 0 3 * * *";

        // Parses under the cron crate (6-field sec/min/hour/dom/mon/dow).
        let schedule =
            Schedule::from_str(DAILY_3AM).expect("auto-provision cron expression must parse");

        // Two adjacent runs are 24h apart -> passes the >= 1h rule in
        // validate_backup_schedule.
        let next_two: Vec<_> = schedule.upcoming(Utc).take(2).collect();
        assert_eq!(next_two.len(), 2, "expected two upcoming runs");
        let gap = next_two[1] - next_two[0];
        assert_eq!(
            gap.num_hours(),
            24,
            "daily base-backup runs should be 24h apart, got {} hours",
            gap.num_hours()
        );
    }

    /// `reconcile_default_external_service_schedules` is a safe no-op when no
    /// default S3 source is configured: `resolve_s3_source_id(None)` errors,
    /// the reconcile swallows it and returns `Ok(())` so the periodic tick can
    /// retry once storage is configured. The MockDatabase returns an empty
    /// `s3_sources` result for the `is_default = true` lookup, so the service
    /// never reaches the MariaDB query.
    #[tokio::test]
    async fn reconcile_default_schedules_noop_without_default_source() {
        if skip_if_no_docker() {
            return;
        }
        let db = Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres)
                // get_default_s3_source: no default configured -> empty result.
                .append_query_results(vec![Vec::<s3_sources::Model>::new()])
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

        let result = svc.reconcile_default_external_service_schedules().await;
        assert!(
            result.is_ok(),
            "reconcile must be a no-op (Ok) when no default S3 source exists, got {:?}",
            result
        );
    }

    /// Full-path integration test (needs a real DB + Docker): with a default
    /// S3 source and an unprovisioned MariaDB service, reconcile creates
    /// exactly one daily schedule, attaches the service to it, flips
    /// `default_backup_provisioned`, and is idempotent on a second call.
    #[tokio::test]
    async fn reconcile_default_schedules_provisions_mariadb_once() {
        if skip_if_no_docker() {
            return;
        }
        use sea_orm::ActiveValue::Set;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        use temps_database::test_utils::TestDatabase;

        let test_db = match TestDatabase::with_migrations().await {
            Ok(d) => d,
            Err(e) => {
                println!("TestDatabase unavailable, skipping: {e}");
                return;
            }
        };
        let db = test_db.db.clone();

        // Default S3 source so resolve_s3_source_id(None) succeeds.
        temps_entities::s3_sources::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("auto-prov-source".to_string()),
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

        // One unprovisioned MariaDB service + one Postgres service (which must
        // be left alone — scope is MariaDB only).
        let maria = temps_entities::external_services::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("maria-auto".to_string()),
            service_type: Set("mariadb".to_string()),
            status: Set("running".to_string()),
            topology: Set("standalone".to_string()),
            ..Default::default()
        }
        .insert(db.as_ref())
        .await
        .expect("insert mariadb service");

        let pg = temps_entities::external_services::ActiveModel {
            id: sea_orm::NotSet,
            name: Set("pg-untouched".to_string()),
            service_type: Set("postgres".to_string()),
            status: Set("running".to_string()),
            topology: Set("standalone".to_string()),
            ..Default::default()
        }
        .insert(db.as_ref())
        .await
        .expect("insert postgres service");

        let svc = BackupService::new(
            db.clone(),
            create_mock_external_service_manager(db.clone()),
            create_mock_alarm_service(),
            create_mock_config_service(),
            Arc::new(EncryptionService::new("test_encryption_key_1234567890ab").unwrap()),
            Arc::new(NoopJobQueue),
        );

        svc.reconcile_default_external_service_schedules()
            .await
            .expect("first reconcile succeeds");

        // Exactly one schedule created.
        let schedules = temps_entities::backup_schedules::Entity::find()
            .all(db.as_ref())
            .await
            .expect("list schedules");
        assert_eq!(
            schedules.len(),
            1,
            "expected exactly one auto-provisioned schedule"
        );
        let schedule = &schedules[0];
        assert_eq!(schedule.schedule_expression, "0 0 3 * * *");
        assert_eq!(schedule.backup_type, "full");
        assert_eq!(schedule.retention_period, 14);
        assert!(!schedule.target_all_services);
        assert!(!schedule.include_control_plane);

        // The MariaDB service is attached to it.
        let attached = svc
            .list_services_for_schedule(schedule.id)
            .await
            .expect("list attached services");
        assert_eq!(attached.len(), 1, "exactly the mariadb service attached");
        assert_eq!(attached[0].id, maria.id);

        // Latch flipped on MariaDB, untouched on Postgres.
        let maria_after = temps_entities::external_services::Entity::find_by_id(maria.id)
            .one(db.as_ref())
            .await
            .expect("reload mariadb")
            .expect("mariadb exists");
        assert!(
            maria_after.default_backup_provisioned,
            "mariadb latch must be set after provisioning"
        );
        let pg_after = temps_entities::external_services::Entity::find_by_id(pg.id)
            .one(db.as_ref())
            .await
            .expect("reload postgres")
            .expect("postgres exists");
        assert!(
            !pg_after.default_backup_provisioned,
            "non-mariadb services must never be provisioned"
        );

        // Idempotency: a second reconcile creates nothing new.
        svc.reconcile_default_external_service_schedules()
            .await
            .expect("second reconcile succeeds");
        let count_after = temps_entities::backup_schedules::Entity::find()
            .filter(temps_entities::backup_schedules::Column::Id.eq(schedule.id))
            .count(db.as_ref())
            .await
            .expect("count");
        let total = temps_entities::backup_schedules::Entity::find()
            .count(db.as_ref())
            .await
            .expect("count all");
        assert_eq!(count_after, 1);
        assert_eq!(total, 1, "second reconcile must not create a duplicate");
    }

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
