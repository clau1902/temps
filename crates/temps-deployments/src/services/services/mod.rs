// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`DeploymentService`]: deployments, their containers and their lifecycle.
//!
//! The service is defined here; its behaviour is split by concern:
//!
//! - `error`: [`DeploymentError`]
//! - `domains`: deployment and environment URLs, environment links, custom
//!   domains
//! - `queries`: deployment reads, captured container logs, jobs, activity and
//!   the row-to-DTO mapping
//! - `settings`: environment deployment settings and the static asset cache
//!   purge
//! - `containers`: container runtime per node, logs, listings, detail,
//!   start/stop/restart, env vars and metrics
//! - `cleanup`: owner cleanup of a deleted project's or environment's
//!   containers, and its ownership checks
//! - `guards`: the Docker-socket grant and external-hosting checks every deploy
//!   path runs
//! - `trigger`: manual pipeline and image deployment triggers
//! - `redeploy`: redeploys from the recorded source, node drain and failover
//!   planning

use futures::Stream;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseBackend,
    DatabaseTransaction, DbErr, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
    Set, Statement, TransactionTrait,
};
use std::collections::HashMap;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use temps_entities::{
    deployment_container_logs, deployment_containers, deployment_domains, deployments,
    environments, nodes, projects,
};
use thiserror::Error;
use tracing::{debug, error, info, warn};

use super::container_operations::{
    ContainerOperations, LocalContainerOperations, RemoteContainerOperations,
};
use crate::services::node_service::{CONTAINER_STATUS_ORPHANED, CONTAINER_STATUS_REMOVED};
use crate::services::types::{
    Deployment, DeploymentDomain, DeploymentEnvironment, DeploymentListResponse,
    LatestDeploymentMedia,
};
use crate::UpdateDeploymentSettingsRequest;
use temps_core::PublicHostnameStrategy;
use temps_core::WorkflowTask;

// Sibling service modules, imported so the submodules below keep reaching
// them as `super::<module>`.
use super::{env_resolver, job_processor};

mod cleanup;
mod containers;
mod domains;
mod error;
mod guards;
mod queries;
mod redeploy;
mod settings;
mod trigger;

pub use containers::{
    ContainerLogParams, ContainerLogStream, ContainerPresentationContext, EnvironmentContainer,
    ResolvedContainerResourceLimits,
};
pub use domains::ComposePublicUrlContext;
pub use error::DeploymentError;
pub use queries::RepoReference;
pub use redeploy::{RedeploySource, MAX_REDEPLOY_PLAN_BATCH};

fn resolve_reused_health(
    build_outputs: Option<&serde_json::Value>,
    environment: Option<&temps_entities::deployment_config::DeploymentConfig>,
    project: Option<&temps_entities::deployment_config::DeploymentConfig>,
) -> Result<Option<temps_core::repo_config::HealthCheckConfig>, DeploymentError> {
    let repo_health = build_outputs.and_then(|outputs| outputs.get("health"));
    let health = if let Some(value) = repo_health {
        Some(
            serde_json::from_value::<temps_core::repo_config::HealthCheckConfig>(value.clone())
                .map_err(|error| {
                    DeploymentError::InvalidInput(format!(
                        "Cannot reuse image health configuration: {error}"
                    ))
                })?,
        )
    } else {
        environment
            .and_then(|config| config.health.clone())
            .or_else(|| project.and_then(|config| config.health.clone()))
    };
    if let Some(health) = &health {
        health.validate().map_err(|reason| {
            DeploymentError::InvalidInput(format!(
                "Cannot reuse image health configuration: {reason}"
            ))
        })?;
    }
    Ok(health)
}

/// Lock the environment row that orders deployment generations. All code paths
/// that insert a deployment must take this lock before assigning `created_at`
/// and inserting, so failover recovery can reliably detect newer user work.
pub(crate) async fn lock_environment_for_deployment_generation(
    transaction: &DatabaseTransaction,
    project_id: i32,
    environment_id: i32,
) -> Result<environments::Model, DbErr> {
    environments::Entity::find_by_id(environment_id)
        .filter(environments::Column::ProjectId.eq(project_id))
        .filter(environments::Column::DeletedAt.is_null())
        .lock(sea_orm::sea_query::LockType::Update)
        .one(transaction)
        .await?
        .ok_or_else(|| {
            DbErr::RecordNotFound(format!(
                "environment {environment_id} for project {project_id} was not found or is deleted"
            ))
        })
}

/// Insert a deployment under the shared environment generation lock.
pub(crate) async fn insert_deployment_with_generation_lock(
    db: &temps_database::DbConnection,
    project_id: i32,
    environment_id: i32,
    mut deployment: deployments::ActiveModel,
) -> Result<deployments::Model, DbErr> {
    let transaction = db.begin().await?;
    lock_environment_for_deployment_generation(&transaction, project_id, environment_id).await?;
    let generation_time = chrono::Utc::now();
    deployment.created_at = Set(generation_time);
    deployment.updated_at = Set(generation_time);
    let deployment = deployment.insert(&transaction).await?;
    transaction.commit().await?;
    Ok(deployment)
}

struct PipelineTriggerOptions {
    branch: Option<String>,
    tag: Option<String>,
    commit: Option<String>,
    rollback_from_deployment_id: Option<i32>,
    recovery_of_deployment_id: Option<i32>,
}

/// Run a failed mark-complete job's cleanup and describe both outcomes.
///
/// Promotion and rollback drive their jobs by hand instead of through the
/// workflow executor, so they must also do what the executor does when a
/// job fails: call its `cleanup`. For `MarkDeploymentCompleteJob` that tears
/// down the containers the deploy step started, unless the deployment
/// already reached completed/deployed.
async fn clean_up_failed_completion(
    job: &dyn WorkflowTask,
    context: &temps_core::WorkflowContext,
    error: &temps_core::WorkflowError,
    operation: &str,
) -> String {
    match job.cleanup(context).await {
        Ok(()) => format!("Mark complete failed: {error}"),
        Err(cleanup_error) => {
            error!(
                "{operation}: cleanup after failed mark-complete for deployment {} also failed: {}",
                context.deployment_id, cleanup_error
            );
            format!(
                "Mark complete failed: {error}; {operation} container cleanup also failed: {cleanup_error}"
            )
        }
    }
}

/// Deployment states a pause applies to: the deployment is live and serving.
const PAUSABLE_DEPLOYMENT_STATES: &[&str] = &["deployed", "completed"];
/// Deployment states a resume applies to: only one paused by `pause_deployment`.
const RESUMABLE_DEPLOYMENT_STATES: &[&str] = &["paused"];

/// Refuse a pause/resume whose current state is not one it applies to.
/// Without this, resume restarted the containers of a stopped, failed or
/// cancelled deployment and marked it "deployed" again.
fn check_lifecycle_transition(
    deployment: &deployments::Model,
    operation: &'static str,
    allowed: &[&str],
    expected: &'static str,
) -> Result<(), DeploymentError> {
    if allowed.contains(&deployment.state.as_str()) {
        return Ok(());
    }
    Err(DeploymentError::DeploymentStateConflict {
        deployment_id: deployment.id,
        operation,
        state: deployment.state.clone(),
        expected,
    })
}

/// True only for a Postgres unique-constraint violation (SQLSTATE 23505).
///
/// Used to detect the race where two requests carrying the same
/// `upload_request_id` both pass the existence pre-check before either
/// inserts: the loser's plain (non-`on_conflict`) insert hits
/// `idx_deployments_upload_request_id` instead of succeeding, and that
/// specific failure -- not a generic database error -- means the winner's
/// row should be looked up and returned instead of surfacing a 500.
pub(crate) fn is_unique_violation(err: &sea_orm::DbErr) -> bool {
    err.sql_err()
        .is_some_and(|e| matches!(e, sea_orm::SqlErr::UniqueConstraintViolation(_)))
}

fn confined_archive_path(
    data_dir: &std::path::Path,
    stored_path: &str,
) -> Result<std::path::PathBuf, DeploymentError> {
    let relative = std::path::Path::new(stored_path);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(DeploymentError::InvalidBundlePath {
            path: stored_path.to_string(),
            reason: "stored archive path escapes the Temps data directory".to_string(),
        });
    }
    Ok(data_dir.join(relative))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeploymentAssetOrigin {
    deployment_id: i32,
    environment_id: i32,
    slug: String,
}

