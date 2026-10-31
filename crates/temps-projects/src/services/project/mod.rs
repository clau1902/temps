// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ProjectService`]: project CRUD, settings, Git wiring and deployment
//! triggers.
//!
//! The service is defined here; its behaviour is split by concern:
//!
//! - `naming`: display-name validation, slug generation and renames
//! - `guards`: reserved-slug claims, Docker-socket grants and tenant ceilings
//! - `queries`: project reads, search, pagination and statistics
//! - `source_directory`: project root-directory validation
//! - `presets`: preset resolution, merging and validation
//! - `templates`: service-template provenance, runtime and upgrades
//! - `hosting`: source type and hosting toggles
//! - `deletion`: project deletion and its delivery-binding guard
//! - `delivery`: CDN delivery provider selection
//! - `webhooks`: provider webhook install/remove (GitLab, Bitbucket, Gitea,
//!   GitHub, generic)

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use temps_core::docker_socket_grant::DockerSocketCapability;
use temps_core::url_validation::{redact_url_password, validate_git_url};
use tracing::{error, info, warn};

use sea_orm::{
    prelude::Uuid, sea_query::LockType, ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait,
    DatabaseTransaction, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
    RelationTrait, Set, Statement, TransactionTrait,
};
use temps_core::{
    ForceRouteReloadJob, Job, ProjectCreatedJob, ProjectDeletedJob, ProjectUpdatedJob,
};
use temps_entities::{
    delivery_profiles, deployments, dns_providers, domain_delivery_bindings, env_var_environments,
    env_vars, environments, external_services, git_provider_connections, git_providers,
    project_delivery_settings, project_group_members, project_services, projects, settings,
    types::ProjectType,
};
use temps_git::services::public_repo::{
    PublicRefKind, PublicRepoError, PublicRepoProviderFactory, ResolvedPublicRef,
};

use serde::Serialize;

use super::types::{
    CreateProjectEnvVar, CreateProjectRequest, Project, ProjectError, ProjectRename,
    ProjectSettingsUpdate, ProjectStatistics, ReservedSlugChange, SlugClaimAuthority,
    SlugClaimCaller, UpdateDeploymentSettingsRequest, UpdateProjectSettingsParams,
};
use super::{EnvVarService, EnvVarWithEnvironments};
use crate::handlers::{UpdateDeploymentConfigRequest, UpdateServiceTemplateRuntimeRequest};
use temps_core::docker_socket_grant::DeployCaller;

mod deletion;
mod delivery;
mod guards;
mod hosting;
mod naming;
mod presets;
mod queries;
mod source_directory;
mod templates;
mod webhooks;

use super::{data_purge, types};
use delivery::{chosen_delivery_provider, delivery_provider_label};
use naming::{slugify, validate_project_name};
use presets::{
    apply_resolved_preset, compose_public_ports, merge_preset_config,
    project_type_for_resolved_preset, resolve_preset_patch, resolve_preset_selection,
    validate_preset_config,
};
use source_directory::{
    normalize_project_directory, normalized_stored_directory, pull_only_root_directory_value,
    SourceDirectoryTarget,
};
pub use templates::{TemplateGithubResponse, TemplateResponse};
use webhooks::repository_target_changed;

// Placeholder functions - these should be implemented properly or imported from other services

/// Optional group membership filter for [`ProjectService::get_projects_paginated_excluding_search`].
#[derive(Debug, Clone, Copy)]
pub enum GroupFilter {
    /// Return only services that are **not** members of any project group.
    Ungrouped,
    /// Return only services that are members of the given project group.
    InGroup(i32),
}

/// Whether changing `repo_owner`/`repo_name` would leave `git_url` pointing at
/// a different repository.
///
/// Returns `Some((old, new))` — both as `owner/name` — only when the stored URL
/// demonstrably identifies the *current* repo and the requested change moves
/// away from it. A URL that doesn't carry a recognisable `owner/name` tail
/// (self-hosted layouts, ssh remotes with unusual paths) returns `None`: we
/// can't prove a desync, so we don't block the operator.
fn would_desync_git_url(
    git_url: &Option<String>,
    current: (&str, &str),
    requested: (Option<&str>, Option<&str>),
) -> Option<(String, String)> {
    let (new_owner, new_name) = (
        requested.0.unwrap_or(current.0),
        requested.1.unwrap_or(current.1),
    );
    let old_pair = format!("{}/{}", current.0, current.1);
    let new_pair = format!("{}/{}", new_owner, new_name);
    if old_pair == new_pair {
        return None;
    }

    let url = git_url.as_deref()?;
    // Compare on the `owner/name` tail, ignoring a `.git` suffix and any
    // trailing slash, so https/ssh and with/without `.git` all match.
    let tail = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit(['/', ':'])
        .take(2)
        .collect::<Vec<_>>();
    if tail.len() < 2 {
        return None;
    }
    let url_pair = format!("{}/{}", tail[1], tail[0]);

    (url_pair.eq_ignore_ascii_case(&old_pair)).then_some((url_pair, new_pair))
}

// Add this new struct to represent an environment variable with its environments
#[derive(Debug, Serialize)]
pub struct EnvVarEnvironment {
    pub id: i32,
    pub name: String,
}

// Constants for default hosted website resource profiles. CPU values are stored
// as microcores (1_000_000 = 1 CPU core); memory values are stored as MB. These
// profiles are intentionally separate from the external database-service
// profiles because app containers are usually burstier and safer to cap by
// default on small single-node installs.
pub const DEFAULT_CPU_REQUEST: i32 = 500_000; // 0.5 cores
pub const DEFAULT_MEMORY_REQUEST: i32 = 128; // 128 MB
pub const DEFAULT_MEMORY_LIMIT: i32 = 512; // 512 MB (small hosted website profile)

// Add these constants at the top of the file proper key management
pub const NONCE_LENGTH: usize = 12;

/// Resolve an API/UI catalog slug to its canonical persisted preset and config.
fn resolve_preset_slug(
    slug: &str,
    config: Option<temps_entities::preset::PresetConfig>,
) -> Result<temps_presets::StoredPreset, ProjectError> {
    temps_presets::resolve_preset_slug(slug, config)
        .map_err(|error| ProjectError::InvalidInput(format!("Invalid preset: {}", error)))
}

#[derive(Clone)]
pub struct ProjectService {
    pub db: Arc<temps_database::DbConnection>,
    pub queue_service: Arc<dyn temps_core::JobQueue>,
    pub config_service: Arc<temps_config::ConfigService>,
    pub external_service_manager: Arc<temps_providers::ExternalServiceManager>,
    pub git_provider_manager: Arc<temps_git::GitProviderManager>,
    env_var_service: Arc<EnvVarService>,
    environment_service: Arc<temps_environments::EnvironmentService>,
    encryption_service: Arc<temps_core::EncryptionService>,
    /// This control plane's own host Docker socket grant (ADR 045).
    ///
    /// Snapshotted from the process-wide grant at construction rather than
    /// read per call: the whole point of the `OnceLock` is that a later
    /// `set_var` cannot change who is root-equivalent, and a field also lets
    /// tests exercise the claim rule against an explicit grant without
    /// mutating process-global environment state.
    docker_socket_grant: temps_core::docker_socket_grant::DockerSocketGrant,
    /// ADR 068 D5: the reachability warning on the project response
    /// (late-bound, optional: empty without the ingress plugin).
    environment_addresses:
        std::sync::OnceLock<Arc<temps_core::environment_address::EnvironmentAddressSlot>>,
}

fn initial_deployment_config(
    request: &CreateProjectRequest,
) -> temps_entities::deployment_config::DeploymentConfig {
    temps_entities::deployment_config::DeploymentConfig {
        cpu_request: request.cpu_request.or(Some(DEFAULT_CPU_REQUEST)),
        cpu_limit: request.cpu_limit,
        memory_request: request.memory_request.or(Some(DEFAULT_MEMORY_REQUEST)),
        memory_limit: request.memory_limit.or(Some(DEFAULT_MEMORY_LIMIT)),
        exposed_port: request.exposed_port,
        automatic_deploy: Some(request.automatic_deploy),
        ..Default::default()
    }
}

/// Why a manual trigger could not resolve its ref in a public repository.
enum PublicTriggerRefError {
    /// The repository has no such branch, tag or commit; carries the
    /// user-facing explanation.
    Missing(String),
    /// The provider could not answer (rate limit, outage, not supported).
    Provider(PublicRepoError),
}

/// Resolve what a manual trigger deploys in a public repository to the commit
/// it points at.
///
/// An explicit `tag` is looked up as a tag only, so a branch of the same name
/// can never answer for it, and the deployment gets that tag's commit rather
/// than the branch tip. Without a tag, `reference` (the branch field) is
/// resolved as a branch, then a tag, then a full or abbreviated commit id,
/// because group repositories and templates pin a release tag or a commit in
/// the same field.
async fn resolve_public_trigger_ref(
    provider: &dyn temps_git::services::public_repo::PublicRepoProvider,
    owner: &str,
    repo: &str,
    reference: &str,
    tag: Option<&str>,
) -> Result<ResolvedPublicRef, PublicTriggerRefError> {
    let (lookup, missing) = match tag {
        Some(tag) => (
            format!("refs/tags/{tag}"),
            format!("Tag '{tag}' not found in public repo {owner}/{repo}"),
        ),
        None => (
            reference.to_string(),
            format!("'{reference}' is not a branch, tag or commit of public repo {owner}/{repo}"),
        ),
    };
    provider
        .resolve_ref(owner, repo, &lookup)
        .await
        .map_err(PublicTriggerRefError::Provider)?
        .ok_or(PublicTriggerRefError::Missing(missing))
}

impl ProjectService {
    pub async fn compose_security_policy(
        &self,
        project_id: i32,
    ) -> Result<
        temps_entities::compose_security::ComposeSecurityPolicy,
        super::compose_security::ComposeSecurityError,
    > {
        super::compose_security::read(self.db.as_ref(), project_id).await
    }

    pub async fn compose_security_legacy_migration_pending(
        &self,
        project_id: i32,
    ) -> Result<bool, super::compose_security::ComposeSecurityError> {
        super::compose_security::legacy_migration_pending(self.db.as_ref(), project_id).await
    }

    pub async fn update_compose_security_policy(
        &self,
        project_id: i32,
        actor: i32,
        policy: temps_entities::compose_security::ComposeSecurityPolicy,
        acknowledged: bool,
        expected_policy: temps_entities::compose_security::ComposeSecurityPolicy,
        acknowledge_legacy_migration: bool,
    ) -> Result<
        temps_entities::compose_security::ComposeSecurityPolicy,
        super::compose_security::ComposeSecurityError,
    > {
        super::compose_security::update(
            self.db.as_ref(),
            project_id,
            actor,
            policy,
            acknowledged,
            expected_policy,
            acknowledge_legacy_migration,
        )
        .await
    }

    pub fn new(
        db: Arc<temps_database::DbConnection>,
        queue_service: Arc<dyn temps_core::JobQueue>,
        config_service: Arc<temps_config::ConfigService>,
        external_service_manager: Arc<temps_providers::ExternalServiceManager>,
        git_provider_manager: Arc<temps_git::GitProviderManager>,
        environment_service: Arc<temps_environments::EnvironmentService>,
        encryption_service: Arc<temps_core::EncryptionService>,
    ) -> Self {
        let env_var_service = Arc::new(EnvVarService::new(db.clone(), encryption_service.clone()));

        ProjectService {
            db: db.clone(),
            queue_service,
            config_service: config_service.clone(),
            external_service_manager,
            git_provider_manager,
            env_var_service,
            environment_service,
            encryption_service,
            docker_socket_grant: temps_core::docker_socket_grant::process_grant().clone(),
            environment_addresses: std::sync::OnceLock::new(),
        }
    }

    /// Wire the ADR 068 D5 reachability warning.
    pub fn set_environment_addresses(
        &self,
        slot: Arc<temps_core::environment_address::EnvironmentAddressSlot>,
    ) {
        let _ = self.environment_addresses.set(slot);
    }