/// Resolve the immutable build artifact behind a deployment. Promotion and
/// rollback rows may already reference an earlier source, so propagating the
/// immediate row would lose assets after the second hop.
fn complete_deployment_asset_origin(
    deployment_id: i32,
    environment_id: i32,
    slug: &str,
    context: Option<&serde_json::Value>,
) -> Option<DeploymentAssetOrigin> {
    let source_deployment_id = context
        .and_then(|value| value.get("source_deployment_id"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());
    let source_environment_id = context
        .and_then(|value| value.get("source_environment_id"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());
    let source_slug = context
        .and_then(|value| value.get("source_deployment_slug"))
        .and_then(serde_json::Value::as_str);

    match (source_deployment_id, source_environment_id, source_slug) {
        (Some(deployment_id), Some(environment_id), Some(slug)) => Some(DeploymentAssetOrigin {
            deployment_id,
            environment_id,
            slug: slug.to_string(),
        }),
        (None, None, None) => Some(DeploymentAssetOrigin {
            deployment_id,
            environment_id,
            slug: slug.to_string(),
        }),
        _ => None,
    }
}

fn source_deployment_id(context: Option<&serde_json::Value>) -> Option<i32> {
    context
        .and_then(|value| value.get("source_deployment_id"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
}

/// Resolve both the current complete reuse metadata and the partial metadata
/// written by Temps before source slugs were persisted. Legacy rows are walked
/// back to the immutable build deployment so a second promotion/rollback does
/// not perpetuate an intermediate, asset-less deployment.
async fn deployment_asset_origin(
    db: &temps_database::DbConnection,
    deployment: &deployments::Model,
) -> Result<DeploymentAssetOrigin, DeploymentError> {
    let root_deployment_id = deployment.id;
    let project_id = deployment.project_id;
    let mut current = deployment.clone();
    let mut visited = HashSet::from([current.id]);

    loop {
        if let Some(origin) = complete_deployment_asset_origin(
            current.id,
            current.environment_id,
            &current.slug,
            current.context_vars.as_ref(),
        ) {
            return Ok(origin);
        }

        let Some(source_id) = source_deployment_id(current.context_vars.as_ref()) else {
            return Ok(DeploymentAssetOrigin {
                deployment_id: current.id,
                environment_id: current.environment_id,
                slug: current.slug,
            });
        };

        if !visited.insert(source_id) {
            return Err(DeploymentError::AssetOriginCycle {
                project_id,
                deployment_id: root_deployment_id,
                source_deployment_id: source_id,
            });
        }

        current = deployments::Entity::find_by_id(source_id)
            .filter(deployments::Column::ProjectId.eq(project_id))
            .one(db)
            .await?
            .ok_or(DeploymentError::AssetOriginNotFound {
                project_id,
                deployment_id: root_deployment_id,
                source_deployment_id: source_id,
            })?;
    }
}

/// What a rollback or promotion knows about the image it reuses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReuseImageProvenance {
    source: crate::jobs::DeployImageSource,
    expected_identity: Option<crate::jobs::ExpectedImageIdentity>,
    /// The workers holding the reused image, when the origin built it on
    /// workers and the control plane cannot serve it itself.
    worker_build: Option<crate::jobs::ReusedWorkerBuild>,
}

impl ReuseImageProvenance {
    fn apply(&self, job: crate::jobs::DeployImageJob) -> crate::jobs::DeployImageJob {
        let job = job.with_image_source(self.source);
        let job = match &self.worker_build {
            Some(worker_build) => job.with_reused_worker_build(worker_build.clone()),
            None => job,
        };
        match &self.expected_identity {
            Some(expected) => job.with_expected_image_identity(expected.clone()),
            None => job,
        }
    }

    fn expected_identity_label(&self) -> String {
        self.expected_identity
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "none recorded".to_string())
    }
}

#[derive(Clone)]
pub struct DeploymentService {
    db: Arc<temps_database::DbConnection>,
    log_service: Arc<temps_deployment_logs::LogService>,
    config_service: Arc<temps_config::ConfigService>,
    queue_service: Arc<dyn temps_core::JobQueue>,
    docker_log_service: Arc<temps_deployment_logs::DockerLogService>,
    docker_handle: Arc<temps_core::DockerHandle>,
    deployer: Arc<dyn temps_deployer::ContainerDeployer>,
    encryption_service: Arc<temps_core::EncryptionService>,
    /// Anonymous product telemetry reporter (late-bound, optional). Set via
    /// [`Self::set_telemetry`]; defaults to a no-op when unset.
    telemetry: std::sync::OnceLock<Arc<dyn temps_core::telemetry::TelemetryReporter>>,
    /// ADR 068: an environment's primary address and the deploy warning
    /// (late-bound, optional: empty without the ingress plugin).
    environment_addresses:
        std::sync::OnceLock<Arc<temps_core::environment_address::EnvironmentAddressSlot>>,
    /// Resolves a container's environment variables from the selected
    /// environment (late-bound; the resolver's six service deps only exist later
    /// in plugin init). Set via [`Self::set_env_resolver`]. Used by the inline
    /// promote/rollback deploy paths so a promoted/rolled-back container gets the
    /// SAME resolved env (user vars, external-service vars, Sentry/OTel, API
    /// token) as a normal deploy — see [`crate::services::env_resolver`].
    env_resolver: std::sync::OnceLock<Arc<crate::services::env_resolver::DeploymentEnvResolver>>,
    /// Late-bound Compose executor (the `Arc<bollard::Docker>` client it needs
    /// is only constructed later in plugin init, after `DeploymentService`
    /// itself). Set via [`Self::set_compose_executor`]. Used by
    /// `cleanup_containers` preserves ambiguous Compose resources until an
    /// operator verifies their ownership before reclaiming them.
    compose_executor: std::sync::OnceLock<Arc<temps_deployer::compose::ComposeExecutor>>,
    /// Audit sink for deploy-path security events (late-bound, optional).
    ///
    /// The rollback and promotion paths build their own `DeployImageJob`
    /// rather than going through `WorkflowExecutionService`, so without this
    /// they would always take the "no audit sink" branch and the ADR-045
    /// `DEPLOYMENT_DOCKER_SOCKET_MOUNTED` record would silently never exist
    /// for a rolled-back or promoted granted project. Late-bound like
    /// `telemetry`: a missing sink degrades to a log line, never a failed
    /// deployment.
    audit_logger: std::sync::OnceLock<Arc<dyn temps_core::AuditLogger>>,
    /// Scheduler shared with the normal workflow path. Promotion and rollback
    /// build deploy jobs inline, so they must receive it explicitly or every
    /// reused image is implicitly assigned to the control plane.
    node_scheduler: std::sync::OnceLock<Arc<crate::services::NodeScheduler>>,
    /// Image source used when a selected worker needs an image transferred.
    image_builder: Arc<dyn temps_deployer::ImageBuilder>,
    /// Planner shared with normal deployments for worker-specific environment
    /// rewrites and cross-node managed-service blockers.
    workflow_planner: std::sync::OnceLock<Arc<crate::services::WorkflowPlanner>>,
    /// The workload plane, bound once it starts (ADR-055). Rollback and
    /// promotion retire the superseded deployment's `enforce` replicas
    /// through it; unset, those replicas are left for a later sweep rather
    /// than removed behind the plane's back.
    workload_plane: std::sync::OnceLock<Arc<temps_workload_plane::WorkloadPlane>>,
}

impl DeploymentService {
    /// Resolve CPU/memory limits + requests for a deploy from the environment
    /// config first, then the project config, leaving each field unset when
    /// neither configures it (→ no Docker limit = uncapped). Mirrors the
    /// resolution in `WorkflowExecutionService` so every deploy path (initial,
    /// rollback, promote) treats resource limits as opt-in identically.
    ///
    /// CPU is stored as microcores in the DB (1_000_000 = 1 core), memory as MB;
    /// emitted with the `u`/`Mi` suffixes the deployer's parsers understand.
    fn resolve_resource_usage(
        env_cfg: Option<&temps_entities::deployment_config::DeploymentConfig>,
        proj_cfg: Option<&temps_entities::deployment_config::DeploymentConfig>,
    ) -> crate::jobs::ResourceUsage {
        let resolve = |getter: fn(
            &temps_entities::deployment_config::DeploymentConfig,
        ) -> Option<i32>|
         -> Option<i32> {
            env_cfg
                .and_then(getter)
                .or_else(|| proj_cfg.and_then(getter))
        };
        crate::jobs::ResourceUsage {
            cpu_limit: resolve(|c| c.cpu_limit).map(|u| format!("{}u", u)),
            memory_limit: resolve(|c| c.memory_limit).map(|mb| format!("{}Mi", mb)),
            cpu_request: resolve(|c| c.cpu_request).map(|u| format!("{}u", u)),
            memory_request: resolve(|c| c.memory_request).map(|mb| format!("{}Mi", mb)),
        }
    }

    fn resource_usage_from_snapshot(
        snapshot: &temps_entities::deployment_config::DeploymentConfigSnapshot,
    ) -> crate::jobs::ResourceUsage {
        crate::jobs::ResourceUsage {
            cpu_limit: snapshot.cpu_limit.map(|value| format!("{value}u")),
            memory_limit: snapshot.memory_limit.map(|value| format!("{value}Mi")),
            cpu_request: snapshot.cpu_request.map(|value| format!("{value}u")),
            memory_request: snapshot.memory_request.map(|value| format!("{value}Mi")),
        }
    }

    fn rollback_snapshot_port_and_replicas(
        snapshot: &temps_entities::deployment_config::DeploymentConfigSnapshot,
        deployment_id: i32,
    ) -> Result<(Option<u16>, u32), DeploymentError> {
        let port = snapshot
            .exposed_port
            .map(u16::try_from)
            .transpose()
            .map_err(|error| {
                DeploymentError::InvalidDeploymentState(format!(
                    "Target deployment {deployment_id} has invalid exposed port {:?}: {error}",
                    snapshot.exposed_port
                ))
            })?;
        let replicas = u32::try_from(snapshot.replicas)
            .ok()
            .filter(|replicas| *replicas > 0)
            .ok_or_else(|| {
                DeploymentError::InvalidDeploymentState(format!(
                    "Target deployment {deployment_id} has invalid replica count {}",
                    snapshot.replicas
                ))
            })?;
        Ok((port, replicas))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Arc<temps_database::DbConnection>,
        log_service: Arc<temps_deployment_logs::LogService>,
        config_service: Arc<temps_config::ConfigService>,
        queue_service: Arc<dyn temps_core::JobQueue>,
        docker_log_service: Arc<temps_deployment_logs::DockerLogService>,
        docker_handle: Arc<temps_core::DockerHandle>,
        deployer: Arc<dyn temps_deployer::ContainerDeployer>,
        image_builder: Arc<dyn temps_deployer::ImageBuilder>,
        encryption_service: Arc<temps_core::EncryptionService>,
    ) -> Self {
        DeploymentService {
            db,
            log_service,
            config_service,
            queue_service,
            docker_log_service,
            docker_handle,
            deployer,
            image_builder,
            encryption_service,
            telemetry: std::sync::OnceLock::new(),
            environment_addresses: std::sync::OnceLock::new(),
            env_resolver: std::sync::OnceLock::new(),
            compose_executor: std::sync::OnceLock::new(),
            audit_logger: std::sync::OnceLock::new(),
            node_scheduler: std::sync::OnceLock::new(),
            workflow_planner: std::sync::OnceLock::new(),
            workload_plane: std::sync::OnceLock::new(),
        }
    }

    /// Late-bind the environment-variable resolver (see the field docs). Called
    /// once during plugin init after the resolver's service deps exist.
    pub fn set_env_resolver(
        &self,
        resolver: Arc<crate::services::env_resolver::DeploymentEnvResolver>,
    ) {
        let _ = self.env_resolver.set(resolver);
    }

    /// Late-bind the Compose executor (see the field docs). Called once
    /// during plugin init after the `Arc<bollard::Docker>` client exists.
    pub fn set_compose_executor(&self, executor: Arc<temps_deployer::compose::ComposeExecutor>) {
        let _ = self.compose_executor.set(executor);
    }

    /// Set the anonymous telemetry reporter used to emit deploy-funnel events
    /// (currently `rollback_triggered`).
    pub fn set_telemetry(&self, reporter: Arc<dyn temps_core::telemetry::TelemetryReporter>) {
        let _ = self.telemetry.set(reporter);
    }

    /// Wire the ADR 068 primary address and deploy warning.
    pub fn set_environment_addresses(
        &self,
        slot: Arc<temps_core::environment_address::EnvironmentAddressSlot>,
    ) {
        let _ = self.environment_addresses.set(slot);
    }

    /// Set the audit sink used for deploy-path security events (ADR 045), so
    /// the rollback and promotion paths record a host-Docker-socket mount the
    /// same way an ordinary deployment does.
    pub fn set_audit_logger(&self, logger: Arc<dyn temps_core::AuditLogger>) {
        let _ = self.audit_logger.set(logger);
    }

    /// Use the cluster scheduler for inline artifact-reuse deployments.
    pub fn set_node_scheduler(&self, scheduler: Arc<crate::services::NodeScheduler>) {
        let _ = self.node_scheduler.set(scheduler);
    }

    pub fn set_workflow_planner(&self, planner: Arc<crate::services::WorkflowPlanner>) {
        let _ = self.workflow_planner.set(planner);
    }

    /// Bind the started workload plane (see the field docs). Later calls are
    /// ignored: a process runs one plane.
    pub fn set_workload_plane(&self, plane: Arc<temps_workload_plane::WorkloadPlane>) {
        let _ = self.workload_plane.set(plane);
    }

    /// The telemetry reporter, or a no-op when none has been wired.
    fn telemetry(&self) -> Arc<dyn temps_core::telemetry::TelemetryReporter> {
        self.telemetry
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(temps_core::telemetry::NoopTelemetryReporter))
    }

    /// Where the image reused by a rollback or promotion lives (and so how a
    /// remote worker obtains it), and which image the reuse is bound to.
    ///
    /// Classified from the deployment that originally produced the image
    /// (`origin_deployment_id`, from [`deployment_asset_origin`]): the
    /// rollback/promotion rows in between record no image metadata of their
    /// own. An uploaded or control-plane-built image must be streamed to the
    /// worker; only a registry image can be pulled there.
    ///
    /// For a registry image, the expected identity is the digest recorded on
    /// the origin's registered external image, when one was recorded. Without
    /// it the deploy job never substitutes the control plane's copy of the tag
    /// for a failed worker pull.
    async fn reuse_image_provenance(
        &self,
        project: &projects::Model,
        target: &deployments::Model,
        origin_deployment_id: i32,
        image_name: &str,
    ) -> Result<ReuseImageProvenance, DeploymentError> {
        // `deployment_asset_origin` usually resolves the origin id from the
        // row's recorded context vars without loading the origin row, so its
        // metadata has to be fetched here.
        let origin = if origin_deployment_id == target.id {
            None
        } else {
            let origin = deployments::Entity::find_by_id(origin_deployment_id)
                .filter(deployments::Column::ProjectId.eq(project.id))
                .one(self.db.as_ref())
                .await?;
            if origin.is_none() {
                warn!(
                    project_id = project.id,
                    deployment_id = target.id,
                    origin_deployment_id,
                    image = %image_name,
                    "Origin deployment of reused image not found in this project; \
                     classifying the image source from the reused deployment's own metadata"
                );
            }
            origin
        };
        let metadata = origin.as_ref().unwrap_or(target).metadata.as_ref();
        let source = crate::jobs::DeployImageSource::for_existing_deployment(
            project.source_type,
            metadata,
            image_name,
        );

        let expected_identity = match (
            source,
            metadata.and_then(|metadata| metadata.external_image_id),
        ) {
            (crate::jobs::DeployImageSource::Registry, Some(external_image_id)) => {
                temps_entities::external_images::Entity::find_by_id(external_image_id)
                    .filter(temps_entities::external_images::Column::ProjectId.eq(project.id))
                    .one(self.db.as_ref())
                    .await?
                    .and_then(|external_image| external_image.digest)
                    .and_then(|digest| crate::jobs::ExpectedImageIdentity::repo_digest(&digest))
            }
            _ => None,
        };

        let worker_build = self
            .reused_worker_build(project.id, origin_deployment_id, image_name)
            .await?;

        Ok(ReuseImageProvenance {
            source,
            expected_identity,
            worker_build,
        })
    }

    /// What the origin deployment's build recorded about the workers holding
    /// `image_name`, for a rollback/promotion that must reach those workers.
    ///
    /// Used for a mixed-architecture worker build always -- the control
    /// plane never holds its other architectures -- for any worker build
    /// when this control plane runs no workloads, since it then has no copy
    /// of the image to stream from, and for a build whose node reported the
    /// image's configuration, which was therefore not copied back. A
    /// full-profile control plane keeps streaming a single-architecture
    /// image from its own copy when the build reported none, as before.
    async fn reused_worker_build(
        &self,
        project_id: i32,
        origin_deployment_id: i32,
        image_name: &str,
    ) -> Result<Option<crate::jobs::ReusedWorkerBuild>, DeploymentError> {
        use temps_entities::deployment_jobs;
        // Only a build of this project's own deployment says where its image lives.
        let origin_in_project = deployments::Entity::find_by_id(origin_deployment_id)
            .filter(deployments::Column::ProjectId.eq(project_id))
            .one(self.db.as_ref())
            .await?
            .is_some();
        if !origin_in_project {
            return Ok(None);
        }
        let build_outputs = deployment_jobs::Entity::find()
            .filter(deployment_jobs::Column::DeploymentId.eq(origin_deployment_id))
            .filter(deployment_jobs::Column::JobType.eq("BuildImageJob"))
            .filter(deployment_jobs::Column::Outputs.is_not_null())
            .order_by_desc(deployment_jobs::Column::Id)
            .one(self.db.as_ref())
            .await?
            .and_then(|job| job.outputs);
        let Some(worker_build) = build_outputs.and_then(|outputs| {
            crate::jobs::ReusedWorkerBuild::from_build_outputs(&outputs, image_name)
        }) else {
            return Ok(None);
        };
        let control_plane_has_no_copy = self
            .node_scheduler
            .get()
            .is_some_and(|scheduler| !scheduler.local_workloads_enabled());
        // A build whose node reported the image's configuration was not
        // copied back to the control plane, so its worker is the source.
        let control_plane_was_not_given_a_copy = worker_build.image_config.is_some();
        Ok((worker_build.is_per_architecture()
            || control_plane_has_no_copy
            || control_plane_was_not_given_a_copy)
            .then_some(worker_build))
    }

    /// Repository health travels with the reused image. Target environment settings
    /// retain their usual precedence over project defaults when no repo probe exists.
    async fn reused_image_health(
        &self,
        origin_deployment_id: i32,
        environment: &environments::Model,
        project: &projects::Model,
    ) -> Result<Option<temps_core::repo_config::HealthCheckConfig>, DeploymentError> {
        use temps_entities::deployment_jobs;
        let outputs = deployment_jobs::Entity::find()
            .filter(deployment_jobs::Column::DeploymentId.eq(origin_deployment_id))
            .filter(deployment_jobs::Column::JobType.eq("BuildImageJob"))
            .filter(deployment_jobs::Column::Outputs.is_not_null())
            .order_by_desc(deployment_jobs::Column::Id)
            .one(self.db.as_ref())
            .await?
            .and_then(|job| job.outputs);
        resolve_reused_health(
            outputs.as_ref(),
            environment.deployment_config.as_ref(),
            project.deployment_config.as_ref(),
        )
    }

    async fn configure_reuse_deploy_builder(
        &self,
        mut builder: crate::jobs::DeployImageJobBuilder,
        project: &projects::Model,
        environment: &environments::Model,
        image_name: &str,
        local_environment_variables: &HashMap<String, String>,
    ) -> crate::jobs::DeployImageJobBuilder {
        if let Some(scheduler) = self.node_scheduler.get() {
            builder = builder.node_scheduler(scheduler.clone());
        }
        builder = builder
            .encryption_service(self.encryption_service.clone())
            .config_service(self.config_service.clone())
            .image_builder(self.image_builder.clone());

        let environment_config = environment.deployment_config.as_ref();
        let project_config = project.deployment_config.as_ref();
        builder = builder.health_check_timeout_secs(
            temps_entities::deployment_config::resolve_health_check_timeout_seconds(
                environment_config
                    .and_then(|config| config.health_check_timeout_seconds)
                    .or_else(|| {
                        project_config.and_then(|config| config.health_check_timeout_seconds)
                    }),
            ),
        );
        if let Some(target_nodes) = environment_config
            .and_then(|config| config.configured_target_nodes().map(<[i32]>::to_vec))
            .or_else(|| {
                project_config
                    .and_then(|config| config.configured_target_nodes().map(<[i32]>::to_vec))
            })
        {
            builder = builder.target_nodes(target_nodes);
        }
        if let Some(target_labels) = environment_config
            .and_then(|config| config.configured_target_labels().cloned())
            .or_else(|| {
                project_config.and_then(|config| config.configured_target_labels().cloned())
            })
        {
            builder = builder.target_labels(target_labels);
        }
        builder = builder.anti_affinity(
            environment_config
                .map(|config| config.anti_affinity)
                .or_else(|| project_config.map(|config| config.anti_affinity))
                .unwrap_or(true),
        );

        if let Some(planner) = self.workflow_planner.get() {
            let remote_plan = planner
                .build_remote_environment_variables(project, local_environment_variables)
                .await;
            builder = builder
                .remote_environment_variables(remote_plan.variables)
                .cross_node_service_blockers(remote_plan.blockers);
        }

        if let Ok(settings) = self.config_service.get_settings().await {
            builder = builder.container_log_config(temps_deployer::ContainerLogConfig::new(
                settings.container_logs.max_size.clone(),
                settings.container_logs.max_file,
            ));
            let registry = &settings.docker_registry;
            if registry.enabled {
                if let (Some(username), Some(password), Some(registry_url)) = (
                    registry.username.clone(),
                    registry.password.clone(),
                    registry.registry_url.clone(),
                ) {
                    let image_registry =
                        crate::jobs::PullExternalImageJob::registry_from_image_ref(image_name);
                    let configured_registry =
                        crate::jobs::PullExternalImageJob::registry_host_from_url(&registry_url);
                    if matches!(
                        (&image_registry, &configured_registry),
                        (Some(image_registry), Some(configured_registry))
                            if image_registry == configured_registry
                    ) {
                        builder = builder.registry_credentials(
                            temps_deployer::remote::RemotePullCredentials {
                                username: Some(username),
                                password: Some(password),
                                identity_token: None,
                                server_address: Some(registry_url),
                            },
                        );
                    } else {
                        warn!(
                            image_registry = ?image_registry,
                            configured_registry = ?configured_registry,
                            "Skipping Docker registry credentials for reused image because the registry does not match"
                        );
                    }
                }
            }
        }

        builder
    }

    async fn fail_reuse_deployment(
        &self,
        deployment_id: i32,
        operation: &str,
        error: impl std::fmt::Display,
    ) -> DeploymentError {
        let reason = format!("{operation} for deployment {deployment_id}: {error}");
        let update = deployments::Entity::update_many()
            .col_expr(
                deployments::Column::State,
                sea_orm::sea_query::Expr::value("failed"),
            )
            .col_expr(
                deployments::Column::FinishedAt,
                sea_orm::sea_query::Expr::value(Some(chrono::Utc::now())),
            )
            .col_expr(
                deployments::Column::CancelledReason,
                sea_orm::sea_query::Expr::value(Some(reason.clone())),
            )
            .filter(deployments::Column::Id.eq(deployment_id))
            .filter(deployments::Column::State.is_in(["pending", "running", "creating"]))
            .exec(self.db.as_ref())
            .await;

        match update {
            Ok(result) if result.rows_affected == 1 => DeploymentError::Other(reason),
            Ok(_) => match deployments::Entity::find_by_id(deployment_id)
                .one(self.db.as_ref())
                .await
            {
                Ok(Some(deployment))
                    if matches!(
                        deployment.state.as_str(),
                        "cancelled" | "stopped" | "completed" | "deployed" | "failed"
                    ) =>
                {
                    DeploymentError::Other(format!(
                        "{reason}; terminal state '{}' was preserved{}",
                        deployment.state,
                        deployment
                            .cancelled_reason
                            .as_deref()
                            .map(|existing| format!(": {existing}"))
                            .unwrap_or_default()
                    ))
                }
                Ok(Some(deployment)) => DeploymentError::DatabaseError {
                    reason: format!(
                        "{reason}; additionally failed to persist terminal state from unexpected state '{}'",
                        deployment.state
                    ),
                },
                Ok(None) => DeploymentError::DatabaseError {
                    reason: format!(
                        "{reason}; additionally failed to persist terminal state because the deployment disappeared"
                    ),
                },
                Err(read_error) => DeploymentError::DatabaseError {
                    reason: format!(
                        "{reason}; additionally failed to verify terminal state: {read_error}"
                    ),
                },
            },
            Err(update_error) => DeploymentError::DatabaseError {
                reason: format!(
                    "{reason}; additionally failed to persist terminal state: {update_error}"
                ),
            },
        }
    }

    async fn complete_static_reuse(
        &self,
        project: &projects::Model,
        environment: &environments::Model,
        deployment: &deployments::Model,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), DeploymentError> {
        let log_id = format!(
            "{}/{}/{}/{:02}/{:02}/{:02}/{:02}/deployment-{}-job-mark_deployment_complete.log",
            project.slug,
            environment.slug,
            now.format("%Y"),
            now.format("%m"),
            now.format("%d"),
            now.format("%H"),
            now.format("%M"),
            deployment.id
        );
        self.log_service
            .create_log_path(&log_id)
            .await
            .map_err(|error| {
                DeploymentError::Other(format!(
                    "Failed to create static completion log path: {error}"
                ))
            })?;

        let job = crate::jobs::MarkDeploymentCompleteJobBuilder::new()
            .environment_addresses(self.environment_addresses.get().cloned())
            .job_id("mark_deployment_complete".to_string())
            .deployment_id(deployment.id)
            .db(self.db.clone())
            .log_id(log_id)
            .log_service(self.log_service.clone())
            .container_deployer(self.deployer.clone())
            .queue(self.queue_service.clone())
            .config_service(self.config_service.clone())
            .encryption_service(self.encryption_service.clone())
            .workload_plane(self.workload_plane.get().cloned())
            .build()
            .map_err(|error| {
                DeploymentError::Other(format!("Failed to create static completion job: {error}"))
            })?;
        let context = temps_core::WorkflowContext::new(
            format!("static-reuse-{}", deployment.id),
            deployment.id,
            project.id,
            environment.id,
            Arc::new(crate::test_utils::MockLogWriter::new(0)),
        );
        match job.execute(context).await {
            Ok(_) => Ok(()),
            Err(error) => {
                self.reconcile_static_completion_error(
                    environment.id,
                    deployment.id,
                    environment.current_deployment_id,
                    error,
                )
                .await
            }
        }
    }

    async fn reconcile_static_completion_error(
        &self,
        environment_id: i32,
        deployment_id: i32,
        previous_deployment_id: Option<i32>,
        error: impl std::fmt::Display,
    ) -> Result<(), DeploymentError> {
        let reason = format!("Failed to complete static reuse: {error}");
        let mut route_receiver = self.queue_service.subscribe();
        let transaction = self.db.begin().await?;
        let environment = environments::Entity::find_by_id(environment_id)
            .filter(environments::Column::DeletedAt.is_null())
            .lock(sea_orm::sea_query::LockType::Update)
            .one(&transaction)
            .await?
            .ok_or_else(|| {
                DeploymentError::NotFound(format!(
                    "Environment {environment_id} not found while reconciling static deployment {deployment_id}"
                ))
            })?;
        let deployment = deployments::Entity::find_by_id(deployment_id)
            .lock(sea_orm::sea_query::LockType::Update)
            .one(&transaction)
            .await?
            .ok_or_else(|| {
                DeploymentError::NotFound(format!(
                    "Deployment {deployment_id} not found while reconciling static completion for environment {environment_id}"
                ))
            })?;

        if environment.current_deployment_id != Some(deployment_id) {
            transaction.commit().await?;
            return Err(DeploymentError::Other(reason));
        }
        if deployment.state == "completed" {
            let completed_environment = environment.clone();
            let completed_deployment = deployment.clone();
            let mut active_environment: environments::ActiveModel = environment.into();
            active_environment.sleeping = Set(false);
            active_environment.updated_at = Set(chrono::Utc::now());
            active_environment.update(&transaction).await?;
            transaction.commit().await?;

            self.queue_service
                .send(temps_core::Job::ForceRouteReload(
                    temps_core::ForceRouteReloadJob {
                        environment_id: Some(environment_id),
                        deployment_id: Some(deployment_id),
                    },
                ))
                .await
                .map_err(|reload_error| DeploymentError::Other(format!(
                    "{reason}; completed deployment remained selected but its route reload request failed: {reload_error}"
                )))?;
            crate::jobs::MarkDeploymentCompleteJob::wait_for_route_ready(
                &mut route_receiver,
                self.db.as_ref(),
                environment_id,
                deployment_id,
                std::time::Duration::from_secs(60),
            )
            .await
            .map_err(|route_error| DeploymentError::Other(format!(
                "{reason}; completed deployment remained selected but route confirmation failed: {route_error}"
            )))?;
            let url = if !completed_environment.host.is_empty() {
                let scheme = self
                    .config_service
                    .get_url_scheme()
                    .await
                    .unwrap_or_else(|_| "https".to_string());
                Some(format!("{}://{}", scheme, completed_environment.host))
            } else if !completed_environment.subdomain.is_empty() {
                self.config_service
                    .get_deployment_url_by_slug(&completed_environment.subdomain)
                    .await
                    .ok()
            } else {
                None
            };
            if let Err(event_error) = self
                .queue_service
                .send(temps_core::Job::DeploymentSucceeded(
                    temps_core::DeploymentSucceededJob {
                        deployment_id,
                        project_id: completed_deployment.project_id,
                        environment_id,
                        environment_name: completed_environment.name,
                        commit_sha: completed_deployment.commit_sha,
                        url,
                        health_check_path: completed_deployment
                            .metadata
                            .and_then(|metadata| metadata.health_check_path),
                    },
                ))
                .await
            {
                warn!(
                    deployment_id,
                    environment_id,
                    error = %event_error,
                    "Failed to re-emit DeploymentSucceeded during static completion reconciliation"
                );
            }
            warn!(
                deployment_id,
                environment_id,
                error = %error,
                "Static deployment completed and remains selected; treating a post-completion side-effect failure as success"
            );
            return Ok(());
        }

        let previous_is_usable = match previous_deployment_id {
            Some(previous_id) => deployments::Entity::find_by_id(previous_id)
                .filter(deployments::Column::EnvironmentId.eq(environment_id))
                .filter(deployments::Column::State.is_in(["completed", "deployed", "stopped"]))
                .one(&transaction)
                .await?
                .is_some(),
            None => true,
        };
        if !previous_is_usable {
            transaction.commit().await?;
            return Err(DeploymentError::Other(format!(
                "{reason}; previous deployment is no longer usable, so the selected route was not changed"
            )));
        }

        let mut active_environment: environments::ActiveModel = environment.into();
        active_environment.current_deployment_id = Set(previous_deployment_id);
        active_environment.update(&transaction).await?;
        transaction.commit().await?;

        self.queue_service
            .send(temps_core::Job::ForceRouteReload(
                temps_core::ForceRouteReloadJob {
                    environment_id: Some(environment_id),
                    deployment_id: previous_deployment_id,
                },
            ))
            .await
            .map_err(|reload_error| DeploymentError::Other(format!(
                "{reason}; restored the previous route in the database but failed to request a route reload: {reload_error}"
            )))?;

        if let Some(previous_id) = previous_deployment_id {
            crate::jobs::MarkDeploymentCompleteJob::wait_for_route_ready(
                &mut route_receiver,
                self.db.as_ref(),
                environment_id,
                previous_id,
                std::time::Duration::from_secs(60),
            )
            .await
            .map_err(|route_error| DeploymentError::Other(format!(
                "{reason}; restored deployment {previous_id} but route confirmation failed: {route_error}"
            )))?;
        }

        Err(DeploymentError::Other(reason))
    }

    pub async fn rollback_to_deployment(
        &self,
        project_id: i32,
        deployment_id: i32,
    ) -> Result<Deployment, DeploymentError> {
        self.rollback_to_deployment_as(
            project_id,
            deployment_id,
            temps_core::docker_socket_grant::DeployCaller::default(),
        )
        .await
    }

    /// [`Self::rollback_to_deployment`] for a caller whose authority is known
    /// (ADR 045).
    pub async fn rollback_to_deployment_as(
        &self,
        project_id: i32,
        deployment_id: i32,
        caller: temps_core::docker_socket_grant::DeployCaller,
    ) -> Result<Deployment, DeploymentError> {
        use temps_entities::deployments::DeploymentMetadata;

        self.guard_granted_project_deploy(project_id, caller)
            .await?;

        // Fetch the target deployment (the one we're rolling back TO)
        let target_deployment = deployments::Entity::find_by_id(deployment_id)
            .filter(deployments::Column::ProjectId.eq(project_id))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::NotFound("Target deployment not found".to_string()))?;

        // Validate that the deployment is in a valid state for rollback.
        //
        // "stopped" belongs here alongside "completed": once a LATER
        // deployment supersedes this one, `cancel_previous_deployments`
        // stops its containers and flips its state to "stopped" (see that
        // function and `teardown_deployment`) -- that is the terminal state
        // every successful-but-no-longer-current deployment actually ends up
        // in. Rollback's whole purpose is reverting to an older deployment,
        // so its target is virtually always going to be "stopped" in
        // practice; excluding it made rollback reject its own primary use
        // case ("Cannot rollback to deployment in 'stopped' state") for any
        // deployment that had already been superseded -- which is every
        // deployment a real user would ever actually want to roll back to.
        // "deployed" is kept for the one other live path that sets it
        // (`resume_deployment`); "failed"/"cancelled"/"paused" stay excluded
        // -- a failed/cancelled deployment has no reliable image to reuse,
        // and rolling back TO a paused deployment is a distinct, not yet
        // supported, operation.
        let valid_rollback_states = ["deployed", "completed", "stopped"];
        if !valid_rollback_states.contains(&target_deployment.state.as_str()) {
            return Err(DeploymentError::InvalidDeploymentState(format!(
                "Cannot rollback to deployment in '{}' state. Only deployed, completed, or stopped (superseded) deployments can be rolled back to.",
                target_deployment.state
            )));
        }

        let environment_id = target_deployment.environment_id;

        let project = projects::Entity::find_by_id(project_id)
            .filter(projects::Column::IsDeleted.eq(false))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::NotFound("Project not found".to_string()))?;

        // An uploaded static bundle may use any static-capable preset (for
        // example Vite). Its retained artifact determines how it is reused,
        // rather than the project's framework label.
        let is_static = target_deployment.static_dir_location.is_some()
            || (target_deployment.image_name.is_none()
                && (project.source_type == temps_entities::source_type::SourceType::StaticFiles
                    || project.preset == temps_entities::preset::Preset::Static));
        if !is_static {
            temps_presets::get_preset_for_storage(project.preset, project.preset_config.as_ref())
                .map_err(|error| DeploymentError::InvalidInput(error.to_string()))?
                .ok_or_else(|| DeploymentError::NotFound("Preset not found".to_string()))?;
        }

        // --- Git projects: rebuild from source when the image isn't reusable ---
        //
        // The artifact-reuse path below redeploys the target's immutable image
        // reference or static asset lineage as-is. The selected worker owns
        // image verification/pull, so a daemon-less control plane never probes
        // its own cache.
        //
        // Git projects fall back to rebuilding the recorded commit only when
        // the durable artifact reference itself is absent. Static deployments
        // reuse their stored asset origin without requiring a server image.
        //
        // Non-git projects (docker_image / static_files / manual without a git
        // ref) have no source to rebuild, so they always use image reuse.
        let has_git_ref = target_deployment
            .commit_sha
            .as_ref()
            .is_some_and(|c| !c.is_empty())
            || target_deployment
                .branch_ref
                .as_ref()
                .is_some_and(|b| !b.is_empty());

        // Artifact availability is represented by durable deployment metadata.
        // The selected worker verifies or pulls a container image; the control
        // plane may intentionally have no Docker daemon at all.
        let artifact_available = if is_static {
            target_deployment
                .static_dir_location
                .as_ref()
                .is_some_and(|location| !location.is_empty())
        } else {
            target_deployment.image_name.as_ref().is_some_and(|image| {
                !image.is_empty() && !image.starts_with(&format!("temps-{}:", project.slug))
            })
        };

        if project.source_type == temps_entities::source_type::SourceType::Git
            && has_git_ref
            && !artifact_available
        {
            info!(
                "Rollback: project {} is git-sourced and the target image is unavailable ({}) — rebuilding from source at commit {:?} (rolling back to #{})",
                project_id,
                if is_static { "static artifact missing" } else { "image reference missing" },
                target_deployment.commit_sha,
                deployment_id
            );

            // Snapshot the latest deployment id BEFORE triggering, so we can
            // identify the one the pipeline creates and return it.
            let prev_max_id = deployments::Entity::find()
                .filter(deployments::Column::ProjectId.eq(project_id))
                .filter(deployments::Column::EnvironmentId.eq(environment_id))
                .order_by_desc(deployments::Column::Id)
                .one(self.db.as_ref())
                .await?
                .map(|d| d.id)
                .unwrap_or(0);

            self.trigger_pipeline_inner(
                project_id,
                environment_id,
                PipelineTriggerOptions {
                    branch: target_deployment.branch_ref.clone(),
                    tag: target_deployment.tag_ref.clone(),
                    commit: target_deployment.commit_sha.clone(),
                    rollback_from_deployment_id: Some(deployment_id),
                    recovery_of_deployment_id: None,
                },
            )
            .await?;

            // Anonymous telemetry: a rollback was initiated. No identifying props.
            self.telemetry()
                .report(temps_core::telemetry::TelemetryEvent::new(
                    temps_core::telemetry::TelemetryEventKind::RollbackTriggered,
                ));

            // The pipeline created a new deployment row; return it so the API
            // response carries the rollback deployment's id/status. It's the
            // newest row for this environment above the prior max.
            let created = deployments::Entity::find()
                .filter(deployments::Column::ProjectId.eq(project_id))
                .filter(deployments::Column::EnvironmentId.eq(environment_id))
                .filter(deployments::Column::Id.gt(prev_max_id))
                .order_by_desc(deployments::Column::Id)
                .one(self.db.as_ref())
                .await?;

            let model = match created {
                Some(dep) => dep,
                // The job is queued; the row may not be visible yet. Surface the
                // target as a stand-in rather than failing — the rollback is
                // already in flight.
                None => target_deployment,
            };
            return Ok(self
                .map_db_deployment_to_deployment(model, false, None)
                .await);
        }

        let image_name = target_deployment.image_name.clone();

        let environment = environments::Entity::find_by_id(environment_id)
            .filter(environments::Column::DeletedAt.is_null())
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::NotFound("Environment not found".to_string()))?;

        info!(
            "Initiating rollback for project_id: {}, to deployment_id: {}, image: {}, environment_id: {}",
            project_id,
            deployment_id,
            image_name.as_deref().unwrap_or("static artifact"),
            environment_id
        );

        // --- Create a NEW deployment record for the rollback ---
        // This gives us fresh timestamps, a unique slug, and proper tracking.
        let now = chrono::Utc::now();

        // Get next deployment number
        let deployment_count = deployments::Entity::find()
            .filter(deployments::Column::ProjectId.eq(project_id))
            .count(self.db.as_ref())
            .await
            .map_err(|e| DeploymentError::Other(format!("Failed to count deployments: {}", e)))?;
        let deployment_number = deployment_count + 1;

        let rollback_slug = format!("{}-{}", project.slug, deployment_number);

        let rollback_asset_origin =
            deployment_asset_origin(self.db.as_ref(), &target_deployment).await?;
        let rollback_health_check_path = target_deployment
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.health_check_path.clone());
        let rollback_command = target_deployment
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.command.clone());
        let rollback_metadata = DeploymentMetadata {
            is_rollback: true,
            rolled_back_from_id: Some(deployment_id),
            health_check_path: rollback_health_check_path.clone(),
            command: rollback_command.clone(),
            runtime_overrides: target_deployment
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.runtime_overrides.clone()),
            ..Default::default()
        };

        let new_deployment = deployments::ActiveModel {
            id: sea_orm::NotSet,
            project_id: Set(project_id),
            environment_id: Set(environment_id),
            slug: Set(rollback_slug.clone()),
            state: Set("pending".to_string()),
            metadata: Set(Some(rollback_metadata)),
            branch_ref: Set(target_deployment.branch_ref.clone()),
            tag_ref: Set(target_deployment.tag_ref.clone()),
            commit_sha: Set(target_deployment.commit_sha.clone()),
            commit_message: Set(target_deployment.commit_message.clone()),
            commit_author: Set(target_deployment.commit_author.clone()),
            commit_json: Set(target_deployment.commit_json.clone()),
            image_name: Set(image_name.clone()),
            started_at: Set(Some(now)),
            finished_at: Set(None),
            deploying_at: Set(Some(now)),
            ready_at: Set(None),
            static_dir_location: Set(target_deployment.static_dir_location.clone()),
            screenshot_location: Set(None),
            cancelled_reason: Set(None),
            context_vars: Set(Some(serde_json::json!({
                "trigger": "rollback",
                "source_deployment_id": rollback_asset_origin.deployment_id,
                "source_deployment_slug": rollback_asset_origin.slug.clone(),
                "source_environment_id": rollback_asset_origin.environment_id,
            }))),
            deployment_config: Set(target_deployment.deployment_config.clone()),
            promoted_from_deployment_id: Set(None),
            upload_request_id: Set(None),
            docker_socket_mounted: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
        };

        let rollback_deployment = insert_deployment_with_generation_lock(
            self.db.as_ref(),
            project_id,
            environment_id,
            new_deployment,
        )
        .await
        .map_err(|e| {
            DeploymentError::Other(format!("Failed to create rollback deployment: {}", e))
        })?;

        let rollback_deployment_id = rollback_deployment.id;
        info!(
            "Created rollback deployment #{} (rolling back to #{}, artifact: {})",
            rollback_deployment_id,
            deployment_id,
            image_name.as_deref().unwrap_or("static")
        );

        let admitted = match super::job_processor::JobProcessorService::try_admit_deployment(
            self.db.as_ref(),
            rollback_deployment_id,
        )
        .await
        {
            Ok(admitted) => admitted,
            Err(error) => {
                return Err(self
                    .fail_reuse_deployment(
                        rollback_deployment_id,
                        "Failed to admit rollback",
                        error,
                    )
                    .await);
            }
        };
        if !admitted {
            return Err(self
                .fail_reuse_deployment(
                    rollback_deployment_id,
                    "Rollback was not admitted",
                    "its project or environment is being deleted",
                )
                .await);
        }

        // Anonymous telemetry: a rollback was initiated. No identifying props.
        self.telemetry()
            .report(temps_core::telemetry::TelemetryEvent::new(
                temps_core::telemetry::TelemetryEventKind::RollbackTriggered,
            ));

        // From this point onward the deployment row exists and has been
        // admitted. Keep every fallible setup/finalization step inside one
        // boundary so no `?` can leave it pending or running indefinitely.
        let execution_result: Result<(), DeploymentError> = async {
        // Check if preset is static - if so, just update environment without deploying
        if is_static {
            info!("Rollback: Static preset detected - updating environment only");

            if let Err(error) = self
                .complete_static_reuse(
                    &project,
                    &environment,
                    &rollback_deployment,
                    now,
                )
                .await
            {
                return Err(self
                    .fail_reuse_deployment(
                        rollback_deployment_id,
                        "Failed to activate static rollback",
                        error,
                    )
                    .await);
            }

            info!(
                "Rollback completed - environment {} now points to rollback deployment {}",
                environment_id, rollback_deployment_id
            );
        } else {
            let image_name = image_name.ok_or_else(|| {
                DeploymentError::Other(format!(
                    "Target deployment {deployment_id} has no reusable image reference"
                ))
            })?;

            // --- Create per-job log paths (matching normal deployment pattern) ---
            let deploy_log_id = format!(
                "{}/{}/{}/{:02}/{:02}/{:02}/{:02}/deployment-{}-job-deploy_container.log",
                project.slug,
                environment.slug,
                now.format("%Y"),
                now.format("%m"),
                now.format("%d"),
                now.format("%H"),
                now.format("%M"),
                rollback_deployment_id
            );
            let complete_log_id = format!(
                "{}/{}/{}/{:02}/{:02}/{:02}/{:02}/deployment-{}-job-mark_deployment_complete.log",
                project.slug,
                environment.slug,
                now.format("%Y"),
                now.format("%m"),
                now.format("%d"),
                now.format("%H"),
                now.format("%M"),
                rollback_deployment_id
            );

            self.log_service
                .create_log_path(&deploy_log_id)
                .await
                .map_err(|e| {
                    DeploymentError::Other(format!("Failed to create deploy log path: {}", e))
                })?;
            self.log_service
                .create_log_path(&complete_log_id)
                .await
                .map_err(|e| {
                    DeploymentError::Other(format!("Failed to create complete log path: {}", e))
                })?;

            // --- Create deployment_jobs records so the API can return them ---
            use temps_entities::{deployment_jobs, types::JobStatus};

            let deploy_job_record = deployment_jobs::ActiveModel {
                deployment_id: Set(rollback_deployment_id),
                job_id: Set("deploy_container".to_string()),
                job_type: Set("DeployImageJob".to_string()),
                name: Set("Deploy Container".to_string()),
                description: Set(Some(format!("Rollback: deploy image {}", image_name))),
                status: Set(JobStatus::Running),
                log_id: Set(deploy_log_id.clone()),
                job_config: Set(None),
                dependencies: Set(None),
                execution_order: Set(Some(0)),
                started_at: Set(Some(now)),
                ..Default::default()
            };
            let deploy_job_model =
                deploy_job_record
                    .insert(self.db.as_ref())
                    .await
                    .map_err(|e| {
                        DeploymentError::Other(format!("Failed to create deploy job record: {}", e))
                    })?;

            let complete_job_record = deployment_jobs::ActiveModel {
                deployment_id: Set(rollback_deployment_id),
                job_id: Set("mark_deployment_complete".to_string()),
                job_type: Set("MarkDeploymentCompleteJob".to_string()),
                name: Set("Mark Deployment Complete".to_string()),
                description: Set(Some("Finalize rollback deployment".to_string())),
                status: Set(JobStatus::Pending),
                log_id: Set(complete_log_id.clone()),
                job_config: Set(None),
                dependencies: Set(Some(
                    serde_json::to_value(vec!["deploy_container"]).unwrap_or_default(),
                )),
                execution_order: Set(Some(1)),
                ..Default::default()
            };
            let complete_job_model =
                complete_job_record
                    .insert(self.db.as_ref())
                    .await
                    .map_err(|e| {
                        DeploymentError::Other(format!(
                            "Failed to create complete job record: {}",
                            e
                        ))
                    })?;

            info!("Rollback: Deploying image: {}", image_name);

            // Step 1: Execute DeployImageJob with external image
            // Use the NEW rollback slug as the container name (not the old deployment's slug)
            let (configured_port, rollback_replicas, rollback_resources) =
                if let Some(snapshot) = target_deployment.deployment_config.as_ref() {
                    let (port, replicas) =
                        Self::rollback_snapshot_port_and_replicas(snapshot, deployment_id)?;
                    (port, replicas, Self::resource_usage_from_snapshot(snapshot))
                } else {
                    (
                        super::port_resolver::configured_port_override(&environment, &project),
                        environment
                            .deployment_config
                            .as_ref()
                            .map(|config| config.replicas as u32)
                            .or_else(|| {
                                project
                                    .deployment_config
                                    .as_ref()
                                    .map(|config| config.replicas as u32)
                            })
                            .unwrap_or(1),
                        Self::resolve_resource_usage(
                            environment.deployment_config.as_ref(),
                            project.deployment_config.as_ref(),
                        ),
                    )
                };
            let exposed_port = configured_port.map(u32::from).unwrap_or(3000);
            // ADR 045: the slug travels with every deploy, including this
            // one. A rollback or promotion of a granted project that omitted
            // it would be placed anywhere and started without its socket.
            let mut deploy_builder =
                crate::jobs::DeployImageJobBuilder::new(project.slug.clone(), caller)
                    // ADR 045: the same audit sink an ordinary deploy uses, so a
                    // rollback/promotion that mounts the socket is recorded rather
                    // than only logged.
                    .audit_logger(self.audit_logger.get().cloned())
                    .job_id("deploy_container".to_string())
                    .build_job_id("external-image".to_string())
                    .target(crate::jobs::DeploymentTarget::Docker {
                        registry_url: "local".to_string(),
                        network: Some(temps_core::NETWORK_NAME.to_string()),
                    })
                    .service_name(rollback_slug.clone())
                    .health_check_path(None)
                    .health_check_path_override(rollback_health_check_path)
                    .health(self.reused_image_health(rollback_asset_origin.deployment_id, &environment, &project).await?)
                    .command(target_deployment.metadata.as_ref()
                        .and_then(|metadata| metadata.container_runtime().command))
                    .entrypoint(target_deployment.metadata.as_ref()
                        .and_then(|metadata| metadata.container_runtime().entrypoint))
                    .replicas(rollback_replicas)
                    .port(exposed_port)
                    .configured_port(configured_port)
                    .log_id(deploy_log_id.clone())
                    .log_service(self.log_service.clone())
                    .failed_container_retention(self.db.clone(), rollback_deployment_id);

            // Resolve CPU/memory limits + requests (env → project), matching the
            // normal deploy path (WorkflowExecutionService). Each field resolves
            // independently; when neither side configures a value it stays unset
            // so the deployer applies no Docker limit. Without this, a rollback
            // would inherit `ResourceUsage::default()` (now all-None) and silently
            // drop a configured limit — or, before the default was fixed, cap an
            // unconfigured environment.
            deploy_builder = deploy_builder.resources(rollback_resources);

            // Resolve the environment's env vars exactly as a normal deploy does,
            // so the rolled-back container boots with the full set (user vars,
            // external-service connection strings, SENTRY_DSN, TEMPS_API_TOKEN,
            // CRON_SECRET, OTEL_*) instead of nothing. Without this, a rollback
            // reuses the image but starts it unconfigured.
            let resolved_env = if let Some(resolver) = self.env_resolver.get() {
                let mut resolved = resolver
                    .resolve(&project, &environment, &rollback_deployment)
                    .await?;
                crate::services::env_resolver::apply_deployment_owned_variables(
                    &mut resolved,
                    project.preset,
                    &rollback_asset_origin.slug,
                    (project.preset != temps_entities::preset::Preset::DockerCompose)
                        .then_some(exposed_port),
                );
                resolved
            } else {
                tracing::warn!(
                    "Rollback: env resolver not wired — rolled-back container starts with no resolved env vars"
                );
                std::collections::HashMap::new()
            };
            deploy_builder = deploy_builder.environment_variables(resolved_env.clone());

            deploy_builder = self
                .configure_reuse_deploy_builder(
                    deploy_builder,
                    &project,
                    &environment,
                    &image_name,
                    &resolved_env,
                )
                .await;

            let provenance = self
                .reuse_image_provenance(
                    &project,
                    &target_deployment,
                    rollback_asset_origin.deployment_id,
                    &image_name,
                )
                .await?;
            info!(
                "Rollback: image {} is {}-sourced (origin deployment #{}, expected identity: {})",
                image_name,
                provenance.source,
                rollback_asset_origin.deployment_id,
                provenance.expected_identity_label()
            );

            let deploy_job = provenance.apply(
                deploy_builder
                    .build(self.deployer.clone())
                    .map_err(|e| {
                        DeploymentError::Other(format!("Failed to create deploy job: {}", e))
                    })?
                    .with_external_image_tag(image_name.clone()),
            );

            // Create workflow context for the NEW rollback deployment
            let mock_log_writer = Arc::new(crate::test_utils::MockLogWriter::new(0));
            let mut rollback_context = temps_core::WorkflowContext::new(
                format!("rollback-{}", rollback_deployment_id),
                rollback_deployment_id,
                project_id,
                environment_id,
                mock_log_writer,
            );

            let cancellation_provider =
                super::workflow_execution_service::DatabaseCancellationProvider::new(
                    self.db.clone(),
                    rollback_deployment_id,
                );
            match deploy_job
                .execute_with_cancellation(rollback_context.clone(), &cancellation_provider)
                .await
            {
                Ok(job_result) => {
                    info!("Rollback: Deploy job completed successfully");
                    rollback_context = job_result.context;

                    // Update deploy job record to Success
                    let mut active_job: deployment_jobs::ActiveModel = deploy_job_model.into();
                    active_job.status = Set(temps_entities::types::JobStatus::Success);
                    active_job.finished_at = Set(Some(chrono::Utc::now()));
                    let _ = active_job.update(self.db.as_ref()).await;
                }
                Err(e) => {
                    error!("Rollback: Deploy job failed: {}", e);
                    let failure_message = match deploy_job.cleanup(&rollback_context).await {
                        Ok(()) => format!("Deploy failed: {e}"),
                        Err(cleanup_error) => format!(
                            "Deploy failed: {e}; rollback container cleanup also failed: {cleanup_error}"
                        ),
                    };

                    // Update deploy job record to Failure
                    let mut active_job: deployment_jobs::ActiveModel = deploy_job_model.into();
                    active_job.status = Set(temps_entities::types::JobStatus::Failure);
                    active_job.finished_at = Set(Some(chrono::Utc::now()));
                    active_job.error_message = Set(Some(failure_message.clone()));
                    let _ = active_job.update(self.db.as_ref()).await;

                    // Cancel the pending complete job
                    let mut active_complete: deployment_jobs::ActiveModel =
                        complete_job_model.into();
                    active_complete.status = Set(JobStatus::Cancelled);
                    active_complete.error_message = Set(Some("Deploy job failed".to_string()));
                    let _ = active_complete.update(self.db.as_ref()).await;

                    // Mark the rollback deployment as failed
                    let mut active_dep: deployments::ActiveModel =
                        rollback_deployment.clone().into();
                    active_dep.state = Set("failed".to_string());
                    active_dep.finished_at = Set(Some(chrono::Utc::now()));
                    active_dep.cancelled_reason = Set(Some(failure_message.clone()));
                    let _ = active_dep.update(self.db.as_ref()).await;

                    return Err(DeploymentError::Other(format!(
                        "Failed to deploy image during rollback: {failure_message}"
                    )));
                }
            }

            // Step 2: Execute MarkDeploymentCompleteJob on the NEW rollback deployment
            info!(
                "Rollback: Marking deployment {} as complete",
                rollback_deployment_id
            );

            // Update complete job to Running
            let mut active_complete: deployment_jobs::ActiveModel = complete_job_model.into();
            active_complete.status = Set(JobStatus::Running);
            active_complete.started_at = Set(Some(chrono::Utc::now()));
            let complete_job_model =
                active_complete
                    .update(self.db.as_ref())
                    .await
                    .map_err(|e| {
                        DeploymentError::Other(format!(
                            "Failed to update complete job status: {}",
                            e
                        ))
                    })?;

            let mark_complete_job = crate::jobs::MarkDeploymentCompleteJobBuilder::new()
                .environment_addresses(self.environment_addresses.get().cloned())
                .job_id("mark_deployment_complete".to_string())
                .deployment_id(rollback_deployment_id)
                .db(self.db.clone())
                .log_id(complete_log_id)
                .log_service(self.log_service.clone())
                .container_deployer(self.deployer.clone())
                .queue(self.queue_service.clone())
                .config_service(self.config_service.clone())
                .encryption_service(self.encryption_service.clone())
                .workload_plane(self.workload_plane.get().cloned())
                .build()
                .map_err(|e| {
                    DeploymentError::Other(format!("Failed to create mark complete job: {}", e))
                })?;

            // `execute` consumes the context; keep a copy for the failure
            // path's cleanup below.
            let cleanup_context = rollback_context.clone();
            match mark_complete_job.execute(rollback_context).await {
                Ok(_) => {
                    info!("Rollback: Mark complete job executed successfully");

                    // Update complete job record to Success
                    let mut active_job: deployment_jobs::ActiveModel = complete_job_model.into();
                    active_job.status = Set(temps_entities::types::JobStatus::Success);
                    active_job.finished_at = Set(Some(chrono::Utc::now()));
                    let _ = active_job.update(self.db.as_ref()).await;
                }
                Err(e) => {
                    error!("Rollback: Mark complete job failed: {}", e);

                    // The deploy step already started this deployment's
                    // containers. The workflow executor runs the job's
                    // cleanup on failure; this hand-rolled path must do the
                    // same, or a readiness or route failure leaves them
                    // running with no owner: failed deployments are excluded
                    // from the previous-deployment sweep. `cleanup` leaves a
                    // deployment that already reached completed/deployed
                    // alone, so a late failure never tears down a serving
                    // deployment.
                    let failure_message = clean_up_failed_completion(
                        &mark_complete_job,
                        &cleanup_context,
                        &e,
                        "rollback",
                    )
                    .await;

                    let mut active_job: deployment_jobs::ActiveModel = complete_job_model.into();
                    active_job.status = Set(temps_entities::types::JobStatus::Failure);
                    active_job.finished_at = Set(Some(chrono::Utc::now()));
                    active_job.error_message = Set(Some(failure_message.clone()));
                    let _ = active_job.update(self.db.as_ref()).await;

                    return Err(DeploymentError::Other(format!(
                        "Failed to mark deployment complete during rollback: {failure_message}"
                    )));
                }
            }

            info!(
                "Rollback completed - deployment {} is now active",
                rollback_deployment_id
            );
        }
        Ok(())
        }
        .await;
        if let Err(error) = execution_result {
            return Err(self
                .fail_reuse_deployment(
                    rollback_deployment_id,
                    "Rollback artifact reuse failed",
                    error,
                )
                .await);
        }

        // Re-fetch the rollback deployment to get the final state
        let final_deployment = deployments::Entity::find_by_id(rollback_deployment_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::Other("Rollback deployment disappeared".to_string()))?;

        Ok(self
            .map_db_deployment_to_deployment(final_deployment, true, None)
            .await)
    }

    /// Promote a deployment to a different environment.
    /// Creates a new deployment in the target environment using the source
    /// deployment's image. The target environment must belong to the same project.
    pub async fn promote_deployment(
        &self,
        project_id: i32,
        source_deployment_id: i32,
        target_environment_id: i32,
    ) -> Result<Deployment, DeploymentError> {
        self.promote_deployment_as(
            project_id,
            source_deployment_id,
            target_environment_id,
            temps_core::docker_socket_grant::DeployCaller::default(),
        )
        .await
    }

    /// [`Self::promote_deployment`] for a caller whose authority is known
    /// (ADR 045).
    pub async fn promote_deployment_as(
        &self,
        project_id: i32,
        source_deployment_id: i32,
        target_environment_id: i32,
        caller: temps_core::docker_socket_grant::DeployCaller,
    ) -> Result<Deployment, DeploymentError> {
        use temps_entities::deployments::DeploymentMetadata;

        self.guard_granted_project_deploy(project_id, caller)
            .await?;

        // Fetch the source deployment
        let source = deployments::Entity::find_by_id(source_deployment_id)
            .filter(deployments::Column::ProjectId.eq(project_id))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| {
                DeploymentError::NotFound(format!(
                    "Source deployment {} not found in project {}",
                    source_deployment_id, project_id
                ))
            })?;

        // Validate state — only successful deployments can be promoted.
        // "stopped" is included for the same reason `rollback_to_deployment`
        // includes it (see the comment there): a source deployment that has
        // since been superseded by a newer one in ITS OWN environment is
        // "stopped", not "completed" -- and promoting an older, already-
        // superseded deployment's image into a different environment is
        // exactly the kind of thing a real user does (e.g. re-promote a
        // known-good build after a bad one shipped on top of it).
        let valid_states = ["deployed", "completed", "ready", "stopped"];
        if !valid_states.contains(&source.state.as_str()) {
            return Err(DeploymentError::InvalidDeploymentState(format!(
                "Cannot promote deployment in '{}' state. Only deployed/completed/ready/stopped deployments can be promoted.",
                source.state
            )));
        }

        // Static deployments reuse their durable asset origin and do not have
        // a container image. Validate the appropriate artifact after the
        // project preset has been resolved below.
        let image_name = source.image_name.clone();

        // Fetch target environment and verify it belongs to the same project
        let target_env = environments::Entity::find_by_id(target_environment_id)
            .filter(environments::Column::ProjectId.eq(project_id))
            .filter(environments::Column::DeletedAt.is_null())
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| {
                DeploymentError::NotFound(format!(
                    "Target environment {} not found in project {}",
                    target_environment_id, project_id
                ))
            })?;

        let project = projects::Entity::find_by_id(project_id)
            .filter(projects::Column::IsDeleted.eq(false))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::NotFound("Project not found".to_string()))?;

        info!(
            "Promoting deployment {} to environment '{}' (project {}, image: {})",
            source_deployment_id,
            target_env.name,
            project_id,
            image_name.as_deref().unwrap_or("static artifact")
        );

        let is_static = source.static_dir_location.is_some()
            || (source.image_name.is_none()
                && (project.source_type == temps_entities::source_type::SourceType::StaticFiles
                    || project.preset == temps_entities::preset::Preset::Static));
        if !is_static {
            temps_presets::get_preset_for_storage(project.preset, project.preset_config.as_ref())
                .map_err(|error| DeploymentError::InvalidInput(error.to_string()))?
                .ok_or_else(|| DeploymentError::NotFound("Preset not found".to_string()))?;
        }

        if is_static
            && source
                .static_dir_location
                .as_ref()
                .is_none_or(|location| location.is_empty())
        {
            return Err(DeploymentError::Other(format!(
                "Source deployment {source_deployment_id} has no reusable static artifact"
            )));
        }

        let now = chrono::Utc::now();

        // Get next deployment number
        let deployment_count = deployments::Entity::find()
            .filter(deployments::Column::ProjectId.eq(project_id))
            .count(self.db.as_ref())
            .await
            .map_err(|e| DeploymentError::Other(format!("Failed to count deployments: {}", e)))?;
        let deployment_number = deployment_count + 1;

        let promote_slug = format!("{}-{}", project.slug, deployment_number);

        let promote_metadata = DeploymentMetadata {
            runtime_overrides: source
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.runtime_overrides.clone()),
            // Reuse build info from source
            builder: source.metadata.as_ref().and_then(|m| m.builder.clone()),
            image_size_bytes: source.metadata.as_ref().and_then(|m| m.image_size_bytes),
            health_check_path: source
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.health_check_path.clone()),
            command: source
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.command.clone()),
            ..Default::default()
        };

        // Merge deployment config for the target environment
        let merged_config = if let Some(project_config) = &project.deployment_config {
            if let Some(env_config) = &target_env.deployment_config {
                Some(project_config.merge(env_config))
            } else {
                Some(project_config.clone())
            }
        } else {
            target_env.deployment_config.clone()
        };

        let deployment_config_snapshot = merged_config.map(|config| {
            temps_entities::deployment_config::DeploymentConfigSnapshot::from_config(
                &config,
                std::collections::HashMap::new(),
            )
        });

        let promotion_asset_origin = deployment_asset_origin(self.db.as_ref(), &source).await?;
        let new_deployment = deployments::ActiveModel {
            id: sea_orm::NotSet,
            project_id: Set(project_id),
            environment_id: Set(target_environment_id),
            slug: Set(promote_slug.clone()),
            state: Set("pending".to_string()),
            metadata: Set(Some(promote_metadata)),
            branch_ref: Set(source.branch_ref.clone()),
            tag_ref: Set(source.tag_ref.clone()),
            commit_sha: Set(source.commit_sha.clone()),
            commit_message: Set(source.commit_message.clone()),
            commit_author: Set(source.commit_author.clone()),
            commit_json: Set(source.commit_json.clone()),
            image_name: Set(image_name.clone()),
            started_at: Set(Some(now)),
            finished_at: Set(None),
            deploying_at: Set(Some(now)),
            ready_at: Set(None),
            static_dir_location: Set(source.static_dir_location.clone()),
            screenshot_location: Set(None),
            cancelled_reason: Set(None),
            context_vars: Set(Some(serde_json::json!({
                "trigger": "promotion",
                "source_deployment_id": promotion_asset_origin.deployment_id,
                "source_deployment_slug": promotion_asset_origin.slug.clone(),
                "source_environment_id": promotion_asset_origin.environment_id,
            }))),
            deployment_config: Set(deployment_config_snapshot),
            promoted_from_deployment_id: Set(Some(source_deployment_id)),
            upload_request_id: Set(None),
            docker_socket_mounted: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
        };

        let promoted_deployment = insert_deployment_with_generation_lock(
            self.db.as_ref(),
            project_id,
            target_environment_id,
            new_deployment,
        )
        .await
        .map_err(|e| {
            DeploymentError::Other(format!("Failed to create promoted deployment: {}", e))
        })?;

        let promoted_id = promoted_deployment.id;
        info!(
            "Created promoted deployment #{} (from #{} to environment '{}')",
            promoted_id, source_deployment_id, target_env.name
        );

        let admitted = match super::job_processor::JobProcessorService::try_admit_deployment(
            self.db.as_ref(),
            promoted_id,
        )
        .await
        {
            Ok(admitted) => admitted,
            Err(error) => {
                return Err(self
                    .fail_reuse_deployment(promoted_id, "Failed to admit promotion", error)
                    .await);
            }
        };
        if !admitted {
            return Err(self
                .fail_reuse_deployment(
                    promoted_id,
                    "Promotion was not admitted",
                    "its project or environment is being deleted",
                )
                .await);
        }

        // As with rollback, every error after admission must converge the new
        // row to a durable terminal state.
        let execution_result: Result<(), DeploymentError> = async {
        // Same logic as rollback — for static presets, just update env pointer
        if is_static {
            info!("Promotion: Static preset detected — updating environment only");

            if let Err(error) = self
                .complete_static_reuse(&project, &target_env, &promoted_deployment, now)
                .await
            {
                return Err(self
                    .fail_reuse_deployment(
                        promoted_id,
                        "Failed to activate static promotion",
                        error,
                    )
                    .await);
            }
        } else {
            let image_name = image_name.ok_or_else(|| {
                DeploymentError::Other(format!(
                    "Source deployment {source_deployment_id} has no reusable image reference"
                ))
            })?;

            // --- Create per-job log paths (matching rollback/normal deployment pattern) ---
            let deploy_log_id = format!(
                "{}/{}/{}/{:02}/{:02}/{:02}/{:02}/deployment-{}-job-deploy_container.log",
                project.slug,
                target_env.slug,
                now.format("%Y"),
                now.format("%m"),
                now.format("%d"),
                now.format("%H"),
                now.format("%M"),
                promoted_id
            );
            let complete_log_id = format!(
                "{}/{}/{}/{:02}/{:02}/{:02}/{:02}/deployment-{}-job-mark_deployment_complete.log",
                project.slug,
                target_env.slug,
                now.format("%Y"),
                now.format("%m"),
                now.format("%d"),
                now.format("%H"),
                now.format("%M"),
                promoted_id
            );

            self.log_service
                .create_log_path(&deploy_log_id)
                .await
                .map_err(|e| {
                    DeploymentError::Other(format!("Failed to create deploy log path: {}", e))
                })?;
            self.log_service
                .create_log_path(&complete_log_id)
                .await
                .map_err(|e| {
                    DeploymentError::Other(format!("Failed to create complete log path: {}", e))
                })?;

            // --- Create deployment_jobs records ---
            use temps_entities::{deployment_jobs, types::JobStatus};

            let deploy_job_record = deployment_jobs::ActiveModel {
                deployment_id: Set(promoted_id),
                job_id: Set("deploy_container".to_string()),
                job_type: Set("DeployImageJob".to_string()),
                name: Set("Deploy Container".to_string()),
                description: Set(Some(format!("Promote: deploy image {}", image_name))),
                status: Set(JobStatus::Running),
                log_id: Set(deploy_log_id.clone()),
                job_config: Set(None),
                dependencies: Set(None),
                execution_order: Set(Some(0)),
                started_at: Set(Some(now)),
                ..Default::default()
            };
            let deploy_job_model =
                deploy_job_record
                    .insert(self.db.as_ref())
                    .await
                    .map_err(|e| {
                        DeploymentError::Other(format!("Failed to create deploy job record: {}", e))
                    })?;

            let complete_job_record = deployment_jobs::ActiveModel {
                deployment_id: Set(promoted_id),
                job_id: Set("mark_deployment_complete".to_string()),
                job_type: Set("MarkDeploymentCompleteJob".to_string()),
                name: Set("Mark Deployment Complete".to_string()),
                description: Set(Some("Finalize promoted deployment".to_string())),
                status: Set(JobStatus::Pending),
                log_id: Set(complete_log_id.clone()),
                job_config: Set(None),
                dependencies: Set(Some(
                    serde_json::to_value(vec!["deploy_container"]).unwrap_or_default(),
                )),
                execution_order: Set(Some(1)),
                ..Default::default()
            };
            let complete_job_model =
                complete_job_record
                    .insert(self.db.as_ref())
                    .await
                    .map_err(|e| {
                        DeploymentError::Other(format!(
                            "Failed to create complete job record: {}",
                            e
                        ))
                    })?;

            info!("Promotion: Deploying image: {}", image_name);

            // Execute DeployImageJob with external image
            let configured_port =
                super::port_resolver::configured_port_override(&target_env, &project);
            let exposed_port = configured_port.map(u32::from).unwrap_or(3000);
            // ADR 045: the slug travels with every deploy, including this
            // one. A rollback or promotion of a granted project that omitted
            // it would be placed anywhere and started without its socket.
            let mut deploy_builder =
                crate::jobs::DeployImageJobBuilder::new(project.slug.clone(), caller)
                    // ADR 045: the same audit sink an ordinary deploy uses, so a
                    // rollback/promotion that mounts the socket is recorded rather
                    // than only logged.
                    .audit_logger(self.audit_logger.get().cloned())
                    .job_id("deploy_container".to_string())
                    .build_job_id("external-image".to_string())
                    .health(self.reused_image_health(promotion_asset_origin.deployment_id, &target_env, &project).await?)
                    .target(crate::jobs::DeploymentTarget::Docker {
                        registry_url: "local".to_string(),
                        network: Some(temps_core::NETWORK_NAME.to_string()),
                    })
                    .service_name(promote_slug.clone())
                    .health_check_path(None)
                    .health_check_path_override(
                        promoted_deployment
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.health_check_path.clone()),
                    )
                    .command(
                        promoted_deployment
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.container_runtime().command),
                    )
                    .entrypoint(promoted_deployment.metadata.as_ref()
                        .and_then(|metadata| metadata.container_runtime().entrypoint))
                    .replicas(
                        target_env
                            .deployment_config
                            .as_ref()
                            .map(|c| c.replicas as u32)
                            .or_else(|| {
                                project
                                    .deployment_config
                                    .as_ref()
                                    .map(|c| c.replicas as u32)
                            })
                            .unwrap_or(1),
                    )
                    .port(exposed_port)
                    .configured_port(configured_port)
                    .log_id(deploy_log_id.clone())
                    .log_service(self.log_service.clone())
                    .failed_container_retention(self.db.clone(), promoted_id);

            // Resolve CPU/memory limits + requests (target env → project),
            // matching the normal deploy path so a promotion preserves a
            // configured limit and leaves an unconfigured environment uncapped.
            deploy_builder = deploy_builder.resources(Self::resolve_resource_usage(
                target_env.deployment_config.as_ref(),
                project.deployment_config.as_ref(),
            ));

            // Resolve the TARGET environment's env vars exactly as a normal
            // deploy does, so the promoted container boots with the full set
            // (user vars, external-service connection strings, SENTRY_DSN,
            // TEMPS_API_TOKEN/URL, CRON_SECRET, OTEL_*) instead of nothing.
            // Without this, promotion reuses the image but starts it unconfigured.
            let resolved_env = if let Some(resolver) = self.env_resolver.get() {
                let mut resolved = resolver
                    .resolve(&project, &target_env, &promoted_deployment)
                    .await?;
                crate::services::env_resolver::apply_deployment_owned_variables(
                    &mut resolved,
                    project.preset,
                    &promotion_asset_origin.slug,
                    (project.preset != temps_entities::preset::Preset::DockerCompose)
                        .then_some(exposed_port),
                );
                resolved
            } else {
                tracing::warn!(
                    "Promotion: env resolver not wired — promoted container starts with no resolved env vars"
                );
                std::collections::HashMap::new()
            };
            deploy_builder = deploy_builder.environment_variables(resolved_env.clone());

            deploy_builder = self
                .configure_reuse_deploy_builder(
                    deploy_builder,
                    &project,
                    &target_env,
                    &image_name,
                    &resolved_env,
                )
                .await;

            let provenance = self
                .reuse_image_provenance(
                    &project,
                    &source,
                    promotion_asset_origin.deployment_id,
                    &image_name,
                )
                .await?;
            info!(
                "Promotion: image {} is {}-sourced (origin deployment #{}, expected identity: {})",
                image_name,
                provenance.source,
                promotion_asset_origin.deployment_id,
                provenance.expected_identity_label()
            );

            let deploy_job = provenance.apply(
                deploy_builder
                    .build(self.deployer.clone())
                    .map_err(|e| {
                        DeploymentError::Other(format!("Failed to create deploy job: {}", e))
                    })?
                    .with_external_image_tag(image_name.clone()),
            );

            // Create workflow context for the promoted deployment
            let mock_log_writer = Arc::new(crate::test_utils::MockLogWriter::new(0));
            let mut promote_context = temps_core::WorkflowContext::new(
                format!("promote-{}", promoted_id),
                promoted_id,
                project_id,
                target_environment_id,
                mock_log_writer,
            );

            let cancellation_provider =
                super::workflow_execution_service::DatabaseCancellationProvider::new(
                    self.db.clone(),
                    promoted_id,
                );
            match deploy_job
                .execute_with_cancellation(promote_context.clone(), &cancellation_provider)
                .await
            {
                Ok(job_result) => {
                    info!("Promotion: Deploy job completed successfully");
                    promote_context = job_result.context;

                    let mut active_job: deployment_jobs::ActiveModel = deploy_job_model.into();
                    active_job.status = Set(temps_entities::types::JobStatus::Success);
                    active_job.finished_at = Set(Some(chrono::Utc::now()));
                    let _ = active_job.update(self.db.as_ref()).await;
                }
                Err(e) => {
                    error!("Promotion: Deploy job failed: {}", e);
                    let failure_message = match deploy_job.cleanup(&promote_context).await {
                        Ok(()) => format!("Deploy failed: {e}"),
                        Err(cleanup_error) => format!(
                            "Deploy failed: {e}; promoted container cleanup also failed: {cleanup_error}"
                        ),
                    };

                    let mut active_job: deployment_jobs::ActiveModel = deploy_job_model.into();
                    active_job.status = Set(temps_entities::types::JobStatus::Failure);
                    active_job.finished_at = Set(Some(chrono::Utc::now()));
                    active_job.error_message = Set(Some(failure_message.clone()));
                    let _ = active_job.update(self.db.as_ref()).await;

                    let mut active_complete: deployment_jobs::ActiveModel =
                        complete_job_model.into();
                    active_complete.status = Set(JobStatus::Cancelled);
                    active_complete.error_message = Set(Some("Deploy job failed".to_string()));
                    let _ = active_complete.update(self.db.as_ref()).await;

                    let mut active_dep: deployments::ActiveModel =
                        promoted_deployment.clone().into();
                    active_dep.state = Set("failed".to_string());
                    active_dep.finished_at = Set(Some(chrono::Utc::now()));
                    active_dep.cancelled_reason = Set(Some(failure_message.clone()));
                    let _ = active_dep.update(self.db.as_ref()).await;

                    return Err(DeploymentError::Other(format!(
                        "Failed to deploy image during promotion: {failure_message}"
                    )));
                }
            }

            // Execute MarkDeploymentCompleteJob
            info!("Promotion: Marking deployment {} as complete", promoted_id);

            let mut active_complete: deployment_jobs::ActiveModel = complete_job_model.into();
            active_complete.status = Set(JobStatus::Running);
            active_complete.started_at = Set(Some(chrono::Utc::now()));
            let complete_job_model =
                active_complete
                    .update(self.db.as_ref())
                    .await
                    .map_err(|e| {
                        DeploymentError::Other(format!(
                            "Failed to update complete job status: {}",
                            e
                        ))
                    })?;

            let mark_complete_job = crate::jobs::MarkDeploymentCompleteJobBuilder::new()
                .environment_addresses(self.environment_addresses.get().cloned())
                .job_id("mark_deployment_complete".to_string())
                .deployment_id(promoted_id)
                .db(self.db.clone())
                .log_id(complete_log_id)
                .log_service(self.log_service.clone())
                .container_deployer(self.deployer.clone())
                .queue(self.queue_service.clone())
                .config_service(self.config_service.clone())
                .encryption_service(self.encryption_service.clone())
                .workload_plane(self.workload_plane.get().cloned())
                .build()
                .map_err(|e| {
                    DeploymentError::Other(format!("Failed to create mark complete job: {}", e))
                })?;

            // `execute` consumes the context; keep a copy for the failure
            // path's cleanup below.
            let cleanup_context = promote_context.clone();
            match mark_complete_job.execute(promote_context).await {
                Ok(_) => {
                    info!("Promotion: Mark complete job executed successfully");

                    let mut active_job: deployment_jobs::ActiveModel = complete_job_model.into();
                    active_job.status = Set(temps_entities::types::JobStatus::Success);
                    active_job.finished_at = Set(Some(chrono::Utc::now()));
                    let _ = active_job.update(self.db.as_ref()).await;
                }
                Err(e) => {
                    error!("Promotion: Mark complete job failed: {}", e);

                    // The deploy step already started this deployment's
                    // containers. The workflow executor runs the job's
                    // cleanup on failure; this hand-rolled path must do the
                    // same, or a readiness or route failure leaves them
                    // running with no owner: failed deployments are excluded
                    // from the previous-deployment sweep. `cleanup` leaves a
                    // deployment that already reached completed/deployed
                    // alone, so a late failure never tears down a serving
                    // deployment.
                    let failure_message = clean_up_failed_completion(
                        &mark_complete_job,
                        &cleanup_context,
                        &e,
                        "promotion",
                    )
                    .await;

                    let mut active_job: deployment_jobs::ActiveModel = complete_job_model.into();
                    active_job.status = Set(temps_entities::types::JobStatus::Failure);
                    active_job.finished_at = Set(Some(chrono::Utc::now()));
                    active_job.error_message = Set(Some(failure_message.clone()));
                    let _ = active_job.update(self.db.as_ref()).await;

                    return Err(DeploymentError::Other(format!(
                        "Failed to mark deployment complete during promotion: {failure_message}"
                    )));
                }
            }

            info!(
                "Promotion completed - deployment {} is now active",
                promoted_id
            );
        }
        Ok(())
        }
        .await;
        if let Err(error) = execution_result {
            return Err(self
                .fail_reuse_deployment(promoted_id, "Promotion artifact reuse failed", error)
                .await);
        }

        // Re-fetch the promoted deployment to get the final state
        let final_deployment = deployments::Entity::find_by_id(promoted_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::Other("Promoted deployment disappeared".to_string()))?;

        Ok(self
            .map_db_deployment_to_deployment(final_deployment, true, None)
            .await)
    }

    /// Tears down a specific deployment, removing containers and cleaning up resources
    pub async fn teardown_deployment(
        &self,
        project_id: i32,
        deployment_id: i32,
    ) -> Result<(), DeploymentError> {
        use temps_entities::deployment_containers;

        // Find the deployment
        let deployment = deployments::Entity::find_by_id(deployment_id)
            .filter(deployments::Column::ProjectId.eq(project_id))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::NotFound("Deployment not found".to_string()))?;

        // Stop all containers for this deployment
        let containers = deployment_containers::Entity::find()
            .filter(deployment_containers::Column::DeploymentId.eq(deployment_id))
            .filter(deployment_containers::Column::DeletedAt.is_null())
            .all(self.db.as_ref())
            .await?;

        for container in containers {
            self.deployer
                .stop_container(&container.container_id)
                .await
                .map_err(|e| DeploymentError::Other(format!("Failed to stop container: {}", e)))?;

            // Mark container as deleted
            let mut active_container: deployment_containers::ActiveModel = container.into();
            active_container.deleted_at = Set(Some(chrono::Utc::now()));
            active_container.status = Set(Some("stopped".to_string()));
            active_container.update(self.db.as_ref()).await?;
        }

        // Update deployment state to "stopped"
        let mut active_deployment: deployments::ActiveModel = deployment.into();
        active_deployment.state = Set("stopped".to_string());
        active_deployment.update(self.db.as_ref()).await?;

        Ok(())
    }

    /// Tears down an environment and all its active deployments
    pub async fn teardown_environment(
        &self,
        project_id: i32,
        env_id: i32,
    ) -> Result<(), DeploymentError> {
        use temps_entities::deployment_containers;

        // Find all deployments in this environment
        let deployments = deployments::Entity::find()
            .filter(deployments::Column::ProjectId.eq(project_id))
            .filter(deployments::Column::EnvironmentId.eq(env_id))
            .all(self.db.as_ref())
            .await?;

        // Stop all containers for all deployments
        for deployment in &deployments {
            let containers = deployment_containers::Entity::find()
                .filter(deployment_containers::Column::DeploymentId.eq(deployment.id))
                .filter(deployment_containers::Column::DeletedAt.is_null())
                .all(self.db.as_ref())
                .await?;

            for container in containers {
                // Stop container with timeout - don't fail the whole teardown if one container fails
                match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    self.deployer.stop_container(&container.container_id),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!(
                            "Failed to stop container {} during teardown: {} (continuing)",
                            container.container_id, e
                        );
                    }
                    Err(_) => {
                        warn!(
                            "Timed out stopping container {} after 30s during teardown (continuing)",
                            container.container_id
                        );
                    }
                }

                // Mark container as deleted
                let mut active_container: deployment_containers::ActiveModel = container.into();
                active_container.deleted_at = Set(Some(chrono::Utc::now()));
                active_container.status = Set(Some("stopped".to_string()));
                active_container.update(self.db.as_ref()).await?;
            }
        }

        // Update all deployment states to "stopped"
        for deployment in deployments {
            let mut active_deployment: deployments::ActiveModel = deployment.into();
            active_deployment.state = Set("stopped".to_string());
            active_deployment.update(self.db.as_ref()).await?;
        }

        Ok(())
    }

    pub async fn pause_deployment(
        &self,
        project_id: i32,
        deployment_id: i32,
    ) -> Result<(), DeploymentError> {
        use sea_orm::{ActiveModelTrait, Set};
        use temps_entities::{deployment_containers, deployments, status_incidents};

        // First verify the deployment exists and belongs to the project
        let deployment = deployments::Entity::find_by_id(deployment_id)
            .filter(deployments::Column::ProjectId.eq(project_id))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::NotFound("Deployment not found".to_string()))?;
        check_lifecycle_transition(&deployment, "pause", PAUSABLE_DEPLOYMENT_STATES, "deployed")?;

        let environment_id = deployment.environment_id;

        // Persist "paused" BEFORE touching any container. Monitoring (the
        // container-health poller and the uptime health checker) treats
        // `state == "paused"` as "stopped on purpose, don't alert" — if we
        // stopped containers first and updated this row last, a poll that
        // lands in between would see an exited container against a
        // not-yet-paused deployment and fire a false crash/downtime alert.
        // Flipping the state first closes that window: any concurrent read
        // of this deployment either sees the old state with all containers
        // still running (nothing exited yet to alert on) or sees "paused"
        // once anything might be mid-stop.
        //
        // That alone still leaves a plain check-then-write race against a
        // concurrent outage check in flight: it can read "not paused" a
        // moment before this write commits, then create an incident for a
        // deployment that's paused by the time the incident actually lands.
        // Closing that requires more than moving this write earlier — the
        // check and the write need to serialize. Take the same Postgres
        // advisory lock, keyed on the environment, that
        // `OutageDetectionService::handle_outage_event` takes around its
        // final live pause re-check + incident insert: whichever side gets
        // the lock first commits (or observes "paused" and bails) before the
        // other proceeds, so no unpaused-read can ever precede this write
        // without the write also being visible to it.
        let txn = self.db.begin().await?;
        txn.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT pg_advisory_xact_lock($1)",
            [sea_orm::Value::BigInt(Some(environment_id as i64))],
        ))
        .await?;
        let mut active_deployment: deployments::ActiveModel = deployment.into();
        active_deployment.state = Set("paused".to_string());
        active_deployment.update(&txn).await?;

        // The lock above only serializes the incident *insert* against this
        // write — `OutageDetectionService` still sends the notification,
        // fires the alarm, and dispatches the workflow AFTER releasing the
        // lock, since those are external I/O (webhook/email delivery, job
        // queue send) that can't reasonably sit inside a DB transaction. A
        // pause landing in that specific gap would otherwise still produce
        // an alert for an incident that's already stale by the time it goes
        // out. Rather than trying to also lock out external I/O (which a DB
        // lock structurally can't do), make this side of the race
        // proactive: while still holding the lock, resolve any incident for
        // this environment that's still open. `handle_outage_event`
        // re-reads the incident's own status immediately before each side
        // effect (see its comment), so a resolve that lands here — even a
        // moment after that incident was created — is what that re-read is
        // watching for.
        status_incidents::Entity::update_many()
            .col_expr(
                status_incidents::Column::Status,
                sea_orm::sea_query::Expr::value("resolved"),
            )
            .col_expr(
                status_incidents::Column::ResolvedAt,
                sea_orm::sea_query::Expr::value(chrono::Utc::now()),
            )
            .filter(status_incidents::Column::EnvironmentId.eq(environment_id))
            .filter(status_incidents::Column::Status.ne("resolved"))
            .exec(&txn)
            .await?;

        txn.commit().await?;

        // Stop and remove all containers for this deployment
        let containers = deployment_containers::Entity::find()
            .filter(deployment_containers::Column::DeploymentId.eq(deployment_id))
            .filter(deployment_containers::Column::DeletedAt.is_null())
            .all(self.db.as_ref())
            .await?;

        // Stop (but do not remove) each container. Keeping the Docker
        // container object around — rather than force-removing it, as this
        // used to do — is what makes `resume_deployment` able to bring the
        // exact same containers back with a plain `docker start` instead of
        // trying to "unpause" containers that no longer exist (see the fix
        // note on `resume_deployment` below). The route table additionally
        // stops sending live traffic to any container whose `status` isn't
        // "running" (see `route_table::load_routes`), so a stopped-but-not-
        // removed container is just as inert from the outside as a removed
        // one, without sacrificing resumability.
        // Best-effort like the `stop_container` call above: the deployment
        // row is already committed as "paused", so aborting this loop on a
        // single container's DB write failure would strand the *remaining*
        // containers untouched (still "running" in the DB, never even
        // asked to stop) and skip the route-table reload below, while the
        // deployment stays paused indefinitely. Warn and keep going so one
        // failure can't silently drop the rest of the pause.
        for container in containers {
            let container_id = container.container_id.clone();
            if let Err(e) = self.deployer.stop_container(&container_id).await {
                warn!(
                    "Failed to stop container {} during deployment pause: {}",
                    container_id, e
                );
            }

            // Retry the DB write: a container whose status never makes it to
            // "stopped" keeps being treated as routable by
            // `route_table::load_routes` even though we just told Docker to
            // stop it — retrying absorbs the transient connection blips that
            // are the realistic cause of a single UPDATE failing right after
            // the read and the deployment-state write above it both
            // succeeded, so routes don't go stale on something recoverable.
            let retry = temps_core::retry::RetryConfig::new(3)
                .with_base_delay(std::time::Duration::from_millis(100))
                .with_max_delay(std::time::Duration::from_secs(2));
            let stopped_at = chrono::Utc::now();
            let update_result = retry
                .retry(|| async {
                    let active_container = deployment_containers::ActiveModel {
                        status: Set(Some("stopped".to_string())),
                        finished_at: Set(Some(stopped_at)),
                        ..deployment_containers::ActiveModel::from(container.clone())
                    };
                    active_container.update(self.db.as_ref()).await
                })
                .await;
            if let Err(e) = update_result {
                warn!(
                    "Failed to persist stopped status for container {} during deployment pause \
                     after retrying: {} — the route table may still treat it as routable until \
                     the next successful status update",
                    container_id, e
                );
            }
        }

        // Force an in-process route-table reload (same mechanism
        // `mark_deployment_complete.rs` uses after a normal deploy — see its
        // comment for why this is needed in addition to PG NOTIFY). Nothing
        // else about a pause touches `environments` or `projects`, which are
        // the only tables with a NOTIFY trigger wired up (see
        // `m20251209_000001_add_environments_route_trigger.rs` /
        // `m20250205_000003_add_projects_route_trigger.rs`) — a bare
        // `deployment_containers` status UPDATE fires no trigger at all. So
        // without this, the proxy's cached peer table keeps the container's
        // OLD (still "valid-looking") address indefinitely and only
        // discovers the pause when the next unrelated route change happens
        // to reload it, in the meantime returning "upstream connection
        // refused" instead of the intended "not currently serving" state.
        if let Err(e) = self
            .queue_service
            .send(temps_core::Job::ForceRouteReload(
                temps_core::ForceRouteReloadJob {
                    environment_id: Some(environment_id),
                    deployment_id: Some(deployment_id),
                },
            ))
            .await
        {
            warn!(
                "Failed to publish in-process ForceRouteReload after pausing deployment {}: {} \
                 — falling back to the next PG NOTIFY-triggered reload",
                deployment_id, e
            );
        }

        info!(
            "Successfully paused deployment {}: stopped all containers",
            deployment_id
        );
        Ok(())
    }

    pub async fn resume_deployment(
        &self,
        project_id: i32,
        deployment_id: i32,
    ) -> Result<(), DeploymentError> {
        use temps_entities::deployment_containers;

        // First verify the deployment exists and belongs to the project
        let deployment = deployments::Entity::find()
            .filter(deployments::Column::Id.eq(deployment_id))
            .filter(deployments::Column::ProjectId.eq(project_id))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::NotFound("Deployment not found".to_string()))?;
        check_lifecycle_transition(&deployment, "resume", RESUMABLE_DEPLOYMENT_STATES, "paused")?;

        // Resume all containers for this deployment
        let containers = deployment_containers::Entity::find()
            .filter(deployment_containers::Column::DeploymentId.eq(deployment_id))
            .filter(deployment_containers::Column::DeletedAt.is_null())
            .all(self.db.as_ref())
            .await?;

        // `pause_deployment` stops (not removes) containers, so bring them
        // back with a plain `docker start` on the same container id/name —
        // not `resume_container` (Docker's `unpause`/cgroup-freeze reverse).
        // `unpause` only undoes a genuine `docker pause`, which nothing in
        // this codebase's real pause path ever calls; using it here against
        // a merely-stopped container always failed ("container is not
        // paused"), so resume could never actually succeed after a real
        // pause.
        for container in containers {
            self.deployer
                .start_container(&container.container_id)
                .await
                .map_err(|e| {
                    DeploymentError::Other(format!("Failed to resume container: {}", e))
                })?;

            // Update container status
            let mut active_container: deployment_containers::ActiveModel = container.into();
            active_container.status = Set(Some("running".to_string()));
            active_container.update(self.db.as_ref()).await?;
        }

        let environment_id = deployment.environment_id;

        // Update deployment state to "deployed"
        let mut active_deployment: deployments::ActiveModel = deployment.into();
        active_deployment.state = Set("deployed".to_string());
        active_deployment.update(self.db.as_ref()).await?;

        // See the matching comment in `pause_deployment`: a container-status
        // UPDATE fires no DB trigger, so force an in-process reload rather
        // than leaving the proxy's cached peer table to notice the resume
        // only whenever some unrelated route change happens to trigger one.
        if let Err(e) = self
            .queue_service
            .send(temps_core::Job::ForceRouteReload(
                temps_core::ForceRouteReloadJob {
                    environment_id: Some(environment_id),
                    deployment_id: Some(deployment_id),
                },
            ))
            .await
        {
            warn!(
                "Failed to publish in-process ForceRouteReload after resuming deployment {}: {} \
                 — falling back to the next PG NOTIFY-triggered reload",
                deployment_id, e
            );
        }

        info!("Successfully resumed deployment: {}", deployment_id);
        Ok(())
    }

    /// Cancel all running deployments with a given reason
    /// This is typically called during server shutdown or startup
    pub async fn cancel_running_deployments(
        &self,
        cancelled_reason: &str,
    ) -> Result<u64, DeploymentError> {
        use sea_orm::sea_query::Expr;
        use temps_entities::deployments;

        debug!(
            "Cancelling all running deployments with reason: {}",
            cancelled_reason
        );

        // Update all running deployments to cancelled status in a single query
        let result = deployments::Entity::update_many()
            .filter(deployments::Column::State.eq("running"))
            .col_expr(deployments::Column::State, Expr::value("cancelled"))
            .col_expr(
                deployments::Column::CancelledReason,
                Expr::value(cancelled_reason),
            )
            .col_expr(
                deployments::Column::FinishedAt,
                Expr::current_timestamp().into(),
            )
            .col_expr(
                deployments::Column::UpdatedAt,
                Expr::current_timestamp().into(),
            )
            .exec(self.db.as_ref())
            .await
            .map_err(|e| DeploymentError::DatabaseError {
                reason: e.to_string(),
            })?;

        let count = result.rows_affected;

        if count > 0 {
            info!("Successfully cancelled {} running deployment(s)", count);
        } else {
            debug!("No running deployments found");
        }

        Ok(count)
    }

    /// Cancel all active deployments for an environment before its containers
    /// are removed by `DeploymentContainerCleaner`.
    pub async fn cancel_all_environment_deployments(
        &self,
        environment_id: i32,
    ) -> Result<u64, DeploymentError> {
        self.cancel_deployments_for_deletion(None, Some(environment_id), "Environment deleted")
            .await
    }

    pub async fn cancel_all_project_deployments(
        &self,
        project_id: i32,
    ) -> Result<u64, DeploymentError> {
        self.cancel_deployments_for_deletion(Some(project_id), None, "Project deleted")
            .await
    }

    async fn cancel_deployments_for_deletion(
        &self,
        project_id: Option<i32>,
        environment_id: Option<i32>,
        reason: &str,
    ) -> Result<u64, DeploymentError> {
        use temps_entities::{deployment_jobs, types::JobStatus};

        let mut query = deployments::Entity::find().filter(
            Condition::all()
                .add(deployments::Column::State.ne("cancelled"))
                .add(deployments::Column::State.ne("completed"))
                .add(deployments::Column::State.ne("deployed"))
                .add(deployments::Column::State.ne("failed"))
                .add(deployments::Column::State.ne("paused"))
                .add(deployments::Column::State.ne("stopped")),
        );
        if let Some(project_id) = project_id {
            query = query.filter(deployments::Column::ProjectId.eq(project_id));
        }
        if let Some(environment_id) = environment_id {
            query = query.filter(deployments::Column::EnvironmentId.eq(environment_id));
        }

        let active_deployments = query.all(self.db.as_ref()).await?;
        let count = active_deployments.len() as u64;

        for deployment in active_deployments {
            let running_jobs = deployment_jobs::Entity::find()
                .filter(deployment_jobs::Column::DeploymentId.eq(deployment.id))
                .filter(deployment_jobs::Column::Status.eq(JobStatus::Running))
                .all(self.db.as_ref())
                .await?;

            for job in running_jobs {
                let message = format!(
                    "DEPLOYMENT CANCELLED: {reason} - Job '{}' is being terminated",
                    job.name
                );
                if let Err(error) = self
                    .log_service
                    .append_structured_log(
                        &job.log_id,
                        temps_deployment_logs::LogLevel::Error,
                        &message,
                    )
                    .await
                {
                    warn!(
                        deployment_id = deployment.id,
                        job_log_id = %job.log_id,
                        %error,
                        "Failed to append deletion cancellation to deployment job log"
                    );
                }
            }

            let deployment_id = deployment.id;
            let mut active_deployment: deployments::ActiveModel = deployment.into();
            active_deployment.state = Set("cancelled".to_string());
            active_deployment.cancelled_reason = Set(Some(reason.to_string()));
            active_deployment.finished_at = Set(Some(chrono::Utc::now()));
            active_deployment.updated_at = Set(chrono::Utc::now());
            active_deployment
                .update(self.db.as_ref())
                .await
                .map_err(|error| DeploymentError::DatabaseError {
                    reason: format!(
                        "Failed to cancel deployment {deployment_id} before owner deletion: {error}"
                    ),
                })?;
        }

        info!(
            ?project_id,
            ?environment_id,
            count,
            "Cancelled deployments before owner deletion"
        );
        Ok(count)
    }

    /// Remove every uploaded archive recorded for a project before deletion.
    /// Runtime containers are handled by `DeploymentContainerCleaner`.
    pub async fn cleanup_project_archives(&self, project_id: i32) -> Result<u64, DeploymentError> {
        let data_dir = self.config_service.data_dir();
        let source_archives = temps_entities::source_bundles::Entity::find()
            .filter(temps_entities::source_bundles::Column::ProjectId.eq(project_id))
            .all(self.db.as_ref())
            .await?;
        let static_archives = temps_entities::static_bundles::Entity::find()
            .filter(temps_entities::static_bundles::Column::ProjectId.eq(project_id))
            .all(self.db.as_ref())
            .await?;
        let archive_paths: Vec<String> = source_archives
            .into_iter()
            .map(|bundle| bundle.archive_path)
            .chain(static_archives.into_iter().map(|bundle| bundle.blob_path))
            .collect();
        let removed = archive_paths.len() as u64;
        for relative_path in archive_paths {
            let archive_path =
                confined_archive_path(&data_dir, &relative_path).map_err(|error| {
                    DeploymentError::InvalidBundlePath {
                        path: relative_path.clone(),
                        reason: format!("stored path for project {project_id} is invalid: {error}"),
                    }
                })?;
            match tokio::fs::remove_file(&archive_path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(DeploymentError::Other(format!(
                        "Failed to remove archive '{}' for project {}: {}",
                        archive_path.display(),
                        project_id,
                        error
                    )));
                }
            }
        }

        info!(
            "Removed {} uploaded archive(s) before deleting project {}",
            removed, project_id
        );
        Ok(removed)
    }

    /// Cancel a specific deployment
    pub async fn cancel_deployment(
        &self,
        project_id: i32,
        deployment_id: i32,
    ) -> Result<(), DeploymentError> {
        use temps_entities::{deployment_jobs, types::JobStatus};

        info!(
            "Cancelling deployment {} for project {}",
            deployment_id, project_id
        );

        // Verify the deployment exists and belongs to the project
        let deployment = deployments::Entity::find_by_id(deployment_id)
            .filter(deployments::Column::ProjectId.eq(project_id))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| DeploymentError::NotFound("Deployment not found".to_string()))?;

        info!(
            "Deployment {} current state: '{}' - checking if cancellable",
            deployment_id, deployment.state
        );

        // Only allow cancelling deployments in pending or running state
        if deployment.state != "pending" && deployment.state != "running" {
            info!(
                "Cannot cancel deployment {} - already in '{}' state",
                deployment_id, deployment.state
            );
            return Err(DeploymentError::InvalidInput(format!(
                "Cannot cancel deployment in '{}' state. Only 'pending' or 'running' deployments can be cancelled.",
                deployment.state
            )));
        }

        // Find currently running job and write cancellation message to its logs
        let running_jobs = deployment_jobs::Entity::find()
            .filter(deployment_jobs::Column::DeploymentId.eq(deployment_id))
            .filter(deployment_jobs::Column::Status.eq(JobStatus::Running))
            .all(self.db.as_ref())
            .await?;

        for job in running_jobs {
            info!(
                "📝 Writing cancellation message to running job: {} ({})",
                job.name, job.log_id
            );

            // Write cancellation message to the job's log
            let cancel_msg = format!(
                "DEPLOYMENT CANCELLED BY USER - Job '{}' is being terminated",
                job.name
            );
            if let Err(e) = self
                .log_service
                .append_structured_log(
                    &job.log_id,
                    temps_deployment_logs::LogLevel::Error,
                    &cancel_msg,
                )
                .await
            {
                warn!(
                    "Failed to write cancellation message to job log {}: {}",
                    job.log_id, e
                );
            }
        }

        // Snapshot fields we'll need *after* the move into ActiveModel for
        // the queue event below — the active model takes ownership of the row.
        let environment_id = deployment.environment_id;

        // Update deployment to cancelled state
        let mut active_deployment: deployments::ActiveModel = deployment.into();
        active_deployment.state = Set("cancelled".to_string());
        active_deployment.cancelled_reason = Set(Some("Cancelled by user".to_string()));
        active_deployment.finished_at = Set(Some(chrono::Utc::now()));
        active_deployment.updated_at = Set(chrono::Utc::now());
        active_deployment.update(self.db.as_ref()).await?;

        // Publish a DeploymentCancelled event so downstream listeners (PR
        // commenter, notifications, audit consumers) can react. The workflow
        // executor publishes the same event when it transitions to Cancelled
        // mid-pipeline; this site covers user-initiated cancels from the UI /
        // API, which previously left the PR comment stuck on "Deploying preview".
        //
        // Best-effort: a queue failure here must NOT undo the cancellation —
        // log and move on, mirroring how DeploymentFailed/Succeeded handle it
        // elsewhere in this file.
        let environment_name =
            match temps_entities::environments::Entity::find_by_id(environment_id)
                .one(self.db.as_ref())
                .await
            {
                Ok(Some(env)) => env.name,
                _ => String::new(),
            };
        let event = temps_core::Job::DeploymentCancelled(temps_core::DeploymentCancelledJob {
            deployment_id,
            project_id,
            environment_id,
            environment_name,
        });
        if let Err(e) = self.queue_service.send(event).await {
            warn!(
                "Failed to send DeploymentCancelled event for deployment {}: {}",
                deployment_id, e
            );
        }

        // Anonymous telemetry: this is the explicit "user clicked Cancel"
        // path (the other emission site, in WorkflowExecutionService, only
        // fires as a fallback when the executor detects a "cancelled" error
        // without this method having already set the state — see that
        // file's comment). Deliberately NOT emitted from the supersede
        // (cancel_in_flight_deployments) or bulk-shutdown
        // (cancel_running_deployments) paths, since those fire automatically
        // on every push / restart and would swamp the funnel signal with
        // non-user-initiated noise.
        let template_provenance = projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await
            .ok()
            .flatten()
            .and_then(|project| project.template_slug);
        self.telemetry().report(
            temps_core::telemetry::TelemetryEvent::new(
                temps_core::telemetry::TelemetryEventKind::DeployCancelled,
            )
            .with("trigger", "user")
            .with_template_provenance(template_provenance.as_deref()),
        );

        info!(
            "Successfully cancelled deployment {} for project {} - workflow will stop at next checkpoint",
            deployment_id, project_id
        );

        Ok(())
    }
}

// Implement DeploymentCanceller trait from temps-core
#[async_trait::async_trait]
impl temps_core::DeploymentCanceller for DeploymentService {
    async fn cancel_all_project_deployments(
        &self,
        project_id: i32,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        self.cancel_all_project_deployments(project_id)
            .await
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn cancel_all_environment_deployments(
        &self,
        environment_id: i32,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        self.cancel_all_environment_deployments(environment_id)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

#[async_trait::async_trait]
impl temps_core::ProjectArchiveCleaner for DeploymentService {
    async fn cleanup_project_archives(
        &self,
        project_id: i32,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        self.cleanup_project_archives(project_id)
            .await
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
    }
}

#[cfg(test)]
mod failed_completion_cleanup_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use temps_core::{JobResult, WorkflowContext, WorkflowError};

    struct NoopLogWriter;

    #[async_trait::async_trait]
    impl temps_core::LogWriter for NoopLogWriter {
        async fn write_log(&self, _message: String) -> Result<(), WorkflowError> {
            Ok(())
        }

        fn stage_id(&self) -> i32 {
            1
        }
    }

    /// A job whose cleanup counts its calls and optionally fails.
    #[derive(Debug)]
    struct CleanupProbe {
        cleanups: AtomicUsize,
        cleanup_error: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl WorkflowTask for CleanupProbe {
        fn job_id(&self) -> &str {
            "mark_deployment_complete"
        }

        fn name(&self) -> &str {
            "probe"
        }

        fn description(&self) -> &str {
            "records cleanup calls"
        }

        async fn execute(&self, _context: WorkflowContext) -> Result<JobResult, WorkflowError> {
            Err(WorkflowError::JobExecutionFailed("not used".to_string()))
        }

        async fn cleanup(&self, _context: &WorkflowContext) -> Result<(), WorkflowError> {
            self.cleanups.fetch_add(1, Ordering::SeqCst);
            match self.cleanup_error {
                Some(reason) => Err(WorkflowError::JobExecutionFailed(reason.to_string())),
                None => Ok(()),
            }
        }
    }

    fn context() -> WorkflowContext {
        WorkflowContext::new("run-1".to_string(), 31, 1, 2, Arc::new(NoopLogWriter))
    }

    #[tokio::test]
    async fn failed_completion_runs_the_jobs_cleanup() {
        let probe = CleanupProbe {
            cleanups: AtomicUsize::new(0),
            cleanup_error: None,
        };
        let error = WorkflowError::JobExecutionFailed("route never became ready".to_string());

        let message = clean_up_failed_completion(&probe, &context(), &error, "promotion").await;

        assert_eq!(probe.cleanups.load(Ordering::SeqCst), 1);
        assert!(message.contains("route never became ready"), "{message}");
        assert!(!message.contains("cleanup also failed"), "{message}");
    }

    #[tokio::test]
    async fn failed_cleanup_is_reported_alongside_the_original_error() {
        let probe = CleanupProbe {
            cleanups: AtomicUsize::new(0),
            cleanup_error: Some("worker node 4 unreachable"),
        };
        let error = WorkflowError::JobExecutionFailed("HTTP 503".to_string());

        let message = clean_up_failed_completion(&probe, &context(), &error, "rollback").await;

        assert_eq!(probe.cleanups.load(Ordering::SeqCst), 1);
        assert!(message.contains("HTTP 503"), "{message}");
        assert!(
            message.contains("rollback container cleanup also failed"),
            "{message}"
        );
        assert!(message.contains("worker node 4 unreachable"), "{message}");
    }
}

#[cfg(test)]
mod lifecycle_transition_tests {
    use super::*;

    fn deployment_in(state: &str) -> deployments::Model {
        deployments::Model {
            id: 17,
            project_id: 1,
            environment_id: 1,
            slug: "deploy-17".to_string(),
            state: state.to_string(),
            metadata: None,
            deploying_at: None,
            ready_at: None,
            started_at: None,
            finished_at: None,
            context_vars: None,
            branch_ref: Some("main".to_string()),
            tag_ref: None,
            commit_sha: None,
            commit_message: None,
            commit_author: None,
            commit_json: None,
            cancelled_reason: None,
            static_dir_location: None,
            screenshot_location: None,
            image_name: None,
            deployment_config: None,
            promoted_from_deployment_id: None,
            upload_request_id: None,
            docker_socket_mounted: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn only_a_paused_deployment_can_be_resumed() {
        let resume = |state| {
            check_lifecycle_transition(
                &deployment_in(state),
                "resume",
                RESUMABLE_DEPLOYMENT_STATES,
                "paused",
            )
        };
        assert!(resume("paused").is_ok());
        for state in [
            "stopped",
            "failed",
            "cancelled",
            "pending",
            "running",
            "deployed",
        ] {
            match resume(state) {
                Err(DeploymentError::DeploymentStateConflict {
                    deployment_id: 17,
                    operation: "resume",
                    state: rejected,
                    ..
                }) => assert_eq!(rejected, state),
                other => panic!("resuming a '{state}' deployment must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn only_a_live_deployment_can_be_paused() {
        let pause = |state| {
            check_lifecycle_transition(
                &deployment_in(state),
                "pause",
                PAUSABLE_DEPLOYMENT_STATES,
                "deployed",
            )
        };
        assert!(pause("deployed").is_ok());
        assert!(pause("completed").is_ok());
        for state in [
            "paused",
            "stopped",
            "failed",
            "cancelled",
            "pending",
            "running",
        ] {
            assert!(
                matches!(
                    pause(state),
                    Err(DeploymentError::DeploymentStateConflict {
                        operation: "pause",
                        ..
                    })
                ),
                "pausing a '{state}' deployment must be refused"
            );
        }
    }

    #[test]
    fn conflict_message_names_the_deployment_and_its_state() {
        let message = check_lifecycle_transition(
            &deployment_in("stopped"),
            "resume",
            RESUMABLE_DEPLOYMENT_STATES,
            "paused",
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("deployment 17"), "{message}");
        assert!(message.contains("'stopped'"), "{message}");
        assert!(message.contains("paused"), "{message}");
    }
}

#[cfg(test)]
mod tests;