    /// ADR 068 D5: the warning for the project's production environment
    /// (the one the project header shows), or `None`. Never an error: a
    /// reachability failure is logged and reads as no warning.
    pub async fn reachability_warning(
        &self,
        project_id: i32,
    ) -> Option<temps_core::environment_address::ReachabilityWarning> {
        use temps_entities::environments;
        let addresses = self.environment_addresses.get()?.get()?;
        let candidates = environments::Entity::find()
            .filter(environments::Column::ProjectId.eq(project_id))
            .filter(environments::Column::DeletedAt.is_null())
            .filter(environments::Column::IsPreview.eq(false))
            .filter(environments::Column::CurrentDeploymentId.is_not_null())
            .order_by_asc(environments::Column::Id)
            .all(self.db.as_ref())
            .await;
        let candidates = match candidates {
            Ok(rows) => rows,
            Err(error) => {
                warn!(project_id, %error, "could not read environments for the reachability warning");
                return None;
            }
        };
        let environment = candidates
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case("production"))
            .or_else(|| candidates.first())?;
        match addresses.address(project_id, environment.id).await {
            Ok(address) => address.warning,
            Err(error) => {
                warn!(
                    project_id,
                    environment_id = environment.id,
                    %error,
                    "could not tell whether the project's environment is reachable"
                );
                None
            }
        }
    }

    pub async fn create_project(
        &self,
        request: CreateProjectRequest,
    ) -> Result<Project, ProjectError> {
        self.create_project_as(request, &SlugClaimCaller::project_writer())
            .await
    }

    /// Create a project on behalf of a caller whose authority is known.
    ///
    /// Only matters for one thing (ADR 045): whether the caller may claim a
    /// slug this host grants host Docker access to. Every other path keeps
    /// [`Self::create_project`], which is fail-closed.
    pub async fn create_project_as(
        &self,
        request: CreateProjectRequest,
        caller: &SlugClaimCaller<'_>,
    ) -> Result<Project, ProjectError> {
        self.create_project_with_identity(request, None, caller, false, None)
            .await
    }

    /// Create a template-backed service project from an immutable resolved
    /// release. Generic project creation cannot supply this identity, so a
    /// caller cannot turn an arbitrary image project into a trusted service.
    pub async fn create_service_project(
        &self,
        request: CreateProjectRequest,
        service_template: temps_core::templates::ServiceTemplateInstance,
    ) -> Result<Project, ProjectError> {
        self.create_service_project_as(
            request,
            service_template,
            &SlugClaimCaller::project_writer(),
        )
        .await
    }

    /// [`Self::create_service_project`] with the caller's slug-claim authority
    /// (ADR 045).
    pub async fn create_service_project_as(
        &self,
        request: CreateProjectRequest,
        service_template: temps_core::templates::ServiceTemplateInstance,
        caller: &SlugClaimCaller<'_>,
    ) -> Result<Project, ProjectError> {
        Self::check_service_release(&request, &service_template)?;
        self.create_project_with_identity(request, Some(service_template), caller, false, None)
            .await
    }

    /// Create one service of a template (ADR-051). An app's services carry
    /// their release; a starter's do not. With `defer_initial_deployment`,
    /// a source-built service does not deploy until
    /// [`Self::queue_initial_deployment`] is called, so a service created
    /// as part of a larger app first gets its project, shared variables and
    /// routes.
    ///
    /// `instantiation_token` is the ownership marker of the instantiation
    /// creating the service (see
    /// [`temps_entities::projects::instantiation_token`]). The project row
    /// is inserted holding its *pending* spelling, so a run that crashes
    /// after the insert can still recognise the project as its own and
    /// discard it; the final token is only written once the project's
    /// environment, variables and service links exist, so it proves a
    /// complete project. `None` for every creation that is not part of an
    /// instantiation.
    pub async fn create_template_service_as(
        &self,
        request: CreateProjectRequest,
        release: Option<temps_core::templates::ServiceTemplateInstance>,
        caller: &SlugClaimCaller<'_>,
        defer_initial_deployment: bool,
        instantiation_token: Option<String>,
    ) -> Result<Project, ProjectError> {
        if let Some(release) = release.as_ref() {
            Self::check_service_release(&request, release)?;
        }
        self.create_project_with_identity(
            request,
            release,
            caller,
            defer_initial_deployment,
            instantiation_token,
        )
        .await
    }

    /// Queue the first deployment of a source-built project whose creation
    /// deferred it. Image projects deploy through their image job instead.
    pub async fn queue_initial_deployment(&self, project_id: i32) -> Result<(), ProjectError> {
        let project = projects::Entity::find_by_id(project_id)
            .filter(projects::Column::IsDeleted.eq(false))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("project {project_id} not found")))?;
        if !project.source_type.requires_git_info()
            || project.repo_owner.is_empty()
            || project.repo_name.is_empty()
        {
            return Err(ProjectError::InvalidInput(format!(
                "Project {} does not build from a repository",
                project.slug
            )));
        }
        let environment = environments::Entity::find()
            .filter(environments::Column::ProjectId.eq(project_id))
            .filter(environments::Column::DeletedAt.is_null())
            .order_by_asc(environments::Column::Id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| {
                ProjectError::NotFound(format!("project {} has no environment", project.slug))
            })?;
        self.queue_initial_deployment_job(&project, &environment)
            .await
    }

    async fn create_project_with_identity(
        &self,
        request: CreateProjectRequest,
        service_template: Option<temps_core::templates::ServiceTemplateInstance>,
        caller: &SlugClaimCaller<'_>,
        defer_initial_deployment: bool,
        instantiation_token: Option<String>,
    ) -> Result<Project, ProjectError> {
        let mut request = request;
        if request.source_type == temps_entities::source_type::SourceType::External {
            if request.repo_name.is_some()
                || request.repo_owner.is_some()
                || request.git_provider_connection_id.is_some()
                || request.git_url.is_some()
                || !request.storage_service_ids.is_empty()
                || service_template.is_some()
            {
                return Err(ProjectError::InvalidInput(format!(
                    "Project '{}' is for monitoring only; configure hosting before attaching a repository or runtime services",
                    request.name
                )));
            }
            // The persisted preset is an inert compatibility placeholder. It is
            // never exposed or executed until hosting is explicitly enabled.
            request.preset = "dockerfile".to_string();
            request.preset_config = None;
            request.directory = "/".to_string();
            request.main_branch = "main".to_string();
            request.automatic_deploy = false;
        }
        if request.template_slug.as_deref().is_some_and(|slug| {
            slug.chars().count() > temps_core::templates::MAX_TEMPLATE_SLUG_CHARS
        }) {
            return Err(ProjectError::InvalidInput(format!(
                "Template slug cannot exceed {} characters",
                temps_core::templates::MAX_TEMPLATE_SLUG_CHARS
            )));
        }

        // Reject unusable env vars before the project row exists. Catching this
        // here keeps it a 400 on the request that caused it, instead of a 500
        // from the post-insert finalize step that then rolls the project back.
        if let Some(env_vars) = request.environment_variables.as_ref() {
            for env_var in env_vars {
                if env_var.key.trim().is_empty() {
                    return Err(ProjectError::InvalidInput(
                        "Environment variable names cannot be empty".to_string(),
                    ));
                }
                if env_var.is_secret && env_var.value.is_empty() {
                    return Err(ProjectError::InvalidInput(format!(
                        "Environment variable '{}' is marked as a secret but has no value. \
                         Provide a value or clear the secret flag.",
                        env_var.key
                    )));
                }
            }
        }

        // Verify storage service IDs exist if provided
        if !request.storage_service_ids.is_empty() {
            use temps_entities::external_services;

            // Get count of matching services using SeaORM
            let found_count = external_services::Entity::find()
                .filter(external_services::Column::Id.is_in(request.storage_service_ids.clone()))
                .count(self.db.as_ref())
                .await
                .map_err(|e| ProjectError::Other(e.to_string()))?;

            // Verify all IDs were found
            if found_count != request.storage_service_ids.len() as u64 {
                return Err(ProjectError::InvalidInput(
                    "One or more storage service IDs not found".to_string(),
                ));
            }
        }

        // The instance checkbox is read only at creation time. An explicit
        // project choice takes precedence, and neither path modifies older projects.
        let delivery_defaults = settings::Entity::find_by_id(1)
            .one(self.db.as_ref())
            .await?
            .map(|row| temps_core::AppSettings::from_json(row.data))
            .unwrap_or_default();
        let delivery_choice = chosen_delivery_provider(
            request.delivery_provider.as_deref(),
            request.cloudflare_enabled,
            delivery_defaults.cloudflare_new_projects,
            delivery_defaults.bunny_new_projects,
        )?;
        let delivery_profile_id = match delivery_choice.provider {
            Some(provider) => match self.usable_delivery_profile(provider).await? {
                Ok(profile_id) => Some(profile_id),
                Err(missing) if delivery_choice.explicit => {
                    return Err(ProjectError::InvalidInput(format!(
                        "{missing} before enabling {} for project '{}'",
                        delivery_provider_label(provider),
                        request.name
                    )));
                }
                Err(missing) => {
                    // An inherited default must never make project creation
                    // itself fail: every importer and the starter project rely
                    // on it. The project is created without CDN delivery and
                    // the operator can enable it once the provider is set up.
                    warn!(
                        project_name = %request.name,
                        provider,
                        reason = %missing,
                        "Instance default enables {} delivery for new projects, but it is not usable; creating project '{}' without CDN delivery",
                        delivery_provider_label(provider),
                        request.name
                    );
                    None
                }
            },
            None => None,
        };

        let normalized_directory = normalize_project_directory(&request.directory)?;

        let validated_name = validate_project_name(&request.name)?;
        self.ensure_project_name_available(&validated_name, None)
            .await?;
        let project_slug = if let Some(expected_slug) = request.expected_slug.as_deref() {
            self.validate_expected_project_slug(&validated_name, expected_slug)
                .await?;
            expected_slug.to_string()
        } else {
            self.generate_unique_project_slug(&validated_name).await?
        };
        // ADR 045: claiming a slug this host grants the Docker socket to is
        // claiming host root on every machine that grants it, so it is an
        // admin-only act. Checked here rather than in the handler because the
        // slug can also be *derived* from the display name a line above —
        // a caller never has to name it to claim it.
        self.guard_reserved_slug(&project_slug, caller, None, ReservedSlugChange::Claim)
            .await?;
        let resolved = resolve_preset_selection(
            request.preset.as_str(),
            request.preset_config.as_ref(),
            None,
        )?;
        let preset = resolved.preset;
        let preset_config = resolved.config.clone();
        // An app's image services are service projects: their runtime is the
        // pinned release. Services an app builds from source keep the type
        // their preset gives them (a static frontend stays static); their
        // snapshot still records the release and its database bindings, which
        // `projects_service_template_identity` allows on a server/static row
        // that names its template (checked by `check_service_release`).
        let project_type = if service_template
            .as_ref()
            .and_then(|release| release.spec())
            .is_some_and(|spec| spec.is_image())
        {
            ProjectType::Service
        } else {
            project_type_for_resolved_preset(&resolved)?
        };
        let service_template_json = service_template
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| {
                ProjectError::InvalidInput(format!(
                    "Could not serialize the resolved service template: {error}"
                ))
            })?;

        // Create deployment config with resource and deployment settings.
        // New hosted websites get the conservative "small" profile by default:
        // a scheduling request plus a hard memory limit so a runaway app cannot
        // OOM a small single-node host. Operators can still choose standard,
        // dedicated, or explicit uncapped limits later via deployment settings.
        let deployment_config = Some(initial_deployment_config(&request));
        if let Some(config) = deployment_config.as_ref() {
            config.validate().map_err(|reason| {
                ProjectError::InvalidInput(format!(
                    "Invalid initial deployment configuration for project '{}': {reason}",
                    request.name
                ))
            })?;
        }
        if request.cpu_request.is_some()
            || request.cpu_limit.is_some()
            || request.memory_request.is_some()
            || request.memory_limit.is_some()
        {
            let app_settings = self.config_service.get_settings().await.map_err(|error| {
                ProjectError::Other(format!(
                    "Failed to read instance settings before applying template resources: {error}"
                ))
            })?;
            if let Some(config) = deployment_config.as_ref() {
                Self::enforce_tenant_ceilings(config, &app_settings)?;
            }
        }

        // SSRF guard: validate git_url before persisting (Fix #12).
        if let Some(ref git_url) = request.git_url {
            validate_git_url(git_url).map_err(|e| ProjectError::InvalidGitUrl {
                url: redact_url_password(git_url),
                reason: e.to_string(),
            })?;
        }

        // The row is inserted holding the *pending* spelling of the token and
        // only switched to the final one after it is fully initialized (see
        // the crash-window note below), so the final token never marks a
        // half-initialized project.
        let token_for_diagnosis = instantiation_token.clone();
        let token_for_seal = instantiation_token.clone();
        let pending_instantiation_token = instantiation_token
            .as_deref()
            .map(temps_entities::projects::pending_instantiation_token);

        // Explain a root directory the repository does not have now, rather
        // than as a bare ENOENT from the first build (issue #1350).
        if request.source_type == temps_entities::source_type::SourceType::Git {
            self.verify_root_directory_in_repository(
                None,
                SourceDirectoryTarget {
                    connection_id: request.git_provider_connection_id,
                    is_public_repo: request.is_public_repo.unwrap_or(false),
                    git_url: request.git_url.as_deref(),
                    repo_owner: request.repo_owner.as_deref().unwrap_or_default(),
                    repo_name: request.repo_name.as_deref().unwrap_or_default(),
                    branch: &request.main_branch,
                    directory: &normalized_directory,
                },
            )
            .await?;
        }

        // A token has at most one project. Say so up front, before the
        // project, its environment and its links are created and rolled
        // back; the unique index stays the backstop for a concurrent holder.
        if let Some(token) = token_for_diagnosis.as_deref() {
            if let Some(holder) = self.instantiation_token_holder(token).await {
                return Err(ProjectError::DatabaseError {
                    reason: format!(
                        "Project {} ('{}') already holds instantiation token '{token}'; \
                         not creating a second project for '{}'",
                        holder.id, holder.slug, project_slug
                    ),
                });
            }
        }

        let project = projects::ActiveModel {
            name: Set(validated_name),
            repo_name: Set(request.repo_name.unwrap_or_default()),
            repo_owner: Set(request.repo_owner.unwrap_or_default()),
            directory: Set(normalized_directory),
            main_branch: Set(request.main_branch),
            preset: Set(preset), // Now required, not Option
            preset_config: Set(preset_config),
            deployment_config: Set(deployment_config),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            slug: Set(project_slug.clone()),
            is_public_repo: Set(request.is_public_repo.unwrap_or(false)),
            git_url: Set(request.git_url),
            git_provider_connection_id: Set(request.git_provider_connection_id),
            deleted_at: Set(None),
            last_deployment: Set(None),
            source_type: Set(request.source_type),
            project_type: Set(project_type),
            template_slug: Set(request.template_slug),
            service_template: Set(service_template_json),
            hosting: Set(request.hosting),
            instantiation_token: Set(pending_instantiation_token.clone()),
            ..Default::default()
        };

        // Insert the project. The slug column has a UNIQUE index — if a
        // concurrent request raced us to the same slug, surface a typed
        // SlugConflict (HTTP 409) instead of a generic 500.
        let project_found_db = match self.persist_named_project(project, true).await {
            Ok(model) => model,
            Err(ProjectError::SlugConflict { slug }) => {
                // The token has its own unique index: say so when it is the
                // token that collided, not the slug.
                if let Some(token) = token_for_diagnosis.as_deref() {
                    if let Some(holder) = self.instantiation_token_holder(token).await {
                        return Err(ProjectError::DatabaseError {
                            reason: format!(
                                "Project {} ('{}') already holds instantiation token '{token}'; \
                                 not creating a second project for '{}'",
                                holder.id, holder.slug, slug
                            ),
                        });
                    }
                }
                return Err(ProjectError::SlugConflict { slug });
            }
            Err(e) => return Err(e),
        };

        info!(
            "Created project id={} slug={} preset={}",
            project_found_db.id, project_found_db.slug, project_found_db.preset
        );

        // From here on, the project row exists. If any downstream step
        // fails, hard-delete it (CASCADE cleans up environments, env vars,
        // service links, etc.) before returning so the caller never sees
        // a half-initialized project. This is the manual rollback recommended
        // in CLAUDE.md "Resource Cleanup" — a real txn would require pushing
        // a `&impl ConnectionTrait` through every dependent service, which
        // is a much larger refactor.
        //
        // Crash window. The environment, variables and service links are
        // separate writes, so a process that stops between the INSERT and
        // the end of `finalize_project_creation` leaves a project row that
        // this rollback never saw. The ownership token is therefore written
        // in two steps: the INSERT carries the *pending* token, and only
        // once initialization succeeded is it switched to the final one
        // (`seal_instantiation_token`, a single conditional UPDATE). Whoever
        // finds a project holding the final token may rely on it being
        // complete; one holding the pending token is provably the run's own
        // but unfinished, and is discarded and recreated rather than
        // adopted (`discard_unfinished_instantiation_project`). A crash
        // after the seal and before this call returns loses only the
        // best-effort `ProjectCreated` job below, which was never part of
        // what the token vouches for. The unique index covers both
        // spellings, so at most one project per (instantiation, service)
        // can ever hold either, and a second creation that raced past the
        // lease fails the seal and is rolled back here.
        let project_id = project_found_db.id;
        let mut project_found_db = project_found_db;
        let initialized = match self
            .finalize_project_creation(
                &project_found_db,
                request.environment_variables,
                request.storage_service_ids,
                request.storage_service_claim_ids,
                request.storage_service_claim_user_id,
                delivery_profile_id,
            )
            .await
        {
            Ok(env) => match (
                pending_instantiation_token.as_deref(),
                token_for_seal.as_deref(),
            ) {
                (Some(pending), Some(token)) => self
                    .seal_instantiation_token(project_id, pending, token)
                    .await
                    .map(|()| env),
                _ => Ok(env),
            },
            Err(err) => Err(err),
        };
        let default_environment = match initialized {
            Ok(env) => {
                if let Some(token) = token_for_seal {
                    project_found_db.instantiation_token = Some(token);
                }
                env
            }
            Err(err) => {
                tracing::error!(
                    "Project {} creation failed after insert, rolling back: {}",
                    project_id,
                    err
                );
                if let Err(cleanup_err) = temps_entities::projects::Entity::delete_by_id(project_id)
                    .exec(self.db.as_ref())
                    .await
                {
                    tracing::error!(
                        "Failed to roll back project {} after creation error: {}",
                        project_id,
                        cleanup_err
                    );
                }
                return Err(err);
            }
        };

        // Emit ProjectCreated job
        let project_created_job = Job::ProjectCreated(ProjectCreatedJob {
            project_id: project_found_db.id,
            project_name: project_found_db.name.clone(),
        });

        if let Err(e) = self.queue_service.send(project_created_job).await {
            warn!(
                "Failed to emit ProjectCreated job for project {}: {}",
                project_found_db.id, e
            );
        } else {
            info!(
                "Emitted ProjectCreated job for project {}",
                project_found_db.id
            );
        }
        // Queue initial deployment/pipeline job only for Git-based projects with repository information
        // For docker_image and static_files source types, deployments are triggered via API
        if defer_initial_deployment {
            info!(
                "Deferring initial deployment for project {} until its template is assembled",
                project_found_db.id
            );
        } else if project_found_db.hosting == temps_entities::types::Hosting::External {
            info!(
                "Project {} runs outside Temps; it is never deployed",
                project_found_db.id
            );
        } else if project_found_db.source_type.requires_git_info()
            && !project_found_db.repo_owner.is_empty()
            && !project_found_db.repo_name.is_empty()
        {
            info!(
                "Queueing initial deployment job for Git project: {}",
                project_found_db.id
            );

            match self
                .queue_initial_deployment_job(&project_found_db, &default_environment)
                .await
            {
                Ok(()) => {
                    info!(
                        "Successfully queued deployment job for project {}",
                        project_found_db.id
                    );
                }
                Err(e) => {
                    // Log error but don't fail project creation
                    tracing::error!(
                        "Failed to queue deployment job for project {}: {}",
                        project_found_db.id,
                        e
                    );
                }
            }
        } else {
            info!(
                "Skipping initial deployment for project {} (source_type: {})",
                project_found_db.id, project_found_db.source_type
            );
        }

        // Auto-install GitLab webhook if applicable (best-effort, non-fatal).
        let project_found_db = if let Some(conn_id) = project_found_db.git_provider_connection_id {
            let repo_owner = project_found_db.repo_owner.clone();
            let repo_name = project_found_db.repo_name.clone();
            if !repo_owner.is_empty() && !repo_name.is_empty() {
                match self
                    .install_gitlab_webhook_for_connection(
                        project_found_db.id,
                        conn_id,
                        &repo_owner,
                        &repo_name,
                    )
                    .await
                {
                    Ok((hook_id, encrypted_token)) => {
                        let mut active = projects::ActiveModel::from(project_found_db.clone());
                        active.gitlab_webhook_id = Set(Some(hook_id as i32));
                        active.gitlab_webhook_signing_token = Set(Some(encrypted_token));
                        active.updated_at = Set(chrono::Utc::now());
                        match active.update(self.db.as_ref()).await {
                            Ok(updated) => updated,
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to persist GitLab webhook fields on new project {}: {}",
                                    project_found_db.id,
                                    e
                                );
                                project_found_db
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to install GitLab webhook for new project {}: {}",
                            project_found_db.id,
                            e
                        );
                        project_found_db
                    }
                }
            } else {
                project_found_db
            }
        } else {
            project_found_db
        };

        // Auto-install Bitbucket Cloud webhook if applicable (best-effort, non-fatal).
        let project_found_db = if let Some(conn_id) = project_found_db.git_provider_connection_id {
            let repo_owner = project_found_db.repo_owner.clone();
            let repo_name = project_found_db.repo_name.clone();
            if !repo_owner.is_empty() && !repo_name.is_empty() {
                match self
                    .install_bitbucket_webhook_for_connection(
                        project_found_db.id,
                        conn_id,
                        &repo_owner,
                        &repo_name,
                    )
                    .await
                {
                    Ok((hook_uuid, encrypted_token)) => {
                        let mut active = projects::ActiveModel::from(project_found_db.clone());
                        active.bitbucket_webhook_hook_id = Set(Some(hook_uuid));
                        active.bitbucket_webhook_token = Set(Some(encrypted_token));
                        active.updated_at = Set(chrono::Utc::now());
                        match active.update(self.db.as_ref()).await {
                            Ok(updated) => updated,
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to persist Bitbucket webhook fields on new project {}: {}",
                                    project_found_db.id,
                                    e
                                );
                                project_found_db
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to install Bitbucket webhook for new project {}: {}",
                            project_found_db.id,
                            e
                        );
                        project_found_db
                    }
                }
            } else {
                project_found_db
            }
        } else {
            project_found_db
        };

        // Auto-install a GitHub repository webhook for non-App GitHub
        // connections (best-effort, non-fatal). Without it a PAT/OAuth
        // project never hears about a push (#1596).
        let project_found_db = if let Some(conn_id) = project_found_db.git_provider_connection_id {
            let repo_owner = project_found_db.repo_owner.clone();
            let repo_name = project_found_db.repo_name.clone();
            if !repo_owner.is_empty() && !repo_name.is_empty() {
                match self
                    .install_github_webhook_for_connection(
                        project_found_db.id,
                        conn_id,
                        &repo_owner,
                        &repo_name,
                        None,
                    )
                    .await
                {
                    Ok(Some((hook_id, encrypted_secret))) => {
                        let mut active = projects::ActiveModel::from(project_found_db.clone());
                        active.github_webhook_id = Set(Some(hook_id));
                        active.github_webhook_signing_token = Set(Some(encrypted_secret));
                        active.updated_at = Set(chrono::Utc::now());
                        match active.update(self.db.as_ref()).await {
                            Ok(updated) => updated,
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to persist GitHub webhook fields on new project {}: {}",
                                    project_found_db.id,
                                    e
                                );
                                project_found_db
                            }
                        }
                    }
                    Ok(None) => project_found_db,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to install GitHub webhook for new project {}: {}",
                            project_found_db.id,
                            e
                        );
                        project_found_db
                    }
                }
            } else {
                project_found_db
            }
        } else {
            project_found_db
        };

        // Auto-install Gitea webhook if applicable (best-effort, non-fatal).
        // BLOCKER 1 fix: gitea_webhook_signing_token was never written.
        let project_found_db = if let Some(conn_id) = project_found_db.git_provider_connection_id {
            let repo_owner = project_found_db.repo_owner.clone();
            let repo_name = project_found_db.repo_name.clone();
            if !repo_owner.is_empty() && !repo_name.is_empty() {
                match self
                    .install_gitea_webhook_for_connection(
                        project_found_db.id,
                        conn_id,
                        &repo_owner,
                        &repo_name,
                    )
                    .await
                {
                    Ok(encrypted_token) => {
                        let mut active = projects::ActiveModel::from(project_found_db.clone());
                        active.gitea_webhook_signing_token = Set(Some(encrypted_token));
                        active.updated_at = Set(chrono::Utc::now());
                        match active.update(self.db.as_ref()).await {
                            Ok(updated) => updated,
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to persist Gitea webhook token on new project {}: {}",
                                    project_found_db.id,
                                    e
                                );
                                project_found_db
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to install Gitea webhook for new project {}: {}",
                            project_found_db.id,
                            e
                        );
                        project_found_db
                    }
                }
            } else {
                project_found_db
            }
        } else {
            project_found_db
        };

        // Auto-install Generic webhook token if applicable (best-effort, non-fatal).
        // BLOCKER 2 fix: generic_webhook_token was never written.
        // Generic has no remote API — generate the token and store it; operators
        // configure the webhook URL manually.
        let project_found_db = if let Some(conn_id) = project_found_db.git_provider_connection_id {
            match self
                .install_generic_webhook_token(project_found_db.id, conn_id)
                .await
            {
                Ok(Some(encrypted_token)) => {
                    let mut active = projects::ActiveModel::from(project_found_db.clone());
                    active.generic_webhook_token = Set(Some(encrypted_token));
                    active.updated_at = Set(chrono::Utc::now());
                    match active.update(self.db.as_ref()).await {
                        Ok(updated) => updated,
                        Err(e) => {
                            tracing::warn!(
                                "Failed to persist Generic webhook token on new project {}: {}",
                                project_found_db.id,
                                e
                            );
                            project_found_db
                        }
                    }
                }
                Ok(None) => {
                    // Not a Generic connection — nothing to do.
                    project_found_db
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to install Generic webhook token for new project {}: {}",
                        project_found_db.id,
                        e
                    );
                    project_found_db
                }
            }
        } else {
            project_found_db
        };

        Ok(self.map_written_project(project_found_db).await)
    }

    /// The project holding instantiation token `token`, under either its
    /// final or its pending spelling (soft-deleted ones included: they keep
    /// the token). Best effort, for diagnostics and early refusal; the unique
    /// index is what enforces "at most one".
    async fn instantiation_token_holder(&self, token: &str) -> Option<projects::Model> {
        let spellings = [
            token.to_string(),
            temps_entities::projects::pending_instantiation_token(token),
        ];
        projects::Entity::find()
            .filter(projects::Column::InstantiationToken.is_in(spellings))
            .one(self.db.as_ref())
            .await
            .ok()
            .flatten()
    }

    /// Switch a project from its pending instantiation token to the final
    /// one, declaring its initialization complete.
    ///
    /// Conditional on the row still holding `pending`, so it can never
    /// re-label a project it did not just create. A unique violation means
    /// another project already holds `token`: this run lost a race it should
    /// not have been in, and the caller rolls its project back.
    async fn seal_instantiation_token(
        &self,
        project_id: i32,
        pending: &str,
        token: &str,
    ) -> Result<(), ProjectError> {
        use sea_orm::sea_query::Expr;
        let result = projects::Entity::update_many()
            .col_expr(projects::Column::InstantiationToken, Expr::value(token))
            .filter(projects::Column::Id.eq(project_id))
            .filter(projects::Column::InstantiationToken.eq(pending))
            .exec(self.db.as_ref())
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) if super::types::is_unique_violation(&error) => {
                let holder = match self.instantiation_token_holder(token).await {
                    Some(holder) => format!("project {} ('{}')", holder.id, holder.slug),
                    None => "another project".to_string(),
                };
                return Err(ProjectError::DatabaseError {
                    reason: format!(
                        "Project {project_id} cannot take instantiation token '{token}': \
                         {holder} already holds it"
                    ),
                });
            }
            Err(error) => {
                return Err(ProjectError::DatabaseError {
                    reason: format!(
                        "Failed to record that project {project_id} finished initializing \
                         (instantiation token '{token}'): {error}"
                    ),
                });
            }
        };
        if result.rows_affected != 1 {
            return Err(ProjectError::DatabaseError {
                reason: format!(
                    "Project {project_id} no longer holds pending instantiation token \
                     '{pending}'; it was not marked as initialized"
                ),
            });
        }
        Ok(())
    }

    /// Hard-delete a project an instantiation began creating and never
    /// finished: its row still holds the *pending* token `pending_token`, so
    /// it has no (or only some of its) environment, variables and service
    /// links, and was never exposed or deployed.
    ///
    /// Conditional on the row still holding that token, so a project that
    /// has since been sealed (or anything else) is never touched. Returns
    /// whether a row was removed. Dependent rows go with it through the same
    /// cascade the in-process rollback of `create_project` relies on.
    pub async fn discard_unfinished_instantiation_project(
        &self,
        project_id: i32,
        pending_token: &str,
    ) -> Result<bool, ProjectError> {
        if !temps_entities::projects::is_pending_instantiation_token(pending_token) {
            return Err(ProjectError::InvalidInput(format!(
                "Refusing to discard project {project_id}: '{pending_token}' is not a pending \
                 instantiation token"
            )));
        }
        let result = projects::Entity::delete_many()
            .filter(projects::Column::Id.eq(project_id))
            .filter(projects::Column::InstantiationToken.eq(pending_token))
            .exec(self.db.as_ref())
            .await
            .map_err(|error| ProjectError::DatabaseError {
                reason: format!(
                    "Failed to discard unfinished project {project_id} \
                     (instantiation token '{pending_token}'): {error}"
                ),
            })?;
        Ok(result.rows_affected > 0)
    }

    /// Post-insert steps for `create_project`. Returns the default environment
    /// on success. On any error, the caller is responsible for rolling back
    /// the project row.
    async fn finalize_project_creation(
        &self,
        project: &projects::Model,
        environment_variables: Option<Vec<CreateProjectEnvVar>>,
        storage_service_ids: Vec<i32>,
        storage_service_claim_ids: Vec<i32>,
        storage_service_claim_user_id: Option<i32>,
        delivery_profile_id: Option<i32>,
    ) -> Result<temps_entities::environments::Model, ProjectError> {
        let project_config = project.deployment_config.as_ref();
        let default_environment = self
            .environment_service
            .create_environment(
                project.id,
                "production".to_string(),
                project_config
                    .and_then(|config| config.cpu_request)
                    .or(Some(DEFAULT_CPU_REQUEST)),
                // The production environment starts with the project's
                // effective profile so its normal precedence does not shadow
                // curated template requirements with platform defaults.
                project_config.and_then(|config| config.cpu_limit),
                project_config
                    .and_then(|config| config.memory_request)
                    .or(Some(DEFAULT_MEMORY_REQUEST)),
                project_config
                    .and_then(|config| config.memory_limit)
                    .or(Some(DEFAULT_MEMORY_LIMIT)),
                project.main_branch.clone(),
            )
            .await
            .map_err(|e| ProjectError::EnvironmentCreationFailed {
                project_id: project.id,
                reason: e.to_string(),
            })?;

        info!(
            "Created default environment for project: {}",
            default_environment.id
        );

        if let Some(env_vars) = environment_variables {
            for env_var in env_vars {
                let CreateProjectEnvVar {
                    key,
                    value,
                    is_secret,
                } = env_var;
                self.env_var_service
                    .create_environment_variable(
                        project.id,
                        vec![default_environment.id],
                        key.clone(),
                        value,
                        is_secret,
                    )
                    .await
                    .map_err(|e| ProjectError::EnvVarCreationFailed {
                        project_id: project.id,
                        key,
                        reason: e.to_string(),
                    })?;
            }
        }

        if !storage_service_ids.is_empty() {
            info!(
                "Linking {} storage services to project {}",
                storage_service_ids.len(),
                project.id
            );
            let claims = storage_service_claim_user_id
                .map(|user_id| {
                    storage_service_claim_ids
                        .into_iter()
                        .map(|service_id| (service_id, user_id))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            self.external_service_manager
                .link_services_to_project_with_claims(&storage_service_ids, project.id, &claims)
                .await
                .map_err(|e| ProjectError::StorageLinksFailed {
                    project_id: project.id,
                    service_ids: storage_service_ids,
                    reason: e.to_string(),
                })?;
        }

        if let Some(profile_id) = delivery_profile_id {
            project_delivery_settings::ActiveModel {
                project_id: Set(project.id),
                default_profile_id: Set(Some(profile_id)),
                updated_at: Set(chrono::Utc::now()),
            }
            .insert(self.db.as_ref())
            .await
            .map_err(|error| ProjectError::DatabaseError {
                reason: format!("Project {} delivery setup failed: {error}", project.id),
            })?;
        }

        Ok(default_environment)
    }

    pub async fn update_project(
        &self,
        project_id: i32,
        request: CreateProjectRequest,
        caller: DeployCaller,
    ) -> Result<Project, ProjectError> {
        // Find the existing project
        let project = projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await?
            .ok_or(ProjectError::NotFound(format!(
                "project {} not found",
                project_id
            )))?;
        if project.project_type == ProjectType::Service {
            return Err(ProjectError::InvalidInput(
                "Service projects must be configured through the service runtime and template update controls"
                    .to_string(),
            ));
        }

        // ADR 045: this method unconditionally rewrites repo_owner, repo_name,
        // directory, main_branch and preset/preset_config below -- the same
        // source-definition fields `update_project_settings_as` gates -- but
        // took no caller at all until this was found to bypass that guard
        // entirely through a sibling handler. The guard is therefore
        // unconditional here too, since every call rewrites every field.
        self.guard_granted_project_write(
            &project.slug,
            "the source repository, branch, directory or build preset",
            caller,
        )?;

        let normalized_directory = normalize_project_directory(&request.directory)?;

        if project.source_type == temps_entities::source_type::SourceType::Git {
            let target = SourceDirectoryTarget {
                connection_id: project.git_provider_connection_id,
                is_public_repo: project.is_public_repo,
                git_url: project.git_url.as_deref(),
                repo_owner: request.repo_owner.as_deref().unwrap_or("unknown"),
                repo_name: request.repo_name.as_deref().unwrap_or("unknown"),
                branch: &request.main_branch,
                directory: &normalized_directory,
            };
            if target.differs_from(&project) {
                self.verify_root_directory_in_repository(Some(project_id), target)
                    .await?;
            }
        }

        let resolved = resolve_preset_selection(
            request.preset.as_str(),
            request.preset_config.as_ref(),
            project.preset_config.as_ref(),
        )?;

        let name = validate_project_name(&request.name)?;
        if name.to_lowercase() != project.name.to_lowercase() {
            self.ensure_project_name_available(&name, Some(project_id))
                .await?;
        }

        // Update the project
        let mut active_project: projects::ActiveModel = project.into();
        active_project.name = Set(name);
        active_project.repo_name = Set(request.repo_name.unwrap_or_else(|| "unknown".to_string()));
        active_project.repo_owner =
            Set(request.repo_owner.unwrap_or_else(|| "unknown".to_string()));
        active_project.directory = Set(normalized_directory);
        active_project.main_branch = Set(request.main_branch);
        apply_resolved_preset(&mut active_project, resolved)?;
        active_project.updated_at = Set(chrono::Utc::now());

        let project_found = self.persist_named_project(active_project, false).await?;

        // Emit ProjectUpdated before reading anything else. Everything after
        // the commit is a fresh await point, and a cancelled request (client
        // disconnect, timeout) drops the task there — so the notification goes
        // out first, and only the response can be lost.
        let project_updated_job = Job::ProjectUpdated(ProjectUpdatedJob {
            project_id: project_found.id,
            project_name: project_found.name.clone(),
        });

        if let Err(e) = self.queue_service.send(project_updated_job).await {
            warn!(
                "Failed to emit ProjectUpdated job for project {}: {}",
                project_found.id, e
            );
        } else {
            info!(
                "Emitted ProjectUpdated job for project {}",
                project_found.id
            );
        }

        Ok(self.map_written_project(project_found).await)
    }

    pub async fn update_project_settings(
        &self,
        project_id: i32,
        params: UpdateProjectSettingsParams,
    ) -> Result<ProjectSettingsUpdate, ProjectError> {
        self.update_project_settings_as(project_id, params, &SlugClaimCaller::project_writer())
            .await
    }

    /// [`Self::update_project_settings`] for a caller whose authority is known.
    ///
    /// Only the *claim* is gated (ADR 045): a project whose slug already
    /// matches the host grant keeps working through every update that does not
    /// change it, whoever sends them.
    pub async fn update_project_settings_as(
        &self,
        project_id: i32,
        params: UpdateProjectSettingsParams,
        caller: &SlugClaimCaller<'_>,
    ) -> Result<ProjectSettingsUpdate, ProjectError> {
        let UpdateProjectSettingsParams {
            name: new_name,
            slug: new_slug,
            git_provider_connection_id,
            main_branch,
            repo_owner,
            repo_name,
            preset,
            directory,
            attack_mode,
            shield_mode,
            enable_preview_environments,
            preview_envs_on_demand,
            preview_envs_idle_timeout_seconds,
            preview_envs_wake_timeout_seconds,
            preset_config,
            ai_alert_summaries_enabled,
            cross_project_trace_sharing,
            serves_own_temps_api,
            error_source_context_enabled,
            vulnerability_scanning_enabled,
            error_source_root,
            ai_api_traffic_summary_enabled,
            image_retention_hours,
            log_ingest_lines_per_sec,
            log_ingest_kib_per_sec,
        } = params;

        // Validate preview env on-demand timeouts before touching the DB.
        // Mirrors DeploymentConfig::validate so the project-level defaults are
        // never out of range.
        if let Some(idle) = preview_envs_idle_timeout_seconds {
            if !(60..=86400).contains(&idle) {
                return Err(ProjectError::InvalidInput(format!(
                    "preview_envs_idle_timeout_seconds {} is not in valid range (60-86400)",
                    idle
                )));
            }
        }
        if let Some(wake) = preview_envs_wake_timeout_seconds {
            if !(5..=120).contains(&wake) {
                return Err(ProjectError::InvalidInput(format!(
                    "preview_envs_wake_timeout_seconds {} is not in valid range (5-120)",
                    wake
                )));
            }
        }
        if let Some(Some(hours)) = image_retention_hours {
            if !(1..=8760).contains(&hours) {
                return Err(ProjectError::InvalidInput(format!(
                    "image_retention_hours {} is not in valid range (1-8760)",
                    hours
                )));
            }
        }
        temps_core::ContainerLogSettings::validate_ingest_override(
            log_ingest_lines_per_sec.flatten(),
            log_ingest_kib_per_sec.flatten(),
        )
        .map_err(|reason| {
            ProjectError::InvalidInput(format!("Project {}: {}", project_id, reason))
        })?;
        // Normalize the display name up-front with the other validations so an
        // unusable name is rejected before anything is written, then persist it
        // only at the end (see below) — a rename must not survive a request that
        // goes on to fail on a conflicting slug or a git desync.
        let new_name = match new_name {
            Some(raw) => Some(validate_project_name(&raw)?),
            None => None,
        };

        // Get the current project
        let mut project = projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await?
            .ok_or(ProjectError::NotFound(format!(
                "Project {} not found",
                project_id
            )))?;
        if project.project_type == ProjectType::Service
            && (git_provider_connection_id.is_some()
                || main_branch.is_some()
                || repo_owner.is_some()
                || repo_name.is_some()
                || preset.is_some()
                || directory.is_some()
                || preset_config.is_some())
        {
            return Err(ProjectError::InvalidInput(
                "Service source and runtime fields must be changed through the service controls"
                    .to_string(),
            ));
        }
        // ADR 045, before any of this request's several independent writes
        // commit: on a project this control plane declares, the source
        // definition *is* what runs as host root. Repointing the repository,
        // branch, subdirectory or build preset and then pushing produces a
        // deployment from a git webhook — which carries no authenticated
        // principal, so there is nothing for the deploy guard to check and no
        // HTTP deploy request is ever made. Gating the source here is what
        // closes that two-step path, at the same choke point the slug claim
        // and release are already gated. `git_provider_connection_id` belongs
        // in this list too: `DownloadRepoJob` resolves the clone host from the
        // connection, not from a stored URL, so repointing the connection
        // alone -- without touching owner/name/branch -- is the same
        // escalation through a field this list previously missed.
        // `enable_preview_environments` belongs here for the same reason:
        // once on, any push to a branch no environment already tracks gets a
        // preview environment auto-created and deployed
        // (`find_environments_for_branch`) as `DeployCaller::Platform`, which
        // neither the deploy gate nor the exec gate refuses -- so flipping
        // this one boolean turns "push a branch" into the same host-root
        // execution path a repository repoint gives, without needing the
        // project's primary source touched at all.
        if git_provider_connection_id.is_some()
            || main_branch.is_some()
            || repo_owner.is_some()
            || repo_name.is_some()
            || directory.is_some()
            || preset.is_some()
            || preset_config.is_some()
            || enable_preview_environments.is_some()
        {
            self.guard_granted_project_write(
                &project.slug,
                "the source repository, connection, branch, directory, build preset or preview \
                 environments",
                DeployCaller::from_instance_admin(caller.authority.may_claim_reserved_slug()),
            )?;
        }
        // Refuse a rename onto another project's name before any of this
        // request's independent writes commit.
        if let Some(name_value) = new_name.as_deref() {
            if name_value.to_lowercase() != project.name.to_lowercase() {
                self.ensure_project_name_available(name_value, Some(project_id))
                    .await?;
            }
        }
        // Same for a root directory the repository does not have (issue
        // #1350): checked against the source this request would leave the
        // project with, before any write, and outside every transaction so a
        // provider round-trip never holds a row lock.
        if project.source_type == temps_entities::source_type::SourceType::Git
            && (git_provider_connection_id.is_some()
                || main_branch.is_some()
                || repo_owner.is_some()
                || repo_name.is_some()
                || directory.is_some())
        {
            let normalized_directory = match directory.as_deref() {
                Some(requested) => normalize_project_directory(requested)?,
                None => normalized_stored_directory(&project.directory),
            };
            let target = SourceDirectoryTarget {
                connection_id: match git_provider_connection_id {
                    Some(id) if id > 0 => Some(id),
                    Some(_) => None,
                    None => project.git_provider_connection_id,
                },
                is_public_repo: project.is_public_repo,
                git_url: project.git_url.as_deref(),
                repo_owner: repo_owner.as_deref().unwrap_or(&project.repo_owner),
                repo_name: repo_name.as_deref().unwrap_or(&project.repo_name),
                branch: main_branch.as_deref().unwrap_or(&project.main_branch),
                directory: &normalized_directory,
            };
            if target.differs_from(&project) {
                self.verify_root_directory_in_repository(Some(project_id), target)
                    .await?;
            }
        }
        // Update the slug if provided
        if let Some(slug_value) = new_slug {
            let slug_value = slugify(&slug_value);
            if slug_value.is_empty() || slug_value.len() > 63 {
                return Err(ProjectError::InvalidInput(
                    "Project slug must contain 1-63 lowercase DNS-safe characters".to_string(),
                ));
            }
            // ADR 045: a slug change that touches a granted slug in either
            // direction is admin-only. Renaming *onto* one is a claim on host
            // root; renaming *away* from one revokes an infrastructure
            // service's access and frees the slug for the next project — so
            // guarding only the claim would leave "rename it away, then create
            // your own" open. Re-sending the current slug unchanged is neither,
            // and an ungranted slug is untouched by both: an existing project
            // must keep working through ordinary settings saves.
            if slug_value != project.slug {
                self.guard_reserved_slug(
                    &slug_value,
                    caller,
                    Some(project_id),
                    ReservedSlugChange::Claim,
                )
                .await?;
                self.guard_reserved_slug(
                    &project.slug,
                    caller,
                    Some(project_id),
                    ReservedSlugChange::Release,
                )
                .await?;
            }
            let txn = self.db.begin().await?;
            project = projects::Entity::find_by_id(project_id)
                .lock_exclusive()
                .one(&txn)
                .await?
                .ok_or_else(|| ProjectError::NotFound(format!("Project {project_id} not found")))?;
            // An administrator may have moved this project onto a granted slug
            // since the initial read. Authorize the actual transition under lock.
            if slug_value != project.slug {
                self.guard_reserved_slug(
                    &slug_value,
                    caller,
                    Some(project_id),
                    ReservedSlugChange::Claim,
                )
                .await?;
                self.guard_reserved_slug(
                    &project.slug,
                    caller,
                    Some(project_id),
                    ReservedSlugChange::Release,
                )
                .await?;
            }
            if git_provider_connection_id.is_some()
                || main_branch.is_some()
                || repo_owner.is_some()
                || repo_name.is_some()
                || directory.is_some()
                || preset.is_some()
                || preset_config.is_some()
                || enable_preview_environments.is_some()
            {
                self.guard_granted_project_write(
                    &project.slug,
                    "the source repository, connection, branch, directory, build preset or preview environments",
                    DeployCaller::from_instance_admin(caller.authority.may_claim_reserved_slug()),
                )?;
            }
            txn.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT pg_advisory_xact_lock(hashtext('project-slug:' || $1))",
                [slug_value.clone().into()],
            ))
            .await?;
            // Check if the slug is already taken by another project
            let existing = projects::Entity::find()
                .filter(projects::Column::Slug.eq(&slug_value))
                .filter(projects::Column::Id.ne(project_id))
                .one(&txn)
                .await?;

            if existing.is_some() {
                return Err(ProjectError::SlugAlreadyExists(format!(
                    "Slug '{}' is already taken",
                    slug_value
                )));
            }

            let old_slug = project.slug.clone();
            if old_slug != slug_value {
                let envs = temps_entities::environments::Entity::find()
                    .filter(temps_entities::environments::Column::ProjectId.eq(project_id))
                    .all(&txn)
                    .await?;
                let project_environment_ids = envs.iter().map(|env| env.id).collect::<Vec<_>>();
                let mut target_subdomains = HashSet::new();

                // Acquire claims in deterministic order to avoid deadlocks when
                // concurrent project renames touch multiple hostnames.
                let mut active_claims = envs
                    .iter()
                    .filter(|env| env.deleted_at.is_none())
                    .map(|env| {
                        (
                            format!("{}-{}", slug_value, env.slug).to_ascii_lowercase(),
                            env.id,
                        )
                    })
                    .collect::<Vec<_>>();
                active_claims.sort_unstable();
                for (new_subdomain, _) in &active_claims {
                    if !target_subdomains.insert(new_subdomain.clone()) {
                        return Err(ProjectError::InvalidInput(format!(
                            "Project slug '{}' would create duplicate environment subdomain '{}'",
                            slug_value, new_subdomain
                        )));
                    }
                    if temps_entities::environments::claim_subdomain(
                        &txn,
                        new_subdomain,
                        &project_environment_ids,
                    )
                    .await?
                    .is_some()
                    {
                        return Err(ProjectError::InvalidInput(format!(
                            "Project slug '{}' would use an environment subdomain that is already in use",
                            slug_value
                        )));
                    }
                }

                project.slug = slug_value.clone();
                let mut active_project: projects::ActiveModel = project.clone().into();
                active_project.slug = Set(slug_value.clone());
                active_project.update(&txn).await?;

                for env in envs {
                    let previous_subdomain = env.subdomain.clone();
                    let new_subdomain = format!("{}-{}", slug_value, env.slug).to_ascii_lowercase();

                    // Keep the environment and its auto-managed domain row in
                    // the same transaction as the project rename.
                    let mut active_env: temps_entities::environments::ActiveModel = env.into();
                    active_env.subdomain = Set(new_subdomain.clone());
                    let updated_env = active_env.update(&txn).await?;

                    self.environment_service
                        .reconcile_managed_domain(
                            &txn,
                            &updated_env,
                            project.source_type,
                            Some(&previous_subdomain),
                        )
                        .await
                        .map_err(|error| ProjectError::DatabaseError {
                            reason: format!("Could not reconcile managed domain for environment {} while renaming project {project_id}: {error}", updated_env.id),
                        })?;
                }

                txn.commit().await?;
            } else {
                txn.rollback().await?;
            }
        }

        // Update git_provider_connection_id if provided
        if let Some(connection_id) = git_provider_connection_id {
            let txn = self.db.begin().await?;
            let project = self
                .lock_project_for_source_write(
                    &txn,
                    project_id,
                    "the Git provider connection",
                    DeployCaller::from_instance_admin(caller.authority.may_claim_reserved_slug()),
                )
                .await?;

            // Verify connection exists and is active if non-zero
            if connection_id > 0 {
                use temps_entities::git_provider_connections;
                let connection = git_provider_connections::Entity::find_by_id(connection_id)
                    .one(self.db.as_ref())
                    .await?
                    .ok_or(ProjectError::GitProviderConnectionNotFound { connection_id })?;

                if !connection.is_active {
                    return Err(ProjectError::Other(format!(
                        "Git provider connection {} is not active",
                        connection_id
                    )));
                }

                // Update the project with the new connection ID
                let mut active_project: projects::ActiveModel = project.into();
                active_project.git_provider_connection_id = Set(Some(connection_id));
                active_project.update(&txn).await?;
            } else {
                // Setting to 0 or negative means remove the connection
                let mut active_project: projects::ActiveModel = project.into();
                active_project.git_provider_connection_id = Set(None);
                active_project.update(&txn).await?;
            }
            txn.commit().await?;
        }

        // ADR 066: the Shield mode and the attack-mode column it replaces
        // are written together for one release (`project_shield_write`).
        if shield_mode.is_some() || attack_mode.is_some() {
            // Reload project to ensure we have the latest state
            let project = projects::Entity::find_by_id(project_id)
                .one(self.db.as_ref())
                .await?
                .ok_or(ProjectError::NotFound(format!(
                    "Project {} not found",
                    project_id
                )))?;
            let current = temps_shield::ShieldMode::effective(
                None,
                &project.shield_mode,
                None,
                project.attack_mode,
            );
            if let Some((mode, attack)) =
                temps_environments::project_shield_write(shield_mode, attack_mode, current)
            {
                let mut active_project: projects::ActiveModel = project.into();
                active_project.shield_mode = Set(mode.as_str().to_string());
                active_project.attack_mode = Set(attack);
                active_project.update(self.db.as_ref()).await?;
            }
        }

        // Update AI feature toggles if provided (ADR-021 / ADR-023). These are
        // tri-state opt-ins (Some(true) = on), stored as nullable columns.
        if ai_alert_summaries_enabled.is_some()
            || error_source_context_enabled.is_some()
            || vulnerability_scanning_enabled.is_some()
            || error_source_root.is_some()
            || ai_api_traffic_summary_enabled.is_some()
        {
            let project = projects::Entity::find_by_id(project_id)
                .one(self.db.as_ref())
                .await?
                .ok_or(ProjectError::NotFound(format!(
                    "Project {} not found",
                    project_id
                )))?;
            let mut active_project: projects::ActiveModel = project.into();
            if let Some(v) = ai_alert_summaries_enabled {
                active_project.ai_alert_summaries_enabled = Set(Some(v));
            }
            if let Some(v) = ai_api_traffic_summary_enabled {
                active_project.ai_api_traffic_summary_enabled = Set(Some(v));
            }
            // Opt-in for native error-tracking source context (non-null bool).
            if let Some(v) = error_source_context_enabled {
                active_project.error_source_context_enabled = Set(v);
            }
            // Opt-in for Trivy vulnerability scanning (non-null bool, default false).
            if let Some(v) = vulnerability_scanning_enabled {
                active_project.vulnerability_scanning_enabled = Set(v);
            }
            // Auto-capture source root (nullable). Empty string clears it back
            // to the build-context default.
            if let Some(v) = error_source_root {
                active_project.error_source_root =
                    Set(if v.trim().is_empty() { None } else { Some(v) });
            }
            active_project.update(self.db.as_ref()).await?;
        }

        // Update cross_project_trace_sharing if provided (ADR-027 Phase 3 opt-out).
        if let Some(sharing) = cross_project_trace_sharing {
            let project = projects::Entity::find_by_id(project_id)
                .one(self.db.as_ref())
                .await?
                .ok_or(ProjectError::NotFound(format!(
                    "Project {} not found",
                    project_id
                )))?;
            let mut active_project: projects::ActiveModel = project.into();
            active_project.cross_project_trace_sharing = Set(sharing);
            active_project.update(self.db.as_ref()).await?;
        }

        // Whether the project's own app answers `/api/_temps` and `/api/otel`.
        // The proxy reads it from the route table, which the projects route
        // trigger reloads when this column changes.
        if let Some(serves) = serves_own_temps_api {
            let project = projects::Entity::find_by_id(project_id)
                .one(self.db.as_ref())
                .await?
                .ok_or(ProjectError::NotFound(format!(
                    "Project {} not found",
                    project_id
                )))?;
            let mut active_project: projects::ActiveModel = project.into();
            active_project.serves_own_temps_api = Set(serves);
            active_project.update(self.db.as_ref()).await?;
        }

        // Update preview environment settings and image retention if any are provided
        let needs_project_row_update = enable_preview_environments.is_some()
            || preview_envs_on_demand.is_some()
            || preview_envs_idle_timeout_seconds.is_some()
            || preview_envs_wake_timeout_seconds.is_some()
            || image_retention_hours.is_some()
            || log_ingest_lines_per_sec.is_some()
            || log_ingest_kib_per_sec.is_some();

        if needs_project_row_update {
            let txn = self.db.begin().await?;
            let project = projects::Entity::find_by_id(project_id)
                .lock_exclusive()
                .one(&txn)
                .await?
                .ok_or(ProjectError::NotFound(format!(
                    "Project {} not found",
                    project_id
                )))?;

            if enable_preview_environments.is_some() {
                self.guard_granted_project_write(
                    &project.slug,
                    "preview environments",
                    DeployCaller::from_instance_admin(caller.authority.may_claim_reserved_slug()),
                )?;
            }

            let mut active_project: projects::ActiveModel = project.into();

            if let Some(enable_preview) = enable_preview_environments {
                active_project.enable_preview_environments = Set(enable_preview);
            }
            if let Some(on_demand) = preview_envs_on_demand {
                active_project.preview_envs_on_demand = Set(on_demand);
            }
            if let Some(idle) = preview_envs_idle_timeout_seconds {
                active_project.preview_envs_idle_timeout_seconds = Set(idle);
            }
            if let Some(wake) = preview_envs_wake_timeout_seconds {
                active_project.preview_envs_wake_timeout_seconds = Set(wake);
            }
            if let Some(hours) = image_retention_hours {
                active_project.image_retention_hours = Set(hours);
            }
            if let Some(lines) = log_ingest_lines_per_sec {
                active_project.log_ingest_lines_per_sec = Set(lines);
            }
            if let Some(kib) = log_ingest_kib_per_sec {
                active_project.log_ingest_kib_per_sec = Set(kib);
            }

            active_project.update(&txn).await?;
            txn.commit().await?;
        }

        // Update git-related fields and preset configuration atomically so a
        // config submitted with a new preset is parsed against that new preset.
        let needs_git_update = main_branch.is_some()
            || repo_owner.is_some()
            || repo_name.is_some()
            || preset.is_some()
            || preset_config.is_some()
            || directory.is_some();

        if needs_git_update {
            let txn = self.db.begin().await?;
            let project = self
                .lock_project_for_source_write(
                    &txn,
                    project_id,
                    "the source repository, branch, directory or build preset",
                    DeployCaller::from_instance_admin(caller.authority.may_claim_reserved_slug()),
                )
                .await?;
            let initial_public_ports = compose_public_ports(project.preset_config.as_ref());

            // `repo_owner`/`repo_name` and `git_url` are read by different
            // code paths — branch resolution uses the former, the clone uses
            // the latter — and this endpoint only writes the former. Changing
            // the repo identity here therefore used to leave a stale clone
            // URL behind, and the next deploy resolved a commit from one repo
            // and cloned another:
            //
            //   Starting repository download for owner/new-repo
            //   Checking out ref: <commit that only exists in new-repo>
            //   Cloning public repository from: .../old-repo.git
            //
            // The project could not be recovered through the API. Reject the
            // change when it would actually desync — the git URL is owned by
            // `POST /projects/{id}/git`, which validates it.
            let desync = would_desync_git_url(
                &project.git_url,
                (&project.repo_owner, &project.repo_name),
                (repo_owner.as_deref(), repo_name.as_deref()),
            );
            if let Some((old, new)) = desync {
                return Err(ProjectError::InvalidInput(format!(
                    "Changing the repository to '{new}' would leave the clone URL pointing at \
                     '{old}'. Update both together with POST /projects/{project_id}/git, which \
                     sets git_url alongside the owner and name."
                )));
            }

            let existing_preset_config = project.preset_config.clone();
            let mut active_project: projects::ActiveModel = project.into();

            if let Some(branch) = main_branch {
                active_project.main_branch = Set(branch);
            }
            if let Some(owner) = repo_owner {
                active_project.repo_owner = Set(owner);
            }
            if let Some(name) = repo_name {
                active_project.repo_name = Set(name);
            }
            if let Some(preset_value) = preset {
                let resolved = resolve_preset_selection(
                    preset_value.as_str(),
                    preset_config.as_ref(),
                    existing_preset_config.as_ref(),
                )?;
                apply_resolved_preset(&mut active_project, resolved)?;
            } else if let Some(config_value) = preset_config.as_ref() {
                let parsed = temps_entities::preset::PresetConfig::parse_for_preset(
                    active_project.preset.as_ref(),
                    config_value,
                )
                .map_err(|error| {
                    ProjectError::InvalidInput(format!("Invalid preset config: {}", error))
                })?;
                let merged = merge_preset_config(
                    existing_preset_config.as_ref(),
                    parsed,
                    config_value,
                    true,
                );
                let merged = validate_preset_config(
                    *active_project.preset.as_ref(),
                    merged,
                    Some(config_value),
                )?;
                let resolved = temps_presets::StoredPreset {
                    preset: *active_project.preset.as_ref(),
                    config: Some(merged.clone()),
                };
                if active_project.project_type.as_ref() != &ProjectType::Service {
                    active_project.project_type = Set(project_type_for_resolved_preset(&resolved)?);
                }
                active_project.preset_config = Set(Some(merged));
            }
            if let Some(dir) = directory {
                active_project.directory = Set(normalize_project_directory(&dir)?);
            }
            // Carry the write and the route-reload signal on one transaction.
            // If the signal cannot be published, returning an error while the
            // rename and the preset/directory changes stayed persisted would
            // leave the caller unable to tell what actually happened — and on a
            // port removal, leave a withdrawn route reachable. Dropping the
            // transaction on the error path rolls all of it back together.
            // Fold the rename into this same write so a name change submitted
            // alongside git settings commits atomically with them. The write
            // itself rides `active_project` below rather than a separate update.
            let mut rename = None;
            if let Some(ref name_value) = new_name {
                rename = match self.locked_rename(&txn, project_id, name_value).await {
                    Ok(rename) => rename,
                    Err(error) => {
                        txn.rollback().await?;
                        return Err(error);
                    }
                };
                active_project.name = Set(name_value.clone());
            }

            let updated_project = active_project.update(&txn).await?;
            let ports_changed = initial_public_ports
                != compose_public_ports(updated_project.preset_config.as_ref());
            if ports_changed {
                // Only this transaction rolls back. Earlier steps of this same
                // request (slug, attack mode, AI toggles, preview envs) were
                // committed independently and survive, so do not tell the
                // operator the whole request was discarded.
                self.publish_route_reload(
                    &txn,
                    project_id,
                    "The git settings, preset, directory and name portion of this update was \
                     rolled back; other settings sent in the same request (slug, attack mode, \
                     AI toggles, preview environments) commit independently and may already be \
                     saved.",
                )
                .await?;
            }
            txn.commit().await?;
            if ports_changed {
                self.enqueue_route_reload(project_id).await;
            }
            // Notify before the provider lookup: past the commit, every await
            // is a point the task can be cancelled at, and losing the response
            // is recoverable where losing the notification is not.
            let project_updated_job = Job::ProjectUpdated(ProjectUpdatedJob {
                project_id: updated_project.id,
                project_name: updated_project.name.clone(),
            });

            if let Err(e) = self.queue_service.send(project_updated_job).await {
                warn!(
                    "Failed to emit ProjectUpdated job for project {}: {}",
                    updated_project.id, e
                );
            }

            return Ok(ProjectSettingsUpdate {
                project: self.map_written_project(updated_project).await,
                rename,
            });
        }

        // Persist the rename last, once every other fallible step has
        // succeeded, so a failed request never leaves a renamed project behind.
        //
        // Note the guarantee is one-directional and this endpoint is *not*
        // all-or-nothing: everything above committed independently, so a failure
        // in this block still leaves those earlier steps persisted. Ordering the
        // rename last only ensures it is never the survivor of someone else's
        // failure. Making the whole endpoint atomic belongs with the parked
        // optimistic-concurrency work, not here.
        // The locked read, the write, and the read-back of the final state all
        // ride one transaction: committing before the read-back would let a
        // failure there report an error over an already-persisted rename, and
        // reading back outside the lock would let a concurrent rename supply
        // the "after" half of a transition whose "before" half came from here.
        let mut rename = None;
        let final_project = if let Some(name_value) = new_name {
            let txn = self.db.begin().await?;
            rename = match self.locked_rename(&txn, project_id, &name_value).await {
                Ok(rename) => rename,
                Err(error) => {
                    txn.rollback().await?;
                    return Err(error);
                }
            };
            if rename.is_some() {
                let mut active_project = projects::ActiveModel {
                    id: Set(project_id),
                    ..Default::default()
                };
                active_project.name = Set(name_value);
                active_project.update(&txn).await?;
            }

            let final_project = projects::Entity::find_by_id(project_id)
                .one(&txn)
                .await?
                .ok_or(ProjectError::NotFound(format!(
                    "Project {} not found",
                    project_id
                )))?;
            txn.commit().await?;
            final_project
        } else {
            // No mutation to lose here, so a plain read is enough.
            projects::Entity::find_by_id(project_id)
                .one(self.db.as_ref())
                .await?
                .ok_or(ProjectError::NotFound(format!(
                    "Project {} not found",
                    project_id
                )))?
        };

        let project_updated_job = Job::ProjectUpdated(ProjectUpdatedJob {
            project_id: final_project.id,
            project_name: final_project.name.clone(),
        });

        if let Err(e) = self.queue_service.send(project_updated_job).await {
            warn!(
                "Failed to emit ProjectUpdated job for project {}: {}",
                final_project.id, e
            );
        }

        Ok(ProjectSettingsUpdate {
            project: self.map_written_project(final_project).await,
            rename,
        })
    }

    pub async fn update_automatic_deploy(
        &self,
        project_id: i32,
        automatic_deploy: bool,
        caller: DeployCaller,
    ) -> Result<Project, ProjectError> {
        // Get the current project
        let project = projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await?
            .ok_or(ProjectError::NotFound(format!(
                "Project {} not found",
                project_id
            )))?;
        // ADR 045: arming automatic_deploy is what turns a plain git push
        // into a deployment (`DeployCaller::Platform`, which the deploy gate
        // does not refuse) -- the same mechanism the preview-environment and
        // environment-settings guards close for their own trigger fields.
        self.guard_granted_project_write(
            &project.slug,
            "whether pushes deploy automatically",
            caller,
        )?;
        // Update automatic_deploy setting in deployment_config
        let mut active_project: projects::ActiveModel = project.clone().into();

        // Update deployment config with new automatic_deploy value
        let mut deployment_config = project.deployment_config.clone().unwrap_or_default();
        deployment_config.automatic_deploy = Some(automatic_deploy);
        active_project.deployment_config = Set(Some(deployment_config));

        let updated_project = active_project.update(self.db.as_ref()).await?;

        Ok(self.map_written_project(updated_project).await)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_git_settings(
        &self,
        project_id: i32,
        git_provider_connection_id: Option<i32>,
        main_branch: String,
        repo_owner: String,
        repo_name: String,
        preset: Option<String>,
        directory: String,
        preset_config: Option<serde_json::Value>,
        git_url: Option<String>,
        is_public_repo: Option<bool>,
        pull_only_root_directory: Option<bool>,
        caller: DeployCaller,
    ) -> Result<Project, ProjectError> {
        // Get the current project (includes the old gitlab_webhook_id / signing_token)
        let project = projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await?
            .ok_or(ProjectError::NotFound(format!(
                "Project {} not found",
                project_id
            )))?;
        // ADR 045: the other half of the source-definition gate (see
        // `update_project_settings_as`). This endpoint owns `git_url` — the
        // URL the deployment actually clones — and always rewrites the owner,
        // name, branch and directory, so on a declared project every call
        // decides what the next git push builds and runs as host root.
        self.guard_granted_project_write(&project.slug, "the source repository", caller)?;
        if project.project_type == ProjectType::Service {
            return Err(ProjectError::InvalidInput(
                "A service project cannot be converted to a Git source; update its applied template or runtime instead"
                    .to_string(),
            ));
        }

        // Snapshot fields we need to reason about the old/new repo transition.
        let old_connection_id = project.git_provider_connection_id;
        let old_repo_owner = project.repo_owner.clone();
        let old_repo_name = project.repo_name.clone();
        let old_gitlab_webhook_id = project.gitlab_webhook_id;
        let old_bitbucket_hook_id = project.bitbucket_webhook_hook_id.clone();
        let old_github_webhook_id = project.github_webhook_id;
        // Gitea: no separate hook_id column — we clear gitea_webhook_signing_token
        // on repo change / disconnect. The orphaned remote hook on Gitea is
        // acceptable for v1 (no stored ID to call DELETE with).
        // Generic: no remote API at all — just regenerate the token.

        // Verify git provider connection if provided.
        //
        // Connections are scoped to the installation (workspace-wide), not to
        // the user who created them: a GitHub App installation is shared by
        // design, and PAT connections are intended to be usable by anyone
        // with write access to the project, not just their creator. Access
        // control for this endpoint is enforced by `permission_guard!` and
        // `project_scope_guard!` in the handler, not by connection ownership.
        if let Some(connection_id) = git_provider_connection_id {
            if connection_id > 0 {
                use temps_entities::git_provider_connections;
                let connection = git_provider_connections::Entity::find_by_id(connection_id)
                    .one(self.db.as_ref())
                    .await?
                    .ok_or(ProjectError::GitProviderConnectionNotFound { connection_id })?;

                if !connection.is_active {
                    return Err(ProjectError::Other(format!(
                        "Git provider connection {} is not active",
                        connection_id
                    )));
                }

                // Verify that the branch exists in the repository
                match self
                    .git_provider_manager
                    .get_branch_latest_commit(connection_id, &repo_owner, &repo_name, &main_branch)
                    .await
                {
                    Ok(_) => {
                        // Branch exists and we got its latest commit
                    }
                    Err(e) => {
                        return Err(ProjectError::GitHubError(format!(
                            "Branch '{}' does not exist in repository {}/{} or failed to verify: {}",
                            main_branch, repo_owner, repo_name, e
                        )));
                    }
                }
            }
        }

        // The branch exists; now the root directory on it (issue #1350).
        // This endpoint always makes the project a Git source, so the check
        // applies whatever the project was before.
        {
            let normalized_directory = normalize_project_directory(&directory)?;
            let target = SourceDirectoryTarget {
                connection_id: match git_provider_connection_id {
                    Some(id) if id > 0 => Some(id),
                    Some(_) => None,
                    None => old_connection_id,
                },
                is_public_repo: is_public_repo.unwrap_or(project.is_public_repo),
                git_url: git_url.as_deref().or(project.git_url.as_deref()),
                repo_owner: &repo_owner,
                repo_name: &repo_name,
                branch: &main_branch,
                directory: &normalized_directory,
            };
            if target.differs_from(&project)
                || project.source_type != temps_entities::source_type::SourceType::Git
            {
                self.verify_root_directory_in_repository(Some(project_id), target)
                    .await?;
            }
        }

        // Capture the current preset/config before converting to ActiveModel
        let project_preset = project.preset;
        let existing_preset_config = project.preset_config.clone();
        let existing_pull_only_root_directory = project.pull_only_root_directory;

        // Update the project
        let mut active_project: projects::ActiveModel = project.into();
        active_project.main_branch = Set(main_branch.clone());
        active_project.repo_owner = Set(repo_owner.clone());
        active_project.repo_name = Set(repo_name.clone());
        let normalized_directory = normalize_project_directory(&directory)?;
        active_project.directory = Set(normalized_directory.clone());
        active_project.pull_only_root_directory = Set(pull_only_root_directory_value(
            &normalized_directory,
            pull_only_root_directory,
            existing_pull_only_root_directory,
        ));
        // Configuring a Git repository makes this a Git-source project — this is
        // how a docker_image / static_files project is converted to Git (the
        // reverse conversion goes through `set_source_type`).
        active_project.source_type = Set(temps_entities::source_type::SourceType::Git);

        // Reject invalid settings before any provider-side webhook changes.
        // Re-resolve under the row lock below; this snapshot can become stale.
        resolve_preset_patch(
            project_preset,
            existing_preset_config.as_ref(),
            preset.as_deref(),
            preset_config.as_ref(),
        )?;

        // Determine the effective new connection id and whether we need to handle
        // webhook lifecycle.  Three cases:
        //   1. connection_id provided and > 0 → connecting / changing repo
        //   2. connection_id provided and == 0 → disconnecting
        //   3. connection_id not provided → no change
        let new_connection_id: Option<i32> = match git_provider_connection_id {
            Some(cid) if cid > 0 => {
                active_project.git_provider_connection_id = Set(Some(cid));
                Some(cid)
            }
            Some(_) => {
                // Explicit disconnect (connection_id == 0)
                active_project.git_provider_connection_id = Set(None);
                None
            }
            None => {
                // No change requested — carry existing connection forward.
                old_connection_id
            }
        };

        if let Some(ref url) = git_url {
            // SSRF guard: validate before persisting (Fix #12).
            validate_git_url(url).map_err(|e| ProjectError::InvalidGitUrl {
                url: redact_url_password(url),
                reason: e.to_string(),
            })?;
            active_project.git_url = Set(Some(url.clone()));
        }

        if let Some(is_public) = is_public_repo {
            active_project.is_public_repo = Set(is_public);
        }

        // ── GitLab webhook lifecycle ──────────────────────────────────────────
        //
        // We detect a "repo change" when either the connection or the repo path
        // differs from what was previously stored.  A change triggers:
        //   • Delete the old webhook from GitLab (best-effort, idempotent on 404).
        //   • Install a new webhook on the new repo (GitLab connections only).
        //
        // Failures here are non-fatal: we log warnings and continue so that the
        // project save always succeeds.

        let repo_changed = git_provider_connection_id.is_some()
            || repo_owner != old_repo_owner
            || repo_name != old_repo_name;

        if repo_changed {
            // Step 1: Remove old webhook if the old connection was GitLab.
            if let (Some(old_hook_id), Some(old_conn_id)) =
                (old_gitlab_webhook_id, old_connection_id)
            {
                if let Err(e) = self
                    .delete_gitlab_webhook_for_connection(
                        old_conn_id,
                        &old_repo_owner,
                        &old_repo_name,
                        old_hook_id,
                    )
                    .await
                {
                    warn!(
                        "Failed to remove old GitLab webhook {} for project {}: {}",
                        old_hook_id, project_id, e
                    );
                }
                // Clear stale hook fields unconditionally — even if delete failed
                // (it may already be gone on GitLab's side).
                active_project.gitlab_webhook_id = Set(None);
                active_project.gitlab_webhook_signing_token = Set(None);
            }

            // Step 2: Install a new webhook if the new connection is GitLab.
            if let Some(conn_id) = new_connection_id {
                match self
                    .install_gitlab_webhook_for_connection(
                        project_id,
                        conn_id,
                        &repo_owner,
                        &repo_name,
                    )
                    .await
                {
                    Ok((hook_id, encrypted_token)) => {
                        active_project.gitlab_webhook_id = Set(Some(hook_id as i32));
                        active_project.gitlab_webhook_signing_token = Set(Some(encrypted_token));
                    }
                    Err(e) => {
                        // Non-fatal: the project connects without the webhook.
                        warn!(
                            "Failed to install GitLab webhook for project {}: {}",
                            project_id, e
                        );
                        active_project.gitlab_webhook_id = Set(None);
                        active_project.gitlab_webhook_signing_token = Set(None);
                    }
                }
            }
        } else if git_provider_connection_id == Some(0) {
            // Explicit disconnect: clear webhook state.
            active_project.gitlab_webhook_id = Set(None);
            active_project.gitlab_webhook_signing_token = Set(None);
        }

        // ── Bitbucket Cloud webhook lifecycle ────────────────────────────────
        //
        // Mirrors the GitLab block above: delete the old hook when the repo
        // changes and install a new one on the new repo (best-effort, non-fatal).

        if repo_changed {
            // Step 1: Remove old webhook if the old connection was Bitbucket.
            if let (Some(old_hook_uuid), Some(old_conn_id)) =
                (old_bitbucket_hook_id.as_deref(), old_connection_id)
            {
                if let Err(e) = self
                    .delete_bitbucket_webhook_for_connection(
                        old_conn_id,
                        &old_repo_owner,
                        &old_repo_name,
                        old_hook_uuid,
                    )
                    .await
                {
                    warn!(
                        "Failed to remove old Bitbucket webhook {} for project {}: {}",
                        old_hook_uuid, project_id, e
                    );
                }
                // Clear stale hook fields unconditionally.
                active_project.bitbucket_webhook_hook_id = Set(None);
                active_project.bitbucket_webhook_token = Set(None);
            }

            // Step 2: Install a new webhook if the new connection is Bitbucket.
            if let Some(conn_id) = new_connection_id {
                match self
                    .install_bitbucket_webhook_for_connection(
                        project_id,
                        conn_id,
                        &repo_owner,
                        &repo_name,
                    )
                    .await
                {
                    Ok((hook_uuid, encrypted_token)) => {
                        active_project.bitbucket_webhook_hook_id = Set(Some(hook_uuid));
                        active_project.bitbucket_webhook_token = Set(Some(encrypted_token));
                    }
                    Err(e) => {
                        // Non-fatal: project connects without the webhook.
                        warn!(
                            "Failed to install Bitbucket webhook for project {}: {}",
                            project_id, e
                        );
                        active_project.bitbucket_webhook_hook_id = Set(None);
                        active_project.bitbucket_webhook_token = Set(None);
                    }
                }
            }
        } else if git_provider_connection_id == Some(0) {
            // Explicit disconnect: clear Bitbucket webhook state.
            active_project.bitbucket_webhook_hook_id = Set(None);
            active_project.bitbucket_webhook_token = Set(None);
        }

        // ── GitHub repository webhook lifecycle ─────────────────────────────
        //
        // Mirrors the GitLab block above for non-App GitHub connections:
        // delete the old repository hook when the repo changes and register
        // one on the new repo (best-effort, non-fatal).
        //
        // Unlike `repo_changed`, this compares against the stored values: the
        // console sends `git_provider_connection_id` with every Git settings
        // save (branch, preset, directory), and replacing a working hook on
        // each of those would rotate its secret and, if the new registration
        // failed, silently stop push deploys over an unrelated edit.
        let github_repo_changed = repository_target_changed(
            (old_connection_id, &old_repo_owner, &old_repo_name),
            (new_connection_id, &repo_owner, &repo_name),
        );

        if github_repo_changed {
            if let (Some(old_hook_id), Some(old_conn_id)) =
                (old_github_webhook_id, old_connection_id)
            {
                if let Err(e) = self
                    .delete_github_webhook_for_connection(
                        old_conn_id,
                        &old_repo_owner,
                        &old_repo_name,
                        old_hook_id,
                    )
                    .await
                {
                    warn!(
                        "Failed to remove old GitHub webhook {} for project {}: {}",
                        old_hook_id, project_id, e
                    );
                }
            }
            // Clear stale hook fields unconditionally — even if delete failed
            // (it may already be gone on GitHub's side).
            active_project.github_webhook_id = Set(None);
            active_project.github_webhook_signing_token = Set(None);

            if let Some(conn_id) = new_connection_id {
                match self
                    .install_github_webhook_for_connection(
                        project_id,
                        conn_id,
                        &repo_owner,
                        &repo_name,
                        None,
                    )
                    .await
                {
                    Ok(Some((hook_id, encrypted_secret))) => {
                        active_project.github_webhook_id = Set(Some(hook_id));
                        active_project.github_webhook_signing_token = Set(Some(encrypted_secret));
                    }
                    Ok(None) => {
                        // Not a non-App GitHub connection — nothing to do.
                    }
                    Err(e) => {
                        warn!(
                            "Failed to install GitHub webhook for project {}: {}",
                            project_id, e
                        );
                    }
                }
            }
        } else if git_provider_connection_id == Some(0) {
            // Explicit disconnect: clear GitHub webhook state.
            active_project.github_webhook_id = Set(None);
            active_project.github_webhook_signing_token = Set(None);
        }

        // ── Gitea webhook lifecycle ──────────────────────────────────────────
        //
        // BLOCKER 1 fix: gitea_webhook_signing_token is now written here.
        // There is no stored hook_id for remote deletion in v1 — on repo
        // change we clear the token (orphaning the remote hook) and re-install.
        // On explicit disconnect we clear the token.

        if repo_changed {
            // Clear the old Gitea signing token unconditionally.
            active_project.gitea_webhook_signing_token = Set(None);

            // Install a new webhook if the new connection is Gitea.
            if let Some(conn_id) = new_connection_id {
                match self
                    .install_gitea_webhook_for_connection(
                        project_id,
                        conn_id,
                        &repo_owner,
                        &repo_name,
                    )
                    .await
                {
                    Ok(encrypted_token) => {
                        active_project.gitea_webhook_signing_token = Set(Some(encrypted_token));
                    }
                    Err(e) => {
                        warn!(
                            "Failed to install Gitea webhook for project {}: {}",
                            project_id, e
                        );
                        active_project.gitea_webhook_signing_token = Set(None);
                    }
                }
            }
        } else if git_provider_connection_id == Some(0) {
            // Explicit disconnect: clear Gitea webhook state.
            active_project.gitea_webhook_signing_token = Set(None);
        }

        // ── Generic webhook token lifecycle ─────────────────────────────────
        //
        // BLOCKER 2 fix: generic_webhook_token is now written here.
        // Generic has no remote API to deregister; we simply regenerate.

        if repo_changed {
            // Clear the old token.
            active_project.generic_webhook_token = Set(None);

            if let Some(conn_id) = new_connection_id {
                match self
                    .install_generic_webhook_token(project_id, conn_id)
                    .await
                {
                    Ok(Some(encrypted_token)) => {
                        active_project.generic_webhook_token = Set(Some(encrypted_token));
                    }
                    Ok(None) => {
                        // Not a Generic connection — nothing to do.
                    }
                    Err(e) => {
                        warn!(
                            "Failed to install Generic webhook token for project {}: {}",
                            project_id, e
                        );
                        active_project.generic_webhook_token = Set(None);
                    }
                }
            }
        } else if git_provider_connection_id == Some(0) {
            // Explicit disconnect: clear Generic webhook state.
            active_project.generic_webhook_token = Set(None);
        }

        // Same rollback-together reasoning as `update_project_settings`: publish
        // the reload on the transaction that carries the write, so a failure to
        // signal leaves nothing persisted.
        let txn = self.db.begin().await?;
        let locked_project = projects::Entity::find_by_id(project_id)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("Project {project_id} not found")))?;
        self.guard_granted_project_write(&locked_project.slug, "the source repository", caller)?;
        let previous_public_ports = compose_public_ports(locked_project.preset_config.as_ref());
        if locked_project.project_type == ProjectType::Service {
            return Err(ProjectError::InvalidInput(
                "A service project cannot be converted to a Git source; update its applied template or runtime instead"
                    .to_string(),
            ));
        }
        if let Some(resolved) = resolve_preset_patch(
            locked_project.preset,
            locked_project.preset_config.as_ref(),
            preset.as_deref(),
            preset_config.as_ref(),
        )? {
            apply_resolved_preset(&mut active_project, resolved)?;
        }
        let enable_hosting =
            locked_project.source_type == temps_entities::source_type::SourceType::External;
        let updated_project = active_project.update(&txn).await?;
        if enable_hosting {
            self.activate_hosting_domains(&txn, project_id).await?;
            // Telemetry environments start without a repository. Attach the
            // production environment to the selected branch when hosting is added.
            environments::Entity::update_many()
                .col_expr(
                    environments::Column::Branch,
                    sea_orm::sea_query::Expr::value(Some(main_branch)),
                )
                .filter(environments::Column::ProjectId.eq(project_id))
                .filter(environments::Column::Name.eq("production"))
                .filter(environments::Column::DeletedAt.is_null())
                .exec(&txn)
                .await?;
        }

        let ports_changed =
            previous_public_ports != compose_public_ports(updated_project.preset_config.as_ref());
        if ports_changed {
            // Every *database* write in this method rides `txn`. The provider-side
            // webhook install/removal above does not, and nothing here undoes
            // it — so do not claim a total rollback.
            self.publish_route_reload(
                &txn,
                project_id,
                "No database changes were saved, but any git-provider webhook this request \
                 installed or removed has already been applied at the provider.",
            )
            .await?;
        }
        txn.commit().await?;
        if enable_hosting {
            self.notify_hosting_enabled(project_id).await;
        }
        if ports_changed {
            self.enqueue_route_reload(project_id).await;
        }

        Ok(self.map_written_project(updated_project).await)
    }

    /// Public Compose ports are read directly from `projects.preset_config`
    /// when the proxy builds its route table. Updating the JSON alone leaves
    /// the in-memory table stale, because the project DB trigger deliberately
    /// ignores generic preset-config changes.
    ///
    /// Publishes on the caller's connection rather than reaching for `self.db`,
    /// so a caller writing inside a transaction issues the NOTIFY on that same
    /// transaction. PostgreSQL queues notifications and delivers them at commit,
    /// so a rolled-back update signals nothing and no listener can observe a
    /// reload for a write that never landed.
    ///
    /// The in-process queue job is deliberately *not* sent here, and there is
    /// deliberately no eager fallback to it when the NOTIFY fails. Its
    /// subscriber reloads the route table over its own database connection, so
    /// enqueuing before the transaction commits races that commit: the
    /// subscriber can read the pre-update ports and latch them in with nothing
    /// left to trigger a re-read. On a change that *removes* a public port
    /// that fails open — the withdrawn route stays reachable indefinitely — so
    /// the safe response to an unavailable NOTIFY is to roll the write back and
    /// let the caller retry, not to persist it behind a signal that may never
    /// land. The caller sends the job after committing; see
    /// [`Self::enqueue_route_reload`].
    async fn publish_route_reload<C: ConnectionTrait>(
        &self,
        conn: &C,
        project_id: i32,
        rolled_back_scope: &str,
    ) -> Result<(), ProjectError> {
        let payload = serde_json::json!({
            "action": "UPDATE",
            "project_id": project_id,
            "field": "preset_config.public_ports",
        })
        .to_string();
        conn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT pg_notify('project_route_change', $1)",
            [payload.into()],
        ))
        .await
        .map_err(|database_error| {
            // The driver text can name the connection target or a constraint, so
            // it stays in the operator's logs rather than the HTTP response.
            error!(
                project_id,
                error = %database_error,
                "Failed to publish project route-reload NOTIFY"
            );
            ProjectError::RouteReloadFailed {
                project_id,
                rolled_back_scope: rolled_back_scope.to_string(),
            }
        })?;

        Ok(())
    }

    /// Send the in-process route-reload job. Call only *after* the transaction
    /// carrying the port change has committed, so the subscriber cannot read
    /// pre-commit state.
    ///
    /// Best-effort. NOTIFY is not durable — it reaches only sessions holding a
    /// `LISTEN` at commit time — so a queue failure here is not automatically
    /// harmless. What makes it recoverable is the listener itself, which
    /// reloads the full route table on two independent triggers: after a
    /// reconnect (catching signals missed while it was down) and after an idle
    /// window elapses with no notification (catching a connection that died
    /// without ever reporting an error). The second is what bounds the damage
    /// here — without it, losing both the NOTIFY and this queue send would
    /// leave a withdrawn public port reachable indefinitely. Do not weaken
    /// either trigger while trusting this comment; see
    /// `temps_routes::project_change_listener::IDLE_RECONCILE_INTERVAL`.
    async fn enqueue_route_reload(&self, project_id: i32) {
        if let Err(e) = self
            .queue_service
            .send(Job::ForceRouteReload(ForceRouteReloadJob {
                environment_id: None,
                deployment_id: None,
            }))
            .await
        {
            warn!(
                project_id,
                error = %e,
                "Route reload job could not be enqueued after committing a public-port change; \
                 this process will rely on the delivered NOTIFY"
            );
        }
    }

    /// See [`Self::update_project_deployment_config`] for what
    /// `ceiling_enforcement` means.
    ///
    /// No handler in this crate calls this method today — `ProjectsWrite`
    /// callers reach deployment-config updates through
    /// [`Self::update_project_deployment_config`] instead. It is kept public
    /// and ceiling-enforced because [`ProjectService`] is registered with the
    /// plugin DI system (see `crates/temps-projects/src/plugin.rs`), so an
    /// out-of-tree plugin may call it directly; if you're removing the last
    /// in-tree reference to this method, check for that before deleting it.
    pub async fn update_deployment_settings(
        &self,
        project_id_or_slug: &str,
        settings: UpdateDeploymentSettingsRequest,
        ceiling_enforcement: temps_core::CeilingEnforcement,
    ) -> Result<Project, ProjectError> {
        // Find project by ID or slug
        let project = if let Ok(project_id_int) = project_id_or_slug.parse::<i32>() {
            projects::Entity::find_by_id(project_id_int)
                .one(self.db.as_ref())
                .await?
                .ok_or_else(|| {
                    ProjectError::NotFound(format!("Project with id {} not found", project_id_int))
                })?
        } else {
            projects::Entity::find()
                .filter(projects::Column::Slug.eq(project_id_or_slug))
                .one(self.db.as_ref())
                .await?
                .ok_or_else(|| {
                    ProjectError::NotFound(format!(
                        "Project with slug {} not found",
                        project_id_or_slug
                    ))
                })?
        };

        // Update the project with new settings
        let mut active_project: projects::ActiveModel = project.clone().into();

        // Update deployment config with new resource settings
        let mut deployment_config = project.deployment_config.clone().unwrap_or_default();
        deployment_config.cpu_request = settings.cpu_request;
        deployment_config.cpu_limit = settings.cpu_limit;
        deployment_config.memory_request = settings.memory_request;
        deployment_config.memory_limit = settings.memory_limit;

        if ceiling_enforcement == temps_core::CeilingEnforcement::Enforce {
            let app_settings = self.config_service.get_settings().await.map_err(|e| {
                ProjectError::Other(format!(
                    "Failed to read instance settings to check resource ceilings for project {project_id_or_slug}: {e}"
                ))
            })?;
            Self::enforce_tenant_ceilings(&deployment_config, &app_settings)?;
        }

        active_project.deployment_config = Set(Some(deployment_config));

        let updated_project = active_project.update(self.db.as_ref()).await?;

        // Emit ProjectUpdated job
        let project_updated_job = Job::ProjectUpdated(ProjectUpdatedJob {
            project_id: updated_project.id,
            project_name: updated_project.name.clone(),
        });

        if let Err(e) = self.queue_service.send(project_updated_job).await {
            warn!(
                "Failed to emit ProjectUpdated job for project {}: {}",
                updated_project.id, e
            );
        } else {
            info!(
                "Emitted ProjectUpdated job for project {} (settings update)",
                updated_project.id
            );
        }

        Ok(self.map_written_project(updated_project).await)
    }

    /// Update a project's deployment config.
    ///
    /// `ceiling_enforcement` reflects the caller's `SettingsWrite` permission:
    /// whoever can raise the instance-wide ceilings is by definition allowed to
    /// exceed them, so the check would be theatre for them. Everyone else is
    /// held to `AppSettings.tenant_resource_ceilings`, which is unenforced
    /// until an operator configures it.
    pub async fn update_project_deployment_config(
        &self,
        project_id: i32,
        config: UpdateDeploymentConfigRequest,
        ceiling_enforcement: temps_core::CeilingEnforcement,
        caller: DeployCaller,
    ) -> Result<Project, ProjectError> {
        // Find project by ID or slug
        let project = projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| {
                ProjectError::NotFound(format!("Project with id {} not found", project_id))
            })?;

        // ADR 045: `automatic_deploy` arms the same push-deploy trigger
        // `ProjectService::update_automatic_deploy` guards, and `exposed_port`
        // republishes a *different* port of a host-root-equivalent container
        // on the node's private address -- both change the attack surface of
        // a granted project without going through a deploy at all. A health
        // command is another executable input to the next granted deployment.
        if config.exposed_port.is_some()
            || config.automatic_deploy.is_some()
            || config.health.is_some()
        {
            self.guard_granted_project_write(
                &project.slug,
                "the exposed port, health command or whether pushes deploy automatically",
                caller,
            )?;
        }

        // Get existing deployment config or create default
        let mut deployment_config = project.deployment_config.clone().unwrap_or_default();

        // Update only the fields that are provided
        if let Some(cpu_request) = config.cpu_request {
            deployment_config.cpu_request = Some(cpu_request);
        }
        if let Some(cpu_limit) = config.cpu_limit {
            deployment_config.cpu_limit = Some(cpu_limit);
        }
        if let Some(memory_request) = config.memory_request {
            deployment_config.memory_request = Some(memory_request);
        }
        if let Some(memory_limit) = config.memory_limit {
            deployment_config.memory_limit = Some(memory_limit);
        }
        if let Some(exposed_port) = config.exposed_port {
            deployment_config.exposed_port = exposed_port;
        }
        if let Some(automatic_deploy) = config.automatic_deploy {
            deployment_config.automatic_deploy = Some(automatic_deploy);
        }
        if let Some(performance_metrics_enabled) = config.performance_metrics_enabled {
            deployment_config.performance_metrics_enabled = performance_metrics_enabled;
        }
        if let Some(session_recording_enabled) = config.session_recording_enabled {
            deployment_config.session_recording_enabled = session_recording_enabled;
        }
        if let Some(replicas) = config.replicas {
            deployment_config.replicas = replicas;
        }
        if let Some(security) = config.security {
            deployment_config.security = Some(security);
        }
        // Absent leaves it unset (disabled, and inheritable); an explicit value
        // — including `false` — pins it for every environment that doesn't
        // override it.
        if let Some(cross_architecture_builds) = config.cross_architecture_builds {
            deployment_config.cross_architecture_builds = Some(cross_architecture_builds);
        }
        if let Some(build_location) = config.build_location {
            deployment_config.build_location = Some(build_location);
        }
        if let Some(request_timeout_seconds) = config.request_timeout_seconds {
            deployment_config.request_timeout_seconds = Some(request_timeout_seconds);
        }
        if let Some(sse_idle_timeout_seconds) = config.sse_idle_timeout_seconds {
            deployment_config.sse_idle_timeout_seconds = Some(sse_idle_timeout_seconds);
        }
        if let Some(websocket_idle_timeout_seconds) = config.websocket_idle_timeout_seconds {
            deployment_config.websocket_idle_timeout_seconds = Some(websocket_idle_timeout_seconds);
        }
        if let Some(max_concurrent_connections) = config.max_concurrent_connections {
            deployment_config.max_concurrent_connections = Some(max_concurrent_connections);
        }
        if let Some(health_check_timeout_seconds) = config.health_check_timeout_seconds {
            deployment_config.health_check_timeout_seconds = Some(health_check_timeout_seconds);
        }

        if let Some(health) = config.health {
            deployment_config.health = Some(health);
        }

        // Validate the deployment config
        deployment_config
            .validate()
            .map_err(|e| ProjectError::InvalidInput(format!("Invalid deployment config: {}", e)))?;

        // Checked on the merged config, not on the request, so clearing an
        // override back to "inherit" is judged by what the project will
        // actually run with.
        if ceiling_enforcement == temps_core::CeilingEnforcement::Enforce {
            let app_settings = self.config_service.get_settings().await.map_err(|e| {
                ProjectError::Other(format!(
                    "Failed to read instance settings to check resource ceilings for project {project_id}: {e}"
                ))
            })?;
            Self::enforce_tenant_ceilings(&deployment_config, &app_settings)?;
        }

        // Update the project
        let mut active_project: projects::ActiveModel = project.clone().into();
        active_project.deployment_config = Set(Some(deployment_config));

        let updated_project = active_project.update(self.db.as_ref()).await?;

        // Emit ProjectUpdated job
        let project_updated_job = Job::ProjectUpdated(ProjectUpdatedJob {
            project_id: updated_project.id,
            project_name: updated_project.name.clone(),
        });

        if let Err(e) = self.queue_service.send(project_updated_job).await {
            warn!(
                "Failed to emit ProjectUpdated job for project {}: {}",
                updated_project.id, e
            );
        } else {
            info!(
                "Emitted ProjectUpdated job for project {} (deployment config update)",
                updated_project.id
            );
        }

        Ok(self.map_written_project(updated_project).await)
    }

    // Environment Variables Methods
    pub async fn get_environment_variables(
        &self,
        project_id: i32,
    ) -> Result<Vec<EnvVarWithEnvironments>, ProjectError> {
        self.env_var_service
            .get_environment_variables(project_id)
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))
    }

    pub async fn create_environment_variable(
        &self,
        project_id: i32,
        environment_ids: Vec<i32>,
        key: String,
        value: String,
        is_secret: bool,
    ) -> Result<EnvVarWithEnvironments, ProjectError> {
        self.env_var_service
            .create_environment_variable(project_id, environment_ids, key, value, is_secret)
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))
    }

    pub async fn update_environment_variable(
        &self,
        project_id: i32,
        var_id: i32,
        key: String,
        value: String,
        environment_ids: Vec<i32>,
    ) -> Result<EnvVarWithEnvironments, ProjectError> {
        self.env_var_service
            .update_environment_variable(project_id, var_id, key, value, environment_ids)
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))
    }

    pub async fn delete_environment_variable(
        &self,
        project_id: i32,
        var_id: i32,
    ) -> Result<(), ProjectError> {
        self.env_var_service
            .delete_environment_variable(project_id, var_id)
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))
    }

    pub async fn get_environment_variable_value(
        &self,
        project_id: i32,
        key: &str,
        environment_id: Option<i32>,
    ) -> Result<String, ProjectError> {
        self.env_var_service
            .get_environment_variable_value(project_id, key, environment_id)
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))
    }

    /// Queue an initial deployment job for a newly created project
    async fn queue_initial_deployment_job(
        &self,
        project: &temps_entities::projects::Model,
        _environment: &temps_entities::environments::Model,
    ) -> Result<(), ProjectError> {
        // Fetch the latest commit from the git provider if connection exists
        let commit_sha = if let Some(connection_id) = project.git_provider_connection_id {
            match self
                .git_provider_manager
                .get_branch_latest_commit(
                    connection_id,
                    &project.repo_owner,
                    &project.repo_name,
                    &project.main_branch,
                )
                .await
            {
                Ok(commit) => {
                    info!(
                        "Fetched latest commit for project {}: {} - {}",
                        project.id, commit.sha, commit.message
                    );
                    commit.sha
                }
                Err(e) => {
                    // Don't fail: deploy the branch tip without a known commit.
                    tracing::warn!(
                        "Failed to fetch latest commit for project {}: {}. Deploying the tip \
                         of branch {} instead.",
                        project.id,
                        e,
                        project.main_branch
                    );
                    String::new()
                }
            }
        } else {
            // No provider API to ask (e.g. a public repository). An empty
            // commit deploys the branch tip, and the download job records the
            // commit it checked out. Never store the symbolic "HEAD": the
            // console would show it as the commit, and checkout would treat
            // it as a commit and clone the full history to resolve it.
            String::new()
        };

        // Create a GitPushEvent job to trigger the initial deployment
        // The deployment service's job processor will handle creating the pipeline and deployment
        let git_push_job = temps_core::GitPushEventJob {
            owner: project.repo_owner.clone(),
            repo: project.repo_name.clone(),
            branch: Some(project.main_branch.clone()),
            tag: None, // No tag for initial deployment
            commit: commit_sha.clone(),
            project_id: project.id, // Include project_id
            // Initial deployment is a user-initiated event (project creation),
            // not a git webhook — bypass automatic_deploy.
            manual_trigger: true,
            rollback_from_deployment_id: None,
            // Infer the target from the branch at creation time (the default
            // environment tracks main_branch).
            target_environment_id: None,
            recovery_of_deployment_id: None,
        };

        self.queue_service
            .send(temps_core::Job::GitPushEvent(git_push_job))
            .await
            .map_err(|e| ProjectError::Other(format!("Failed to queue deployment job: {}", e)))?;

        info!(
            "Queued GitPushEvent job for initial deployment of project {} (owner: {}, repo: {}, branch: {}, commit: {})",
            project.id,
            &project.repo_owner,
            &project.repo_name,
            project.main_branch,
            commit_sha
        );

        Ok(())
    }

    /// Trigger a pipeline for a specific project and environment
    pub async fn trigger_pipeline(
        &self,
        project_id: i32,
        environment_id: i32,
        branch: Option<String>,
        tag: Option<String>,
        commit: Option<String>,
    ) -> Result<(i32, i32, Option<String>, Option<String>, Option<String>), ProjectError> {
        self.trigger_pipeline_as(
            project_id,
            environment_id,
            branch,
            tag,
            commit,
            DeployCaller::default(),
            None,
        )
        .await
    }

    /// [`Self::trigger_pipeline`] for a caller whose authority is known.
    ///
    /// ADR 045: deploying a project that holds host Docker access runs the
    /// deployed image and command as host root, so it is admin-only — deploy
    /// permission on the project is not sufficient. Every other project is
    /// unaffected.
    #[allow(clippy::too_many_arguments)]
    pub async fn trigger_pipeline_as(
        &self,
        project_id: i32,
        environment_id: i32,
        branch: Option<String>,
        tag: Option<String>,
        commit: Option<String>,
        caller: DeployCaller,
        github_user_id: Option<i32>,
    ) -> Result<(i32, i32, Option<String>, Option<String>, Option<String>), ProjectError> {
        // Get the project to validate it exists and get repository information
        let project = temps_entities::projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))?
            .ok_or_else(|| ProjectError::NotFound("Project not found".to_string()))?;
        self.guard_granted_project_deploy(&project.slug, caller)?;

        // Validate environment belongs to this project and is not soft-deleted
        let environment = temps_entities::environments::Entity::find_by_id(environment_id)
            .filter(temps_entities::environments::Column::ProjectId.eq(project_id))
            .filter(temps_entities::environments::Column::DeletedAt.is_null())
            .one(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))?
            .ok_or_else(|| {
                ProjectError::NotFound(
                    "Environment not found or doesn't belong to project".to_string(),
                )
            })?;

        // Validate project has repository information
        if project.repo_owner.is_empty() || project.repo_name.is_empty() {
            return Err(ProjectError::InvalidInput(
                "Project must have repository information to trigger pipeline".to_string(),
            ));
        }

        // Use provided branch/commit or fall back to project defaults
        let branch_to_use = branch.unwrap_or(project.main_branch.clone());

        // An empty tag is no tag. A tag names the commit to deploy: it is
        // resolved to that commit below, never ignored in favour of the
        // branch tip.
        let mut tag = tag.filter(|tag| !tag.trim().is_empty());

        // Fetch the latest commit from the branch if no commit was provided
        let commit_to_use = if let Some(commit) = commit {
            commit
        } else if let Some(connection_id) = project.git_provider_connection_id {
            // The ref to resolve: the tag when one was asked for, otherwise
            // the branch. Providers resolve a branch, tag or commit id here.
            let lookup_ref = tag.as_deref().unwrap_or(&branch_to_use);
            // Fetch latest commit from the ref using authenticated git provider
            match self
                .git_provider_manager
                .get_branch_latest_commit(
                    connection_id,
                    &project.repo_owner,
                    &project.repo_name,
                    lookup_ref,
                )
                .await
            {
                Ok(commit_info) => {
                    info!(
                        "Fetched latest commit from ref {}: {} ({})",
                        lookup_ref, commit_info.sha, commit_info.message
                    );
                    commit_info.sha
                }
                Err(e) => {
                    warn!(
                        "Failed to fetch latest commit from ref {}: {}, using placeholder",
                        lookup_ref, e
                    );
                    format!("manual-trigger-{}", chrono::Utc::now().timestamp())
                }
            }
        } else if project.is_public_repo {
            // For public repos without git provider connection, fetch from public API
            let provider_name = if let Some(ref git_url) = project.git_url {
                if git_url.contains("github.com") {
                    "github"
                } else if git_url.contains("gitlab.com") {
                    "gitlab"
                } else {
                    return Err(ProjectError::InvalidInput(format!(
                        "Unknown git provider for public repo URL: {}. Only GitHub and GitLab public repos are supported.",
                        git_url
                    )));
                }
            } else {
                // No git_url, try to infer from repo structure (assume GitHub for public repos)
                "github"
            };

            // Use only the requesting user's GitHub credential. A provider
            // configured by another user cannot be borrowed by this project.
            let token = if provider_name == "github" {
                match github_user_id {
                    Some(user_id) => {
                        self.git_provider_manager
                            .get_valid_github_token_for_user(user_id)
                            .await
                    }
                    None => None,
                }
            } else {
                None
            };
            let provider = PublicRepoProviderFactory::create_with_token(provider_name, token)
                .map_err(|e| {
                    ProjectError::Other(format!(
                        "Failed to create public repo provider for {}: {}",
                        provider_name, e
                    ))
                })?;

            // A shared credential may be able to see private repositories.
            // `is_public_repo` must never turn that credential into a private
            // repository oracle, so verify visibility before reading branches.
            provider
                .get_repository(&project.repo_owner, &project.repo_name)
                .await
                .map_err(|e| match e {
                    PublicRepoError::RateLimitExceeded => ProjectError::PublicRepoRateLimited {
                        project_id,
                        project_slug: project.slug.clone(),
                        provider: provider_name.to_string(),
                    },
                    e => ProjectError::Other(format!(
                        "Failed to verify that repository {}/{} is public: {}",
                        project.repo_owner, project.repo_name, e
                    )),
                })?;

            // The tag when one was asked for, otherwise the branch field,
            // which may hold a tag or a commit id (group repositories and
            // templates pin either). One lookup per kind by name: listing
            // every branch stops at a safety cap and reports existing
            // branches of large repositories as missing.
            let resolved = resolve_public_trigger_ref(
                provider.as_ref(),
                &project.repo_owner,
                &project.repo_name,
                &branch_to_use,
                tag.as_deref(),
            )
            .await
            .map_err(|e| match e {
                PublicTriggerRefError::Provider(PublicRepoError::RateLimitExceeded) => {
                    ProjectError::PublicRepoRateLimited {
                        project_id,
                        project_slug: project.slug.clone(),
                        provider: provider_name.to_string(),
                    }
                }
                PublicTriggerRefError::Provider(e) => ProjectError::Other(format!(
                    "Failed to resolve '{}' in public repo {}/{}: {}. The repository may not exist, be private, or the provider API may be unavailable.",
                    tag.as_deref().unwrap_or(&branch_to_use),
                    project.repo_owner,
                    project.repo_name,
                    e
                )),
                PublicTriggerRefError::Missing(message) => ProjectError::NotFound(message),
            })?;

            info!(
                "Resolved {} '{}' of public repo {}/{} to commit {}",
                resolved.kind.as_str(),
                resolved.name,
                project.repo_owner,
                project.repo_name,
                resolved.commit_sha
            );
            // Keep the tag on the deployment when the ref turned out to be one.
            if resolved.kind == PublicRefKind::Tag && tag.is_none() {
                tag = Some(resolved.name.clone());
            }
            resolved.commit_sha
        } else {
            warn!("No git provider connection found for project, using placeholder commit");
            format!("manual-trigger-{}", chrono::Utc::now().timestamp())
        };

        // Create GitPushEvent job to trigger pipeline
        let git_push_job = temps_core::GitPushEventJob {
            owner: project.repo_owner.clone(),
            repo: project.repo_name.clone(),
            branch: Some(branch_to_use.clone()),
            tag: tag.clone(),
            commit: commit_to_use.clone(),
            project_id, // Include project_id
            // `trigger_pipeline` on the projects service is hit by the
            // "Deploy" button and the CLI — both are user-initiated.
            manual_trigger: true,
            rollback_from_deployment_id: None,
            // The caller explicitly chose this environment — deploy there
            // directly rather than re-inferring the target from the branch
            // (which would fall through to a preview/named-preview env when the
            // environment doesn't have the branch configured).
            target_environment_id: Some(environment_id),
            recovery_of_deployment_id: None,
        };

        // Send the job to the queue
        self.queue_service
            .send(temps_core::Job::GitPushEvent(git_push_job))
            .await
            .map_err(|e| {
                ProjectError::Other(format!("Failed to queue pipeline trigger job: {}", e))
            })?;

        info!(
            "Triggered pipeline for project {} ({}), environment {} ({}), branch: {}",
            project_id, project.name, environment_id, environment.name, branch_to_use
        );

        // Return the details for the response
        Ok((
            project_id,
            environment_id,
            Some(branch_to_use),
            tag,
            Some(commit_to_use),
        ))
    }
}

#[cfg(test)]
mod tests;
