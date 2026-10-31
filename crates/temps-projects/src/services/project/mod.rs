// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

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

// Placeholder functions - these should be implemented properly or imported from other services

/// Optional group membership filter for [`ProjectService::get_projects_paginated_excluding_search`].
#[derive(Debug, Clone, Copy)]
pub enum GroupFilter {
    /// Return only services that are **not** members of any project group.
    Ungrouped,
    /// Return only services that are members of the given project group.
    InGroup(i32),
}

async fn has_ready_deployment(
    db: &temps_database::DbConnection,
    hidden: &[i32],
) -> Result<bool, ProjectError> {
    // last_deployment is stamped when a Git deployment starts, so it is
    // not proof of activation. New completions preserve ready_at; older
    // deployments can still be identified by their successful lifecycle state.
    // Fetch one id, rather than counting or loading deployment history.
    let completed = deployments::Entity::find().filter(
        Condition::any()
            .add(deployments::Column::ReadyAt.is_not_null())
            .add(deployments::Column::State.is_in([
                "completed",
                "deployed",
                "superseded",
                "paused",
            ])),
    );
    let completed = if hidden.is_empty() {
        completed
    } else {
        completed.filter(deployments::Column::ProjectId.is_not_in(hidden.iter().copied()))
    };
    let completed = completed
        .select_only()
        .column(deployments::Column::Id)
        .into_tuple::<i32>()
        .one(db)
        .await
        .map_err(|e| {
            ProjectError::DatabaseConnectionError(format!(
                "Failed to check for a completed deployment in project statistics: {e}"
            ))
        })?
        .is_some();
    Ok(completed)
}

/// The delivery provider a new project starts with, and whether the caller
/// asked for it or it was inherited from the instance-wide default.
///
/// The distinction decides what happens when the provider is not usable: an
/// explicit request fails loudly, an inherited default degrades to "no
/// delivery" so that importers, starter projects and API callers that never
/// mention delivery keep working when an admin's default points at a provider
/// that has since been disconnected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeliveryChoice {
    provider: Option<&'static str>,
    explicit: bool,
}

impl DeliveryChoice {
    fn explicit(provider: Option<&'static str>) -> Self {
        Self {
            provider,
            explicit: true,
        }
    }

    fn inherited(provider: Option<&'static str>) -> Self {
        Self {
            provider,
            explicit: false,
        }
    }
}

fn chosen_delivery_provider(
    explicit: Option<&str>,
    legacy_cloudflare: Option<bool>,
    cloudflare_default: bool,
    bunny_default: bool,
) -> Result<DeliveryChoice, ProjectError> {
    match explicit {
        Some("none") => Ok(DeliveryChoice::explicit(None)),
        Some("cloudflare") => Ok(DeliveryChoice::explicit(Some("cloudflare"))),
        Some("bunny") => Ok(DeliveryChoice::explicit(Some("bunny"))),
        Some(value) => Err(ProjectError::InvalidInput(format!(
            "Unknown delivery provider '{value}'; choose none, cloudflare, or bunny"
        ))),
        // The legacy flag only ever spoke about Cloudflare: `true` asks for
        // it, `false` opts out of it. Opting out of Cloudflare says nothing
        // about Bunny, so a Bunny instance default still applies.
        None => match legacy_cloudflare {
            Some(true) => Ok(DeliveryChoice::explicit(Some("cloudflare"))),
            Some(false) if bunny_default => Ok(DeliveryChoice::inherited(Some("bunny"))),
            Some(false) => Ok(DeliveryChoice::explicit(None)),
            None if cloudflare_default => Ok(DeliveryChoice::inherited(Some("cloudflare"))),
            None if bunny_default => Ok(DeliveryChoice::inherited(Some("bunny"))),
            None => Ok(DeliveryChoice::inherited(None)),
        },
    }
}

fn delivery_provider_label(provider: &str) -> &'static str {
    match provider {
        "cloudflare" => "Cloudflare",
        "bunny" => "Bunny",
        _ => "CDN",
    }
}

/// A project row plus the provider type of the Git connection it is linked to.
///
/// The provider type lives two hops away (`projects` →
/// `git_provider_connections` → `git_providers`), so the read queries LEFT JOIN
/// it in as one extra column rather than paying a second round trip. `LEFT` and
/// `Option` because most projects have no connection at all — a Docker-image or
/// uploaded-source project must still come back from the same query.
#[derive(Debug)]
struct ProjectWithGitProviderType {
    project: projects::Model,
    git_provider_type: Option<String>,
}

impl sea_orm::FromQueryResult for ProjectWithGitProviderType {
    fn from_query_result(result: &sea_orm::QueryResult, pre: &str) -> Result<Self, sea_orm::DbErr> {
        Ok(Self {
            project: projects::Model::from_query_result(result, pre)?,
            git_provider_type: result.try_get(pre, GIT_PROVIDER_TYPE_ALIAS)?,
        })
    }
}

/// Alias for the joined-in `git_providers.provider_type` column. Named rather
/// than bare so it can never collide with a `projects` column.
const GIT_PROVIDER_TYPE_ALIAS: &str = "git_provider_type";

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

/// Normalize and validate a project display name.
///
/// The name is not just a label: it becomes `OTEL_SERVICE_NAME` on the next
/// deployment, and the Compose env-file renderer rejects control characters —
/// so a name accepted here but refused there turns one bad save into every
/// later deploy failing with an error naming a variable the operator never set.
/// Format characters (bidi overrides, zero-width spaces) are rejected too:
/// names are not unique and are rendered next to each other in the dashboard,
/// alerts, and audit records, where an override can make one project display as
/// another.
///
/// Shared by every path that writes `projects.name` so the three cannot drift.
fn validate_project_name(raw: &str) -> Result<String, ProjectError> {
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() || trimmed.chars().count() > 100 {
        return Err(ProjectError::InvalidInput(
            "Project name must contain 1-100 characters".to_string(),
        ));
    }
    if trimmed.chars().any(|c| {
        c.is_control()
            || matches!(c,
                '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}')
    }) {
        return Err(ProjectError::InvalidInput(
            "Project name cannot contain control or text-direction characters".to_string(),
        ));
    }
    Ok(trimmed)
}

fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn escape_like_literal(value: &str) -> String {
    value.chars().fold(String::new(), |mut escaped, character| {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
        escaped
    })
}

fn compose_public_ports(
    config: Option<&temps_entities::preset::PresetConfig>,
) -> Vec<temps_entities::preset::ComposePublicPort> {
    match config {
        Some(temps_entities::preset::PresetConfig::DockerCompose(compose)) => {
            compose.public_ports.clone()
        }
        _ => Vec::new(),
    }
}

// API Response types
#[derive(Debug, Serialize)]
pub struct TemplateResponse {
    pub name: String,
    pub description: String,
    pub image: String,
    pub github: TemplateGithubResponse,
    pub preset: Option<String>,
    pub project_type: String,
    pub services: Option<Vec<String>>,
    pub features: Option<Vec<String>>,
    pub env: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub struct TemplateGithubResponse {
    pub owner: String,
    pub repo: String,
    pub path: Option<String>,
    #[serde(rename = "ref")]
    pub ref_: Option<String>,
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

/// Derive the durable project lifecycle from the resolved runtime preset.
///
/// Several framework presets (Vite, Create React App, Docusaurus and Rsbuild)
/// deploy static output even though their stored preset is not the literal
/// `static` variant. Nixpacks can also switch between server and static based
/// on its provider configuration, so callers must classify the complete
/// resolved preset rather than matching only its enum discriminator.
fn project_type_for_resolved_preset(
    resolved: &temps_presets::StoredPreset,
) -> Result<ProjectType, ProjectError> {
    let runtime_preset =
        temps_presets::get_preset_for_storage(resolved.preset, resolved.config.as_ref())
            .map_err(|error| {
                ProjectError::InvalidInput(format!(
                    "Could not resolve project lifecycle for preset '{}': {error}",
                    resolved.preset.as_str()
                ))
            })?
            .ok_or_else(|| {
                ProjectError::InvalidInput(format!(
                    "Preset '{}' does not provide a deployable runtime",
                    resolved.preset.as_str()
                ))
            })?;

    Ok(match runtime_preset.project_type() {
        temps_presets::ProjectType::Static => ProjectType::Static,
        temps_presets::ProjectType::Server => ProjectType::Server,
    })
}

/// Apply a canonical preset selection to a project update.
fn apply_resolved_preset(
    active: &mut projects::ActiveModel,
    resolved: temps_presets::StoredPreset,
) -> Result<(), ProjectError> {
    if active.project_type.as_ref() != &ProjectType::Service {
        active.project_type = Set(project_type_for_resolved_preset(&resolved)?);
    }
    active.preset = Set(resolved.preset);
    active.preset_config = Set(resolved.config);
    Ok(())
}

/// Preserve discriminator-like fields when a partial config patch omits them.
///
/// An explicit empty Nixpacks provider list still resets to auto, and an
/// explicit Dockerfile variant is still honored. Catalog preset selection is
/// normalized separately by the selected preset's resolver.
fn merge_preset_config(
    existing: Option<&temps_entities::preset::PresetConfig>,
    parsed: temps_entities::preset::PresetConfig,
    config_value: &serde_json::Value,
    preserve_omitted_providers: bool,
) -> temps_entities::preset::PresetConfig {
    use temps_entities::preset::PresetConfig;

    let omits_providers = config_value
        .as_object()
        .map(|map| !map.contains_key("providers"))
        .unwrap_or(true);
    let omits_dockerfile_variant = config_value
        .as_object()
        .map(|map| !map.contains_key("variant"))
        .unwrap_or(true);

    match (existing, parsed) {
        (Some(PresetConfig::Nixpacks(existing_cfg)), PresetConfig::Nixpacks(mut parsed_cfg)) => {
            if preserve_omitted_providers
                && omits_providers
                && parsed_cfg.providers.is_empty()
                && !existing_cfg.providers.is_empty()
            {
                parsed_cfg.providers = existing_cfg.providers.clone();
            }
            PresetConfig::Nixpacks(parsed_cfg)
        }
        (
            Some(PresetConfig::Dockerfile(existing_cfg)),
            PresetConfig::Dockerfile(mut parsed_cfg),
        ) => {
            if omits_dockerfile_variant {
                parsed_cfg.variant = existing_cfg.variant;
            }
            if config_value
                .as_object()
                .map(|map| !map.contains_key("imageRuntime"))
                .unwrap_or(true)
            {
                parsed_cfg.image_runtime = existing_cfg.image_runtime.clone();
            }
            let obj = config_value.as_object();
            let omits = |key: &str| obj.map(|map| !map.contains_key(key)).unwrap_or(true);
            if omits("dockerfilePath") {
                parsed_cfg.dockerfile_path = existing_cfg.dockerfile_path.clone();
            }
            if omits("buildContext") {
                parsed_cfg.build_context = existing_cfg.build_context.clone();
            }
            if omits("target") {
                parsed_cfg.target = existing_cfg.target.clone();
            }
            if omits("command") {
                parsed_cfg.runtime.command = existing_cfg.runtime.command.clone();
            }
            if omits("entrypoint") {
                parsed_cfg.runtime.entrypoint = existing_cfg.runtime.entrypoint.clone();
            }
            PresetConfig::Dockerfile(parsed_cfg)
        }
        (
            Some(PresetConfig::DockerCompose(existing_cfg)),
            PresetConfig::DockerCompose(mut parsed_cfg),
        ) => {
            // A partial PATCH (e.g. the settings-page exclusion toggle sends
            // only `excludedServices`) parses into a config where every
            // omitted field is its zero value, not "leave unchanged" — so
            // without this, a one-field patch would silently wipe
            // composePath/composeOverride/publicPorts/composeServices.
            let obj = config_value.as_object();
            let omits = |key: &str| obj.map(|map| !map.contains_key(key)).unwrap_or(true);
            if omits("composePath") {
                parsed_cfg.compose_path = existing_cfg.compose_path.clone();
            }
            if omits("composeOverride") {
                parsed_cfg.compose_override = existing_cfg.compose_override.clone();
            }
            if omits("publicPorts") {
                parsed_cfg.public_ports = existing_cfg.public_ports.clone();
            }
            if omits("excludedServices") {
                parsed_cfg.excluded_services = existing_cfg.excluded_services.clone();
            }
            if omits("composeServices") {
                parsed_cfg.compose_services = existing_cfg.compose_services.clone();
            }
            if omits("relaxedCapabilityServices") {
                parsed_cfg.relaxed_capability_services =
                    existing_cfg.relaxed_capability_services.clone();
            }
            if omits("unsandboxedServices") {
                parsed_cfg.unsandboxed_services = existing_cfg.unsandboxed_services.clone();
            }
            PresetConfig::DockerCompose(parsed_cfg)
        }
        (_, other) => other,
    }
}

/// Resolve a settings patch against the caller's current project row. Writers
/// must call this under the row lock; a preflight result must never be saved.
fn resolve_preset_patch(
    current_preset: temps_entities::preset::Preset,
    current_config: Option<&temps_entities::preset::PresetConfig>,
    requested_preset: Option<&str>,
    config_value: Option<&serde_json::Value>,
) -> Result<Option<temps_presets::StoredPreset>, ProjectError> {
    if let Some(slug) = requested_preset {
        return resolve_preset_selection(slug, config_value, current_config).map(Some);
    }
    let Some(value) = config_value else {
        return Ok(None);
    };
    let parsed = temps_entities::preset::PresetConfig::parse_for_preset(&current_preset, value)
        .map_err(|error| ProjectError::InvalidInput(format!("Invalid preset config: {error}")))?;
    let merged = merge_preset_config(current_config, parsed, value, true);
    let config = validate_preset_config(current_preset, merged, Some(value))?;
    Ok(Some(temps_presets::StoredPreset {
        preset: current_preset,
        config: Some(config),
    }))
}

fn validate_preset_config(
    preset: temps_entities::preset::Preset,
    config: temps_entities::preset::PresetConfig,
    config_value: Option<&serde_json::Value>,
) -> Result<temps_entities::preset::PresetConfig, ProjectError> {
    temps_presets::validate_preset_config(preset, &config)
        .map_err(|error| ProjectError::InvalidInput(format!("Invalid preset config: {}", error)))?;
    // Only re-validate when this call's patch explicitly touched
    // relaxedCapabilityServices. A value merged forward unchanged from the
    // existing config (e.g. because a later, unrelated patch replaced
    // composeServices and the previously-relaxed service name is no longer
    // in the new snapshot) must not retroactively fail every subsequent
    // save — that would permanently wedge the project's settings until the
    // user manually clears a field they never touched.
    let touches_relaxed_capability_services = config_value
        .and_then(|v| v.as_object())
        .is_some_and(|map| map.contains_key("relaxedCapabilityServices"));
    let touches_unsandboxed_services = config_value
        .and_then(|v| v.as_object())
        .is_some_and(|map| map.contains_key("unsandboxedServices"));
    if touches_relaxed_capability_services || touches_unsandboxed_services {
        if let temps_entities::preset::PresetConfig::DockerCompose(ref cfg) = config {
            validate_relaxed_capability_services(cfg)?;
            validate_unsandboxed_services(cfg)?;
        }
    }
    let touches_public_ports = config_value
        .and_then(|value| value.as_object())
        .is_some_and(|map| map.contains_key("publicPorts"));
    if touches_public_ports {
        if let temps_entities::preset::PresetConfig::DockerCompose(ref cfg) = config {
            validate_compose_public_ports(cfg)?;
        }
    }
    Ok(config)
}

fn validate_compose_public_ports(
    cfg: &temps_entities::preset::DockerComposeConfig,
) -> Result<(), ProjectError> {
    let mut routes = std::collections::HashSet::new();
    for route in &cfg.public_ports {
        if route.service.trim().is_empty() {
            return Err(ProjectError::InvalidInput(
                "Compose public route service cannot be empty".to_string(),
            ));
        }
        if route.port == 0 || route.published == Some(0) {
            return Err(ProjectError::InvalidInput(format!(
                "Compose public route for service '{}' must use ports between 1 and 65535",
                route.service
            )));
        }
        if let Some(path) = route.health_check_path.as_deref() {
            if path.len() > 2048
                || !path.starts_with('/')
                || path.contains('@')
                || path.contains("://")
                || path.chars().any(char::is_control)
            {
                return Err(ProjectError::InvalidInput(format!(
                    "Compose public route health path '{}' for service '{}' must be a safe relative HTTP path starting with '/'",
                    path, route.service
                )));
            }
        }
        if !routes.insert((route.service.as_str(), route.port)) {
            return Err(ProjectError::InvalidInput(format!(
                "Compose service '{}' already has a public URL for port {}",
                route.service, route.port
            )));
        }
        if cfg
            .excluded_services
            .iter()
            .any(|excluded| excluded == &route.service)
        {
            return Err(ProjectError::InvalidInput(format!(
                "Compose service '{}' cannot be both disabled and public",
                route.service
            )));
        }
        if !cfg.compose_services.is_empty()
            && !cfg
                .compose_services
                .iter()
                .any(|service| service.name == route.service)
        {
            return Err(ProjectError::InvalidInput(format!(
                "Compose public route references unknown service '{}'",
                route.service
            )));
        }
    }

    // Additional ports on a service get a `{service}-{port}` hostname label.
    // Reject configs where that label would be claimed by another service, or
    // one of the two URLs would silently shadow the other.
    let labels = temps_entities::preset::compose_public_route_labels(&cfg.public_ports);
    let mut seen_labels = std::collections::HashSet::new();
    for (route, label) in cfg.public_ports.iter().zip(&labels) {
        let collides_with_service = *label != route.service
            && cfg
                .compose_services
                .iter()
                .any(|service| service.name == *label);
        if collides_with_service || !seen_labels.insert(label.as_str()) {
            return Err(ProjectError::InvalidInput(format!(
                "Public URL for port {} of compose service '{}' would use the hostname label '{}', which is already used by another compose service",
                route.port, route.service, label
            )));
        }
    }
    Ok(())
}

/// `relaxed_capability_services` grants a compose service back the Linux
/// capabilities (CHOWN, DAC_OVERRIDE, FOWNER, SETUID, SETGID) many official
/// images' entrypoints need to fix ownership on a data directory and drop
/// from root to a service user at startup — this is not unique to database
/// images (confirmed live: Gitea's own official image hits the identical
/// `chown: ... Operation not permitted` / `su-exec: setgroups: Operation not
/// permitted` failure), so the settings UI offers this toggle for every
/// compose service, not just ones flagged `looks_like_database`. The
/// server-side check mirrors that: any name is accepted as long as it
/// matches a real service in the persisted snapshot, which rejects typos or
/// phantom names without narrowing eligibility to a specific image family.
/// If the snapshot is empty (e.g. before the first deploy has captured one),
/// allow the list through rather than block a legitimate first-time setup,
/// since there is nothing yet to validate against.
fn validate_relaxed_capability_services(
    cfg: &temps_entities::preset::DockerComposeConfig,
) -> Result<(), ProjectError> {
    if cfg.relaxed_capability_services.is_empty() || cfg.compose_services.is_empty() {
        return Ok(());
    }
    for service_name in &cfg.relaxed_capability_services {
        let matches_known_service = cfg.compose_services.iter().any(|s| &s.name == service_name);
        if !matches_known_service {
            return Err(ProjectError::InvalidInput(format!(
                "Cannot grant elevated capabilities to service '{}': it is not a recognized \
                 service in this compose file.",
                service_name
            )));
        }
    }
    Ok(())
}

fn validate_unsandboxed_services(
    cfg: &temps_entities::preset::DockerComposeConfig,
) -> Result<(), ProjectError> {
    if cfg.unsandboxed_services.is_empty() {
        return Ok(());
    }
    if cfg.compose_services.is_empty() {
        return Err(ProjectError::InvalidInput(
            "Cannot disable the Temps sandbox before Compose services have been recognized. Sync the Compose services from the repository first."
                .to_string(),
        ));
    }
    for service_name in &cfg.unsandboxed_services {
        if !cfg.compose_services.iter().any(|s| &s.name == service_name) {
            return Err(ProjectError::InvalidInput(format!(
                "Cannot disable the Temps sandbox for service '{}': it is not a recognized service in this compose file.",
                service_name
            )));
        }
        if cfg.relaxed_capability_services.contains(service_name) {
            return Err(ProjectError::InvalidInput(format!(
                "Service '{}' cannot use both elevated permissions and a disabled sandbox. Remove one of these settings.",
                service_name
            )));
        }
    }
    Ok(())
}

fn normalize_project_directory(directory: &str) -> Result<String, ProjectError> {
    let normalized = directory
        .trim()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string();
    if normalized.is_empty() || normalized == "." {
        return Ok(".".to_string());
    }
    let path = std::path::Path::new(&normalized);
    let has_windows_drive_prefix = normalized.as_bytes().get(1) == Some(&b':');
    if has_windows_drive_prefix
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(ProjectError::InvalidInput(format!(
            "Project directory '{directory}' must be a relative path inside the source root"
        )));
    }
    Ok(normalized.trim_start_matches("./").to_string())
}

/// The source location a write would leave a Git project with, as far as the
/// configuration-time root directory check (issue #1350) needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceDirectoryTarget<'a> {
    connection_id: Option<i32>,
    is_public_repo: bool,
    git_url: Option<&'a str>,
    repo_owner: &'a str,
    repo_name: &'a str,
    branch: &'a str,
    /// Already normalized with [`normalize_project_directory`].
    directory: &'a str,
}

impl SourceDirectoryTarget<'_> {
    /// Whether this write changes anything the check depends on compared with
    /// the stored project. Unchanged sources are not re-checked, so an
    /// unrelated edit (a preset change re-sending the same directory) is never
    /// refused because the repository changed underneath the project.
    fn differs_from(&self, project: &projects::Model) -> bool {
        // The public flag and URL decide whether (and against which
        // provider) a connectionless project can be checked at all.
        self.connection_id != project.git_provider_connection_id
            || self.is_public_repo != project.is_public_repo
            || self.git_url != project.git_url.as_deref()
            || self.repo_owner != project.repo_owner
            || self.repo_name != project.repo_name
            || self.branch != project.main_branch
            || self.directory != normalized_stored_directory(&project.directory)
    }
}

/// A stored directory in the normalized form, for change detection. Rows
/// written before normalization (`"/"`, `"./"`) compare equal to `"."`.
fn normalized_stored_directory(directory: &str) -> String {
    normalize_project_directory(directory).unwrap_or_else(|_| directory.to_string())
}

/// The public provider that can list `git_url`'s repository without
/// credentials, when there is one.
///
/// Only GitHub: its contents API answers unauthenticated requests for public
/// repositories. Deliberately unauthenticated, so the check can never reveal
/// anything about a private repository a shared credential could see.
fn public_directory_provider(git_url: Option<&str>) -> Option<&'static str> {
    let url = url::Url::parse(git_url?.trim()).ok()?;
    if !matches!(url.scheme(), "https" | "http") {
        return None;
    }
    match url.host_str()?.to_ascii_lowercase().as_str() {
        "github.com" | "www.github.com" => Some("github"),
        _ => None,
    }
}

/// Root checkout (`.`) cannot use sparse clone — there is no subdirectory.
fn pull_only_root_directory_value(
    normalized_directory: &str,
    requested: Option<bool>,
    existing: bool,
) -> bool {
    if normalized_directory == "." {
        return false;
    }
    requested.unwrap_or(existing)
}

/// Resolve an explicit catalog selection for create/update.
///
/// Existing config is retained when it belongs to the same canonical preset.
/// Selecting base `nixpacks` is authoritative: omitted providers reset to
/// auto-detection while other Nixpacks settings remain intact.
fn resolve_preset_selection(
    slug: &str,
    config_value: Option<&serde_json::Value>,
    existing: Option<&temps_entities::preset::PresetConfig>,
) -> Result<temps_presets::StoredPreset, ProjectError> {
    use temps_entities::preset::PresetConfig;

    let base_selection = resolve_preset_slug(slug, None)?;
    let compatible_existing =
        existing.filter(|config| config.preset_type() == base_selection.preset);

    let config = match config_value {
        Some(value) => {
            let parsed =
                PresetConfig::parse_for_preset(&base_selection.preset, value).map_err(|error| {
                    ProjectError::InvalidInput(format!("Invalid preset config: {}", error))
                })?;
            Some(merge_preset_config(
                compatible_existing,
                parsed,
                value,
                slug != "nixpacks",
            ))
        }
        None => {
            let mut config = compatible_existing.cloned();
            if slug == "nixpacks" {
                if let Some(PresetConfig::Nixpacks(nixpacks)) = config.as_mut() {
                    nixpacks.providers.clear();
                }
            }
            config
        }
    };

    let resolved = if config.is_some() {
        resolve_preset_slug(slug, config)?
    } else {
        base_selection
    };
    let config = match resolved.config {
        Some(config) => Some(validate_preset_config(
            resolved.preset,
            config,
            config_value,
        )?),
        None => None,
    };
    Ok(temps_presets::StoredPreset {
        preset: resolved.preset,
        config,
    })
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

fn apply_new_template_default_when_unmodified<T: Clone + PartialEq>(
    current: T,
    applied_default: T,
    target_default: T,
) -> T {
    if current == applied_default {
        target_default
    } else {
        current
    }
}

fn template_resource_value(
    template: &temps_core::templates::TemplateServiceSpec,
    select: impl FnOnce(&temps_core::templates::TemplateResources) -> Option<i32>,
) -> Option<i32> {
    template.resources.as_ref().and_then(select)
}

fn missing_required_service_types(required: &[String], linked: &HashSet<String>) -> Vec<String> {
    required
        .iter()
        .filter(|service| {
            !linked.iter().any(|linked_service| {
                temps_core::templates::managed_service_types_compatible(service, linked_service)
            })
        })
        .cloned()
        .collect()
}

/// Keep template upgrades reversible with the current deployment model.
/// Deployments intentionally read the project's current managed-service
/// aliases, so an upgrade may add aliases but cannot remove or remap aliases
/// used by an older release. That keeps a rollback's database credentials
/// compatible without guessing from the live catalog.
fn validate_service_binding_compatibility(
    applied: &temps_core::templates::TemplateServiceSpec,
    target: &temps_core::templates::TemplateServiceSpec,
) -> Result<(), ProjectError> {
    for required_service in &applied.databases {
        if !target.databases.iter().any(|target_service| {
            temps_core::templates::managed_service_types_compatible(
                required_service,
                target_service,
            )
        }) {
            return Err(ProjectError::InvalidInput(format!(
                "Template upgrade cannot remove required managed service '{required_service}'; older deployments require it for safe rollback"
            )));
        }
    }
    for (service_type, applied_bindings) in &applied.database_bindings {
        let target_bindings = target
            .database_bindings
            .iter()
            .find(|(target_service_type, _)| {
                temps_core::templates::managed_service_types_compatible(
                    service_type,
                    target_service_type,
                )
            })
            .map(|(_, bindings)| bindings);
        for (application_variable, service_variable) in applied_bindings {
            let preserved = target_bindings
                .and_then(|bindings| bindings.get(application_variable))
                .is_some_and(|target_variable| target_variable == service_variable);
            if !preserved {
                return Err(ProjectError::InvalidInput(format!(
                    "Template upgrade cannot remove or remap managed-service binding \
                     {service_type}.{application_variable} -> {service_variable}; older \
                     deployments require it for safe rollback"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn production_configured_variable_keys(
    variables: &[env_vars::Model],
    links: &[env_var_environments::Model],
    production_environment_id: i32,
) -> HashSet<String> {
    let linked_ids = links
        .iter()
        .map(|link| link.env_var_id)
        .collect::<HashSet<_>>();
    let production_linked_ids = links
        .iter()
        .filter(|link| link.environment_id == production_environment_id)
        .map(|link| link.env_var_id)
        .collect::<HashSet<_>>();

    variables
        .iter()
        .filter(|variable| {
            variable.environment_id == Some(production_environment_id)
                || production_linked_ids.contains(&variable.id)
                // Legacy/global variables have neither the old direct
                // environment id nor a join-table relation and apply to every
                // environment. Keep their values during upgrades.
                || (variable.environment_id.is_none() && !linked_ids.contains(&variable.id))
        })
        .map(|variable| variable.key.clone())
        .collect()
}

fn applied_service_template_from_model(
    project: &projects::Model,
) -> Result<temps_core::templates::ServiceTemplateInstance, ProjectError> {
    if project.project_type != ProjectType::Service {
        return Err(ProjectError::InvalidInput(format!(
            "Project {} is not a service project",
            project.id
        )));
    }

    if let Some(snapshot) = &project.service_template {
        let instance = serde_json::from_value::<temps_core::templates::ServiceTemplateInstance>(
            snapshot.clone(),
        )
        .map_err(|error| {
            ProjectError::Other(format!(
                "Stored service template for project {} is invalid: {error}",
                project.id
            ))
        })?;
        instance.validate().map_err(|error| {
            ProjectError::Other(format!(
                "Stored service template for project {} is invalid: {error}",
                project.id
            ))
        })?;
        return Ok(instance);
    }

    Err(ProjectError::Other(format!(
        "Service project {} has no applied template release",
        project.id
    )))
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

fn base_project_slug(name: &str) -> String {
    let mut slug = slugify(name);
    // Leave room for the production environment and service namespace within
    // a single DNS label. This must stay identical for planning and creation.
    if slug.chars().count() > 40 {
        slug = slug.chars().take(40).collect::<String>();
        slug.truncate(slug.trim_end_matches('-').len());
    }
    slug
}

/// Resolve the git webhook delivery base from the settings row:
/// `public_ingress_url` when set, `external_url` otherwise, and the local
/// development address when neither is configured.
fn git_webhook_base_url(settings: Option<&temps_core::AppSettings>) -> String {
    settings
        .and_then(temps_core::AppSettings::configured_public_ingress_url)
        .unwrap_or_else(|| "http://localhost:8080".to_string())
}

/// URL of the GitHub repository webhook registered for `project_id`.
///
/// GitHub refuses a second hook with the same URL and events on a repository,
/// so each project's hook needs a URL of its own for two projects to deploy
/// from one repository. The receiver ignores the query: it identifies the
/// project by the secret that signed the delivery.
fn github_repo_webhook_url(base_url: &str, project_id: i32) -> String {
    use temps_git::handlers::github::GITHUB_REPO_WEBHOOK_PATH;

    format!("{base_url}/api{GITHUB_REPO_WEBHOOK_PATH}?project_id={project_id}")
}

/// Whether a provider needs Temps to register a GitHub repository webhook for
/// its projects: a GitHub provider authenticated any way other than as a
/// GitHub App. An App delivers pushes through its own webhook and needs none.
fn github_connection_needs_repo_webhook(provider_type: &str, auth_method: &str) -> bool {
    provider_type == "github" && auth_method != "github_app"
}

/// Whether a project's Git connection or repository differs between `old`
/// and `new` (`(connection_id, repo_owner, repo_name)`), so a repository
/// webhook registered for `old` must be replaced.
fn repository_target_changed(
    old: (Option<i32>, &str, &str),
    new: (Option<i32>, &str, &str),
) -> bool {
    old != new
}

impl ProjectService {
    /// The delivery profile a new project using `provider` would be attached
    /// to, or a sentence naming what is missing and where to configure it.
    ///
    /// The outer `Result` is a database failure; the inner one is "not set
    /// up", which the caller turns into either a 400 or a degraded default.
    async fn usable_delivery_profile(
        &self,
        provider: &'static str,
    ) -> Result<Result<i32, String>, ProjectError> {
        if provider == "cloudflare" {
            let dns_provider = dns_providers::Entity::find()
                .filter(dns_providers::Column::ProviderType.eq("cloudflare"))
                .filter(dns_providers::Column::IsActive.eq(true))
                .one(self.db.as_ref())
                .await?;
            if dns_provider.is_none() {
                return Ok(Err(
                    "Connect an active Cloudflare DNS provider in Settings > DNS Providers".into(),
                ));
            }
        }
        let profile = delivery_profiles::Entity::find()
            .filter(delivery_profiles::Column::ProviderKind.eq(provider))
            .order_by_asc(delivery_profiles::Column::Id)
            .one(self.db.as_ref())
            .await?;
        Ok(profile.map(|profile| profile.id).ok_or_else(|| {
            format!(
                "Create a {} delivery profile in Delivery Profiles",
                delivery_provider_label(provider)
            )
        }))
    }

    pub async fn cloudflare_project_capability(
        &self,
    ) -> Result<crate::handlers::CloudflareProjectCapability, ProjectError> {
        let defaults = settings::Entity::find_by_id(1)
            .one(self.db.as_ref())
            .await?
            .map(|row| temps_core::AppSettings::from_json(row.data))
            .unwrap_or_default();
        let provider = dns_providers::Entity::find()
            .filter(dns_providers::Column::ProviderType.eq("cloudflare"))
            .filter(dns_providers::Column::IsActive.eq(true))
            .one(self.db.as_ref())
            .await?;
        let profile = delivery_profiles::Entity::find()
            .filter(delivery_profiles::Column::ProviderKind.eq("cloudflare"))
            .one(self.db.as_ref())
            .await?;
        let bunny_profile = delivery_profiles::Entity::find()
            .filter(delivery_profiles::Column::ProviderKind.eq("bunny"))
            .one(self.db.as_ref())
            .await?;
        let (reason, setup_path) = if provider.is_none() {
            (
                Some("Connect an active Cloudflare DNS provider".into()),
                Some("/dns-providers".into()),
            )
        } else if profile.is_none() {
            (
                Some("Create a Cloudflare delivery profile".into()),
                Some("/delivery-profiles".into()),
            )
        } else {
            (None, None)
        };
        Ok(crate::handlers::CloudflareProjectCapability {
            configured: provider.is_some() && profile.is_some(),
            default_enabled: defaults.cloudflare_new_projects,
            reason,
            setup_path,
            bunny_configured: bunny_profile.is_some(),
            bunny_default_enabled: defaults.bunny_new_projects,
            bunny_reason: bunny_profile
                .is_none()
                .then(|| "Create a Bunny delivery profile with an active Pull Zone".into()),
        })
    }

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

    /// Override the host Docker socket grant this service answers from.
    ///
    /// Exists for tests: production always uses the process-wide grant read
    /// from the environment at startup, which is deliberately un-settable at
    /// runtime.
    #[cfg(test)]
    fn with_docker_socket_grant(
        mut self,
        grant: temps_core::docker_socket_grant::DockerSocketGrant,
    ) -> Self {
        self.docker_socket_grant = grant;
        self
    }

    /// Refuse a root directory the repository provably does not have at the
    /// configured branch (issue #1350).
    ///
    /// Policy: refuse only on proof of absence (see
    /// `temps_git::services::repository_directory`). A provider that cannot
    /// be reached, rate limits, has no listing API, or a project without a
    /// connection or a listable public URL, saves unverified with a warning:
    /// GitHub being flaky must never stop someone saving a project, and the
    /// deploy-time check in `download_repo` still explains a missing
    /// directory against the actual checkout. Bounded by the provider check's
    /// own timeout, and at most one listing per path component.
    async fn verify_root_directory_in_repository(
        &self,
        project_id: Option<i32>,
        target: SourceDirectoryTarget<'_>,
    ) -> Result<(), ProjectError> {
        use temps_git::RepositoryDirectoryCheck;

        let owner = target.repo_owner.trim();
        let name = target.repo_name.trim();
        let branch = target.branch.trim();
        if target.directory == "."
            || owner.is_empty()
            || name.is_empty()
            || owner == "unknown"
            || name == "unknown"
            || branch.is_empty()
        {
            return Ok(());
        }
        let repository = format!("{owner}/{name}");
        let check = if let Some(connection_id) = target.connection_id.filter(|id| *id > 0) {
            self.git_provider_manager
                .check_repository_directory(connection_id, owner, name, branch, target.directory)
                .await
        } else if target.is_public_repo {
            match public_directory_provider(target.git_url) {
                Some(provider_name) => match PublicRepoProviderFactory::create(provider_name) {
                    Ok(provider) => {
                        temps_git::services::public_repo::check_public_repository_directory(
                            provider.as_ref(),
                            owner,
                            name,
                            branch,
                            target.directory,
                        )
                        .await
                    }
                    Err(error) => RepositoryDirectoryCheck::Unverified {
                        reason: format!("the {provider_name} public API is unavailable: {error}"),
                    },
                },
                None => RepositoryDirectoryCheck::Unverified {
                    reason: "only public GitHub repositories can be listed without a Git \
                             connection"
                        .to_string(),
                },
            }
        } else {
            RepositoryDirectoryCheck::Unverified {
                reason: "the project has no Git connection to list the repository with".to_string(),
            }
        };
        match check {
            RepositoryDirectoryCheck::Present => {
                info!(
                    project_id = ?project_id,
                    %repository,
                    branch,
                    directory = target.directory,
                    "Root directory '{}' exists in {} on branch '{}'",
                    target.directory,
                    repository,
                    branch
                );
                Ok(())
            }
            RepositoryDirectoryCheck::Missing(missing) => {
                warn!(
                    project_id = ?project_id,
                    %repository,
                    branch,
                    directory = target.directory,
                    "Refusing root directory '{}': {}",
                    target.directory,
                    missing.explanation()
                );
                Err(ProjectError::DirectoryNotInRepository {
                    repository,
                    branch: branch.to_string(),
                    directory: target.directory.to_string(),
                    explanation: missing.explanation(),
                })
            }
            RepositoryDirectoryCheck::Unverified { reason } => {
                warn!(
                    project_id = ?project_id,
                    %repository,
                    branch,
                    directory = target.directory,
                    %reason,
                    "Saving root directory '{}' for {} on branch '{}' without verifying it \
                     exists ({}); the first deployment checks it in the checkout",
                    target.directory,
                    repository,
                    branch,
                    reason
                );
                Ok(())
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

    fn check_service_release(
        request: &CreateProjectRequest,
        service_template: &temps_core::templates::ServiceTemplateInstance,
    ) -> Result<(), ProjectError> {
        if service_template.template.kind != temps_core::templates::TemplateKind::App {
            return Err(ProjectError::InvalidInput(format!(
                "Template '{}' is not a service template",
                service_template.slug
            )));
        }
        service_template.validate().map_err(|error| {
            ProjectError::InvalidInput(format!(
                "Invalid service template {}@{}: {error}",
                service_template.slug, service_template.version,
            ))
        })?;
        if request.template_slug.as_deref() != Some(service_template.slug.as_str()) {
            return Err(ProjectError::InvalidInput(format!(
                "Service project provenance must match template '{}'",
                service_template.slug
            )));
        }
        Ok(())
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

    pub async fn get_projects(&self) -> Result<Vec<Project>, ProjectError> {
        let results = Self::with_git_provider_type(projects::Entity::find())
            // Most-recently-deployed first; never-deployed projects (NULL
            // last_deployment) sort last, not first — a NULL under DESC would
            // otherwise be treated as "deployed infinitely recently".
            .order_by_with_nulls(
                projects::Column::LastDeployment,
                sea_orm::Order::Desc,
                sea_orm::sea_query::NullOrdering::Last,
            )
            .order_by_desc(projects::Column::CreatedAt)
            .into_model::<ProjectWithGitProviderType>()
            .all(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))?;

        Ok(results
            .into_iter()
            .map(Self::map_db_project_row_to_project)
            .collect())
    }

    pub async fn get_project(&self, project_id: i32) -> Result<Project, ProjectError> {
        let project_found_db =
            Self::with_git_provider_type(projects::Entity::find_by_id(project_id))
                .into_model::<ProjectWithGitProviderType>()
                .one(self.db.as_ref())
                .await
                .map_err(|e| ProjectError::Other(e.to_string()))?;

        project_found_db
            .map(Self::map_db_project_row_to_project)
            .ok_or(ProjectError::NotFound(format!(
                "project {} not found",
                project_id
            )))
    }

    /// Load the immutable release currently applied to a service project.
    pub async fn get_applied_service_template(
        &self,
        project_id: i32,
    ) -> Result<temps_core::templates::ServiceTemplateInstance, ProjectError> {
        let project = projects::Entity::find_by_id(project_id)
            .filter(projects::Column::IsDeleted.eq(false))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| {
                ProjectError::NotFound(format!("Project with id {project_id} not found"))
            })?;
        applied_service_template_from_model(&project)
    }

    /// Return the managed-service families currently linked to a project.
    pub async fn get_linked_service_types(
        &self,
        project_id: i32,
    ) -> Result<HashSet<String>, ProjectError> {
        let linked = project_services::Entity::find()
            .filter(project_services::Column::ProjectId.eq(project_id))
            .find_also_related(external_services::Entity)
            .all(self.db.as_ref())
            .await?;
        Ok(linked
            .into_iter()
            .filter_map(|(_, service)| service)
            .map(|service| service.service_type.to_ascii_lowercase())
            .collect())
    }

    /// Refuse a move of a slug this host grants host Docker access to
    /// (ADR 045), unless the caller is an instance admin.
    ///
    /// Both directions are guarded, and for different reasons.
    /// [`ReservedSlugChange::Claim`] hands a project host root on every host
    /// that grants the slug. [`ReservedSlugChange::Release`] takes it away
    /// from a project that has it — silently breaking an operator-owned
    /// infrastructure service — and frees the slug for the next project
    /// created, which would inherit the access. Guarding only the claim would
    /// leave "rename it away, then create your own" open to any project
    /// writer.
    ///
    /// `project_id` is `Some` for a rename and `None` for a create; it only
    /// enriches the rejection log.
    ///
    /// Answers from this process's own grant, exactly like the deployer's bind
    /// decision and the capability response, so the three can never disagree.
    /// Two questions, in this order: *may* this caller move the slug, and is
    /// their session *recent enough* to. The order is the security property —
    /// a project writer gets the 403 that names ADR 045, never an MFA prompt
    /// for an operation they still would not be allowed to perform.
    ///
    /// The step-up lives here rather than in the three handlers so it cannot
    /// be forgotten by the fourth: every path that can move a slug already
    /// funnels through this one guard.
    ///
    /// This guarantees the *result* is correct -- no project is ever
    /// created or renamed onto a granted slug without authority and step-up
    /// -- but says nothing about side effects a caller performs *before*
    /// reaching it. A template deploy in fork mode was found doing exactly
    /// that: creating and pushing to an upstream repository before this
    /// guard ever ran, so a 428 here left a real external repository with
    /// no Temps project. [`Self::preflight_guard_reserved_slug`] exists for
    /// callers with an irreversible step between planning a slug and
    /// creating the project -- check it first, then still go through this
    /// guard at creation, since a slug can change between the two calls
    /// (another create/rename racing on the same name) and only this one is
    /// authoritative.
    async fn guard_reserved_slug(
        &self,
        slug: &str,
        caller: &SlugClaimCaller<'_>,
        project_id: Option<i32>,
        change: ReservedSlugChange,
    ) -> Result<(), ProjectError> {
        Self::guard_reserved_slug_against(
            &self.docker_socket_grant,
            slug,
            caller.authority,
            project_id,
            change,
        )?;
        self.require_slug_claim_step_up(slug, caller, project_id, change)
            .await
    }

    /// Check authority and step-up for creating a project with `slug`,
    /// before a caller does anything irreversible on the strength of "this
    /// creation will probably succeed".
    ///
    /// A template deploy in fork mode creates and pushes to a real upstream
    /// repository before it ever calls [`Self::create_project_as`], using a
    /// slug it already planned with [`Self::plan_project_slug`]. Without
    /// this check, a refusal at creation time (403, or a 428 step-up
    /// challenge) arrives *after* that external side effect, leaving a real
    /// repository with no Temps project — and, for the 428 case, a retry
    /// that resends the same repository name fails with "already exists"
    /// instead of completing.
    ///
    /// This does not replace the check inside [`Self::create_project_as`]:
    /// the planned slug can change between this call and creation (a
    /// concurrent create or rename claiming the same name), so only the
    /// creation-time guard is authoritative. This is purely fail-fast for
    /// the common case, at the cost of one extra check.
    pub async fn preflight_guard_reserved_slug(
        &self,
        slug: &str,
        caller: &SlugClaimCaller<'_>,
    ) -> Result<(), ProjectError> {
        self.guard_reserved_slug(slug, caller, None, ReservedSlugChange::Claim)
            .await
    }

    /// The step-up half of [`Self::guard_reserved_slug`], reached only once
    /// the authority check has already said yes.
    ///
    /// Returns `Ok(())` immediately for a slug this host does not reserve, so
    /// an ordinary project create or rename never sees an MFA prompt.
    async fn require_slug_claim_step_up(
        &self,
        slug: &str,
        caller: &SlugClaimCaller<'_>,
        project_id: Option<i32>,
        change: ReservedSlugChange,
    ) -> Result<(), ProjectError> {
        if !temps_core::docker_socket_grant::slug_is_reserved(&self.docker_socket_grant, slug) {
            return Ok(());
        }
        let Some(step_up) = caller.step_up.as_ref() else {
            // Admin authority with nothing to challenge. No production path
            // constructs that — `SlugClaimCaller::from_request` always carries
            // the authorizer, and `project_writer()` can never reach this line
            // because the authority check above refuses it first. Reaching it
            // means a future caller asserted an authority out of band, so
            // refuse rather than silently skip the challenge.
            warn!(
                slug = %slug,
                project_id = ?project_id,
                change = ?change,
                "Refused a reserved-slug change from a caller claiming admin authority with no \
                 session to challenge (ADR 045)"
            );
            return Err(ProjectError::DockerSocketSlugReserved {
                slug: slug.to_string(),
                change,
            });
        };
        // `project_id` is `None` on create, where there is no row yet to name.
        // `0` is the "not yet persisted" sentinel in the action identity; it
        // only ever appears in the challenge's `action` payload and the
        // step-up audit trail, never as a lookup key.
        let project_id = project_id.unwrap_or(0);
        let action = match change {
            ReservedSlugChange::Claim => {
                temps_core::SensitiveAction::ClaimDockerSocketSlug { project_id }
            }
            ReservedSlugChange::Release => {
                temps_core::SensitiveAction::ReleaseDockerSocketSlug { project_id }
            }
        };
        temps_auth::require_sensitive_action(step_up.authorizer, step_up.auth, action)
            .await
            .map_err(|problem| ProjectError::SlugClaimStepUpRequired {
                problem: Box::new(problem),
            })
    }

    /// [`Self::guard_reserved_slug`] with the grant injected.
    ///
    /// Pure, mirroring `temps_deployer::docker::docker_socket_bind_for`: the
    /// process grant is a `OnceLock` frozen at first use, so the rule would
    /// otherwise be untestable without mutating process-global environment
    /// state from parallel tests.
    fn guard_reserved_slug_against(
        grant: &temps_core::docker_socket_grant::DockerSocketGrant,
        slug: &str,
        authority: SlugClaimAuthority,
        project_id: Option<i32>,
        change: ReservedSlugChange,
    ) -> Result<(), ProjectError> {
        if !temps_core::docker_socket_grant::slug_is_reserved(grant, slug)
            || authority.may_claim_reserved_slug()
        {
            return Ok(());
        }
        // No audit event covers "a write that never happened", and this one
        // matters: it is an attempt to move host root between projects. Logged
        // with everything the service knows, so an operator reviewing host
        // logs can see who tried and on which project.
        warn!(
            slug = %slug,
            project_id = ?project_id,
            change = ?change,
            env = temps_core::docker_socket_grant::DOCKER_SOCKET_PROJECTS_ENV,
            "{}",
            match change {
                ReservedSlugChange::Claim =>
                    "Refused a non-admin claim on a slug this host grants host Docker access to \
                     (ADR 045)",
                ReservedSlugChange::Release =>
                    "Refused a non-admin rename away from a slug this host grants host Docker \
                     access to (ADR 045)",
            }
        );
        Err(ProjectError::DockerSocketSlugReserved {
            slug: slug.to_string(),
            change,
        })
    }

    /// Refuse a deployment of a project this control plane declares as
    /// requiring the host Docker socket (ADR 045), unless the caller is an
    /// instance admin (or Temps itself).
    ///
    /// The slug guard decides who may *point a project at* host root; this one
    /// decides who may *run code as* host root, and they are different
    /// principals: a granted project created by an admin carries no
    /// restrictive access grants, so without this any holder of
    /// `ProjectsWrite`/`DeploymentsCreate` on it could deploy their own image
    /// and command into a container that receives `/var/run/docker.sock`.
    fn guard_granted_project_deploy(
        &self,
        project_slug: &str,
        caller: DeployCaller,
    ) -> Result<(), ProjectError> {
        Self::guard_granted_project_deploy_against(&self.docker_socket_grant, project_slug, caller)
    }

    /// [`Self::guard_granted_project_deploy`] with the grant injected, so the
    /// rule is testable without the process-wide `OnceLock`.
    fn guard_granted_project_deploy_against(
        grant: &temps_core::docker_socket_grant::DockerSocketGrant,
        project_slug: &str,
        caller: DeployCaller,
    ) -> Result<(), ProjectError> {
        if !temps_core::docker_socket_grant::deploy_requires_instance_admin(
            grant,
            project_slug,
            caller,
        ) {
            return Ok(());
        }
        warn!(
            slug = %project_slug,
            env = temps_core::docker_socket_grant::DOCKER_SOCKET_PROJECTS_ENV,
            "Refused a non-admin deployment of a project that holds host Docker access (ADR 045)"
        );
        Err(ProjectError::DockerSocketDeployRequiresAdmin {
            slug: project_slug.to_string(),
        })
    }

    /// Refuse a write that decides what a project holding host Docker access
    /// (ADR 045) *runs*, unless the caller is an instance admin.
    ///
    /// The third of the three ADR-045 guards, and the one that closes the
    /// two-step version of the attack the other two refuse in one step.
    /// `guard_reserved_slug` decides who may point a project at host root;
    /// `guard_granted_project_deploy` decides who may run code as host root
    /// right now. Neither covers a project writer who *plants* the input and
    /// lets somebody else's deployment execute it: the persisted runtime
    /// command becomes the default for every deploy that does not override it
    /// (including an admin's), and the source-repository fields decide what a
    /// git push builds and runs — and a git push carries no authenticated
    /// principal to gate at deploy time at all.
    ///
    /// `field` is the caller-facing name of what they tried to change; it
    /// reaches the API message, so it must read as a noun phrase.
    fn guard_granted_project_write(
        &self,
        project_slug: &str,
        field: &str,
        caller: DeployCaller,
    ) -> Result<(), ProjectError> {
        Self::guard_granted_project_write_against(
            &self.docker_socket_grant,
            project_slug,
            field,
            caller,
        )
    }

    /// [`Self::guard_granted_project_write`] with the grant injected, so the
    /// rule is testable without the process-wide `OnceLock`.
    fn guard_granted_project_write_against(
        grant: &temps_core::docker_socket_grant::DockerSocketGrant,
        project_slug: &str,
        field: &str,
        caller: DeployCaller,
    ) -> Result<(), ProjectError> {
        if !temps_core::docker_socket_grant::deploy_requires_instance_admin(
            grant,
            project_slug,
            caller,
        ) {
            return Ok(());
        }
        warn!(
            slug = %project_slug,
            field = %field,
            env = temps_core::docker_socket_grant::DOCKER_SOCKET_PROJECTS_ENV,
            "Refused a non-admin write to a setting that decides what a project holding host \
             Docker access runs (ADR 045)"
        );
        Err(ProjectError::DockerSocketWriteRequiresAdmin {
            slug: project_slug.to_string(),
            field: field.to_string(),
        })
    }

    /// Where this project is granted host Docker access (ADR 045).
    ///
    /// Answers from two sources, both authoritative for the host they describe:
    /// this control plane's own process-wide grant, and each worker node's
    /// advertised grant from its last heartbeat. Nodes of any status are
    /// counted — an offline node still *grants* the project, and reporting
    /// otherwise would send the operator to change a variable that is already
    /// correct. Whether a granted project can be placed *right now* is the
    /// scheduler's answer, not this one.
    ///
    /// Only ever computed for a single project's detail view, never per row of
    /// a list: it is one extra query, and the list endpoints must stay cheap.
    ///
    /// The match is done by Postgres, not by Rust: this runs on every
    /// project-detail request, and loading every `nodes` row (each carrying a
    /// full heartbeat `capacity` blob and a labels blob) to then discard
    /// almost all of them costs memory proportional to the fleet for an answer
    /// that is usually one name or none. JSONB containment on the array the
    /// agent advertises does the same test in the index-able place, and only
    /// the node *name* is selected, since that is all the capability carries.
    pub async fn docker_socket_capability(
        &self,
        slug: &str,
    ) -> Result<DockerSocketCapability, ProjectError> {
        // `capacity->'docker_socket_projects' @> '["<slug>"]'` is true only for
        // an exact element match, which is the same exact-match rule the
        // deployer's bind and the reservation guard use — a near miss must
        // fail closed rather than widen the grant. The slug is bound as a
        // parameter (never interpolated), and a node whose capacity has no
        // such key, or a non-array there, simply does not match — the tolerant
        // behaviour `capacity_grants` already has for an older agent.
        let granting_node_names: Vec<String> = temps_entities::nodes::Entity::find()
            .select_only()
            .column(temps_entities::nodes::Column::Name)
            .filter(sea_orm::sea_query::Expr::cust_with_values(
                format!(
                    "\"capacity\" -> '{}' @> $1::jsonb",
                    temps_core::docker_socket_grant::NODE_CAPACITY_KEY
                ),
                [sea_orm::Value::from(
                    serde_json::Value::from(vec![slug.to_string()]).to_string(),
                )],
            ))
            .order_by_asc(temps_entities::nodes::Column::Name)
            .into_tuple::<String>()
            .all(self.db.as_ref())
            .await?;

        Ok(DockerSocketCapability::evaluate(
            slug,
            // The control plane's declaration is the gate: a node advertising
            // a slug this process never declared is a misconfigured (or
            // hostile) node, not a grant.
            self.docker_socket_grant.declares(slug),
            granting_node_names,
        ))
    }

    pub async fn get_project_by_slug(&self, slug: &str) -> Result<Project, ProjectError> {
        let project_found_db = Self::with_git_provider_type(projects::Entity::find())
            .filter(projects::Column::Slug.eq(slug))
            .into_model::<ProjectWithGitProviderType>()
            .one(self.db.as_ref())
            .await?
            .ok_or(ProjectError::NotFound(format!(
                "project {} not found",
                slug
            )))?;
        let project_found = Self::map_db_project_row_to_project(project_found_db);

        Ok(project_found)
    }

    pub async fn get_projects_by_repo_owner_and_name(
        &self,
        repo_owner: &str,
        repo_name: &str,
    ) -> Result<Vec<Project>, ProjectError> {
        let projects_found_db = Self::with_git_provider_type(projects::Entity::find())
            .filter(projects::Column::RepoOwner.eq(repo_owner))
            .filter(projects::Column::RepoName.eq(repo_name))
            .into_model::<ProjectWithGitProviderType>()
            .all(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))?;

        let projects_found: Vec<Project> = projects_found_db
            .into_iter()
            .map(Self::map_db_project_row_to_project)
            .collect();
        Ok(projects_found)
    }

    pub async fn find_project_by_repo(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Project, ProjectError> {
        let project_found = Self::with_git_provider_type(projects::Entity::find())
            .filter(projects::Column::RepoOwner.eq(owner))
            .filter(projects::Column::RepoName.eq(repo))
            .into_model::<ProjectWithGitProviderType>()
            .one(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::Other(format!("Database error: {}", e)))?;

        match project_found {
            Some(project) => Ok(Self::map_db_project_row_to_project(project)),
            None => Err(ProjectError::NotFound(format!(
                "Project not found for repository {}/{}",
                owner, repo
            ))),
        }
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

    /// Claim the reserved environment names when a telemetry project gains hosting.
    /// Telemetry and environment IDs remain unchanged.
    async fn activate_hosting_domains(
        &self,
        txn: &DatabaseTransaction,
        project_id: i32,
    ) -> Result<(), ProjectError> {
        let environments = environments::Entity::find()
            .filter(environments::Column::ProjectId.eq(project_id))
            .filter(environments::Column::DeletedAt.is_null())
            .order_by_asc(environments::Column::Id)
            .all(txn)
            .await?;
        for environment in environments {
            self.environment_service
                .reconcile_managed_domain(
                    txn,
                    &environment,
                    temps_entities::source_type::SourceType::DockerImage,
                    None,
                )
                .await.map_err(|error| ProjectError::DatabaseError {
                reason: format!("Could not enable hosting for project {project_id}, environment {}: domain '{}' could not be registered: {error}", environment.id, environment.subdomain),
            })?;
        }
        Ok(())
    }

    async fn notify_hosting_enabled(&self, project_id: i32) {
        match self.environment_service.get_environments(project_id).await {
            Ok(environments) => {
                for environment in environments {
                    let job = Job::EnvironmentCreated(temps_core::EnvironmentCreatedJob {
                        project_id,
                        environment_id: environment.id,
                        environment_name: environment.name,
                        subdomain: environment.subdomain,
                    });
                    if let Err(error) = self.queue_service.send(job).await {
                        tracing::warn!(project_id, %error, "Could not initialize hosting monitor");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(project_id, %error, "Could not load environments to initialize hosting monitors")
            }
        }
    }

    /// Authorize source changes using the slug protected by this transaction.
    /// Each independently committed settings write must take this lock itself.
    async fn lock_project_for_source_write(
        &self,
        txn: &DatabaseTransaction,
        project_id: i32,
        field: &str,
        caller: DeployCaller,
    ) -> Result<projects::Model, ProjectError> {
        let project = projects::Entity::find_by_id(project_id)
            .lock_exclusive()
            .one(txn)
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("Project {project_id} not found")))?;
        self.guard_granted_project_write(&project.slug, field, caller)?;
        Ok(project)
    }

    /// Change a project's source type to a Git-less type (docker_image /
    /// static_files / manual). Switching TO Git is rejected here because that
    /// needs repo + provider-connection config — it goes through
    /// [`Self::update_git_settings`] instead (which sets `source_type = Git`).
    pub async fn set_source_type(
        &self,
        project_id: i32,
        source_type: temps_entities::source_type::SourceType,
        caller: DeployCaller,
    ) -> Result<Project, ProjectError> {
        use temps_entities::source_type::SourceType;
        let project = projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await?
            .ok_or(ProjectError::NotFound(format!(
                "project {} not found",
                project_id
            )))?;
        if project.project_type == ProjectType::Service {
            return Err(ProjectError::InvalidInput(
                "A service project's source type is fixed by its applied template".to_string(),
            ));
        }
        // ADR 045, defense in depth: `source_type` decides which deploy
        // pipeline runs, and every image/static/drop deploy path already
        // gates on `guard_deploy` and `DeployImageJobBuilder::build`
        // independently of this flag -- so this is not currently a way
        // around those gates. Gated anyway, unconditionally, since this
        // method has exactly one caller-facing effect and the alternative is
        // trusting that stays true as the deploy pipelines evolve.
        self.guard_granted_project_write(&project.slug, "the source type", caller)?;

        // Switching to Git is a direct flip only when a repository is already
        // configured (repo owner + name). A project can carry git info without
        // being Git-typed — that's a one-click switch. Otherwise the user must
        // set a repository up first (via Git settings).
        if matches!(source_type, SourceType::Git) {
            let has_repo = !project.repo_owner.trim().is_empty()
                && !project.repo_name.trim().is_empty()
                && project.repo_owner != "unknown"
                && project.repo_name != "unknown";
            if !has_repo {
                return Err(ProjectError::InvalidInput(
                    "To switch to a Git source, configure a repository in Git settings \
                     (a provider connection, repository, and branch are required)."
                        .to_string(),
                ));
            }
        }

        if source_type == SourceType::External {
            return Err(ProjectError::InvalidInput(format!(
                "Project {project_id} cannot switch to monitoring-only through a source change. Create a monitoring project instead; disabling hosting here could suppress deployment monitoring."
            )));
        }
        let mut active_project: projects::ActiveModel = project.into();
        active_project.source_type = Set(source_type);
        active_project.updated_at = Set(chrono::Utc::now());
        let txn = self.db.begin().await?;
        let locked_project = projects::Entity::find_by_id(project_id)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("Project {project_id} not found")))?;
        self.guard_granted_project_write(&locked_project.slug, "the source type", caller)?;
        let enable_hosting = locked_project.source_type == SourceType::External;
        let updated = active_project.update(&txn).await?;
        if enable_hosting {
            self.activate_hosting_domains(&txn, project_id).await?;
        }
        txn.commit().await?;
        if enable_hosting {
            self.notify_hosting_enabled(project_id).await;
        }

        // Deploy routing / behavior keys off source_type — notify consumers
        // before anything else awaits, so a cancelled request can only cost
        // the response, never the notification.
        if let Err(e) = self
            .queue_service
            .send(Job::ProjectUpdated(ProjectUpdatedJob {
                project_id: updated.id,
                project_name: updated.name.clone(),
            }))
            .await
        {
            warn!(
                "Failed to emit ProjectUpdated after source-type change for {}: {}",
                updated.id, e
            );
        }
        Ok(self.map_written_project(updated).await)
    }

    /// Toggle whether the project accepts deployments from a source other than
    /// its configured `source_type`.
    ///
    /// This deliberately does NOT touch `source_type`: the project keeps its
    /// primary source (and, for a Git project, its repository, webhook-driven
    /// auto-deploy and rollback rebuild-from-source), and simply gains the
    /// ability to also take an uploaded source archive via `drop`.
    pub async fn set_allow_alternate_sources(
        &self,
        project_id: i32,
        allow: bool,
        caller: DeployCaller,
    ) -> Result<Project, ProjectError> {
        let project = projects::Entity::find_by_id(project_id)
            .filter(projects::Column::IsDeleted.eq(false))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("project {} not found", project_id)))?;

        // ADR 045, defense in depth: accepting an alternate source (a
        // dropped archive) is not by itself a deploy -- `SourceDropService`
        // plans with the fail-closed `DeployCaller::default()` regardless of
        // this flag -- but it does widen what can trigger one, so it is
        // gated the same way as `set_source_type`.
        self.guard_granted_project_write(
            &project.slug,
            "whether alternate sources are accepted",
            caller,
        )?;

        let mut active_project: projects::ActiveModel = project.into();
        active_project.allow_alternate_sources = Set(Some(allow));
        active_project.updated_at = Set(chrono::Utc::now());
        let updated = active_project.update(self.db.as_ref()).await?;
        Ok(self.map_written_project(updated).await)
    }

    /// Mark whether Temps runs the project's app (`hosted`) or only observes
    /// an app running elsewhere (`external`). An external project is never
    /// deployed; deployments already running keep running until stopped.
    pub async fn set_hosting(
        &self,
        project_id: i32,
        hosting: temps_entities::types::Hosting,
        caller: DeployCaller,
    ) -> Result<Project, ProjectError> {
        let project = projects::Entity::find_by_id(project_id)
            .filter(projects::Column::IsDeleted.eq(false))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("project {} not found", project_id)))?;
        if project.hosting == hosting {
            return Ok(self.map_written_project(project).await);
        }
        // Switching back to hosted re-enables deploys, so it is gated like
        // the other settings that widen what can deploy the project.
        if hosting == temps_entities::types::Hosting::Hosted {
            self.guard_granted_project_write(&project.slug, "whether Temps hosts it", caller)?;
        }
        let mut active_project: projects::ActiveModel = project.into();
        active_project.hosting = Set(hosting);
        active_project.updated_at = Set(chrono::Utc::now());
        let updated = active_project.update(self.db.as_ref()).await?;
        Ok(self.map_written_project(updated).await)
    }

    /// Fail with [`ProjectError::DeliveryBindingsExist`] when the project
    /// still has CDN delivery bindings, naming the affected hostnames.
    pub async fn ensure_no_delivery_bindings(&self, project_id: i32) -> Result<(), ProjectError> {
        Self::refuse_delivery_bindings(self.db.as_ref(), project_id).await
    }

    /// [`Self::ensure_no_delivery_bindings`] on `connection`, so a caller
    /// holding the project row lock counts bindings inside its transaction.
    async fn refuse_delivery_bindings<C: ConnectionTrait>(
        connection: &C,
        project_id: i32,
    ) -> Result<(), ProjectError> {
        let bindings: Vec<(i32, String)> = domain_delivery_bindings::Entity::find()
            .filter(domain_delivery_bindings::Column::ProjectId.eq(project_id))
            .select_only()
            .column(domain_delivery_bindings::Column::Id)
            .column(domain_delivery_bindings::Column::Hostname)
            .order_by_asc(domain_delivery_bindings::Column::Hostname)
            .into_tuple()
            .all(connection)
            .await?;
        if bindings.is_empty() {
            return Ok(());
        }
        let (binding_ids, hostnames): (Vec<i32>, Vec<String>) = bindings.into_iter().unzip();
        Err(ProjectError::DeliveryBindingsExist {
            project_id,
            binding_count: hostnames.len(),
            hostnames,
            binding_ids,
        })
    }

    /// Persist deletion intent before cancelling workflows or touching Docker.
    /// Deployment workers reject projects with this fence, closing the window
    /// where a new container could appear after the cleanup snapshot.
    ///
    /// The project row is locked `FOR UPDATE`, delivery bindings are counted
    /// and the fence is written in one transaction. A domain delivery
    /// reservation share-locks the same row and refuses a fenced project, so
    /// either it commits first and its binding is seen here, or it waits for
    /// this fence and refuses. A binding can therefore never appear behind the
    /// fence, where it would block the final delete while its cleanup refuses
    /// the deleted project. Retrying an already-fenced project is a no-op.
    pub async fn begin_project_deletion(&self, project_id: i32) -> Result<(), ProjectError> {
        let txn = self.db.begin().await?;
        let outcome = Self::fence_locked_project(&txn, project_id).await;
        // End the transaction before returning, refusal included, so the row
        // lock is released when the caller sees the result rather than when
        // the pool gets round to rolling back a dropped transaction.
        match outcome {
            Ok(fenced) => {
                txn.commit().await?;
                if fenced {
                    info!(project_id, "Marked project for deletion");
                }
                Ok(())
            }
            Err(error) => {
                if let Err(rollback_error) = txn.rollback().await {
                    error!(
                        "Failed to roll back the deletion fence of project {} after it failed with '{}': {}",
                        project_id, error, rollback_error
                    );
                }
                Err(error)
            }
        }
    }

    /// The checks and the fence of [`Self::begin_project_deletion`], on
    /// `txn`. Returns whether this call wrote the fence: `false` when the
    /// project was already fenced.
    async fn fence_locked_project(
        txn: &DatabaseTransaction,
        project_id: i32,
    ) -> Result<bool, ProjectError> {
        let project = projects::Entity::find_by_id(project_id)
            .lock_exclusive()
            .one(txn)
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("project {} not found", project_id)))?;
        // Refuse before fencing or touching containers: a delivery binding
        // owns DNS records and CDN hostnames that only the delivery service
        // can clean up, and its foreign key would block the final delete.
        Self::refuse_delivery_bindings(txn, project_id).await?;
        if project.is_deleted {
            return Ok(false);
        }

        let mut active: projects::ActiveModel = project.into();
        active.is_deleted = Set(true);
        active.deleted_at = Set(Some(chrono::Utc::now()));
        active.updated_at = Set(chrono::Utc::now());
        active.update(txn).await?;
        Ok(true)
    }

    pub async fn delete_project(
        &self,
        project_id: i32,
        project_name: &str,
    ) -> Result<(), ProjectError> {
        self.ensure_no_delivery_bindings(project_id).await?;

        // Fetch environments before deletion to emit cleanup jobs.
        // We only need id, name, and project_id — use select_only to avoid loading full models.
        let environments_to_delete: Vec<(i32, String, i32)> =
            temps_entities::environments::Entity::find()
                .filter(temps_entities::environments::Column::ProjectId.eq(project_id))
                .select_only()
                .column(temps_entities::environments::Column::Id)
                .column(temps_entities::environments::Column::Name)
                .column(temps_entities::environments::Column::ProjectId)
                .into_tuple()
                .all(self.db.as_ref())
                .await
                .map_err(|e| ProjectError::Other(e.to_string()))?;

        // Emit EnvironmentDeleted jobs before deletion so subscribers can clean up
        for (env_id, env_name, env_project_id) in &environments_to_delete {
            let env_deleted_job = Job::EnvironmentDeleted(temps_core::EnvironmentDeletedJob {
                environment_id: *env_id,
                environment_name: env_name.clone(),
                project_id: *env_project_id,
            });

            if let Err(e) = self.queue_service.send(env_deleted_job).await {
                warn!(
                    "Failed to emit EnvironmentDeleted job for environment {}: {}",
                    env_id, e
                );
            }
        }

        // Delete the project row. Control-plane data (deployments, environments,
        // domains, crons, env_vars, services, etc.) goes with it through
        // ON DELETE CASCADE. The high-volume telemetry tables hold no foreign
        // key to the project, so they are not part of this statement: their
        // rows are purged in the background, from the purge row committed in
        // the same transaction, so the project can't disappear without one.
        let txn = self.db.begin().await?;
        let deleted = temps_entities::projects::Entity::delete_by_id(project_id)
            .exec(&txn)
            .await?;
        if deleted.rows_affected > 0 {
            super::data_purge::ProjectDataPurger::enqueue(&txn, project_id, project_name).await?;
        }
        txn.commit().await?;

        // Emit ProjectDeleted job for async cleanup (e.g. status monitors)
        let project_deleted_job = Job::ProjectDeleted(ProjectDeletedJob {
            project_id,
            project_name: project_name.to_string(),
        });

        if let Err(e) = self.queue_service.send(project_deleted_job).await {
            warn!(
                "Failed to emit ProjectDeleted job for project {}: {}",
                project_id, e
            );
        }

        info!(
            "Project {} and all related data deleted successfully",
            project_id
        );

        Ok(())
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

    /// Take the row lock and report the rename `new_name` would perform against
    /// the currently-stored name, or `None` when it matches and no write is
    /// needed.
    ///
    /// Both ends of the transition come from the locked read, so the pair always
    /// describes one real change. Callers must perform the write on the same
    /// `conn`: reading here and writing outside this transaction would let a
    /// concurrent rename land in between and make the reported transition
    /// fiction.
    /// Takes a transaction rather than a generic connection on purpose: passing
    /// an autocommit handle would compile and silently discard the `FOR UPDATE`
    /// guarantee this whole design rests on, so the type makes that unrepresentable.
    async fn locked_rename(
        &self,
        txn: &DatabaseTransaction,
        project_id: i32,
        new_name: &str,
    ) -> Result<Option<ProjectRename>, ProjectError> {
        let locked = projects::Entity::find_by_id(project_id)
            .lock_exclusive()
            .one(txn)
            .await?
            .ok_or(ProjectError::NotFound(format!(
                "Project {} not found",
                project_id
            )))?;

        if locked.name.to_lowercase() != new_name.to_lowercase() {
            self.lock_project_name_available(txn, new_name, Some(project_id))
                .await?;
        }
        Ok(if locked.name == new_name {
            None
        } else {
            Some(ProjectRename {
                from: locked.name,
                to: new_name.to_string(),
            })
        })
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

    /// Resolve whether the given connection points to a GitLab provider.
    /// Returns `(base_url, access_token, auth_method)` for GitLab connections;
    /// `Err` for all others.
    async fn resolve_gitlab_connection(
        &self,
        connection_id: i32,
    ) -> Result<(String, String, String), String> {
        use temps_entities::{git_provider_connections, git_providers};

        let connection = git_provider_connections::Entity::find_by_id(connection_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error: {}", e))?
            .ok_or_else(|| format!("Connection {} not found", connection_id))?;

        let provider = git_providers::Entity::find_by_id(connection.provider_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error: {}", e))?
            .ok_or_else(|| format!("Provider {} not found", connection.provider_id))?;

        // Only handle GitLab providers.
        if provider.provider_type != "gitlab" {
            return Err(format!(
                "Provider {} is not a GitLab provider (type: {})",
                provider.id, provider.provider_type
            ));
        }

        let base_url = provider
            .base_url
            .unwrap_or_else(|| "https://gitlab.com".to_string());

        let auth_method = provider.auth_method.clone();

        let access_token = self
            .git_provider_manager
            .get_connection_token(connection_id)
            .await
            .map_err(|e| {
                format!(
                    "Failed to get access token for connection {}: {}",
                    connection_id, e
                )
            })?;

        Ok((base_url, access_token, auth_method))
    }

    /// Base URL a git provider is told to deliver webhooks to, with no
    /// trailing slash.
    ///
    /// The provider calls from its own infrastructure, so this is the public
    /// ingress URL (ADR-051) and not necessarily where operators reach the
    /// console. An unreadable settings row falls back like an unconfigured
    /// one: a failed read must not abort a project's git setup.
    async fn git_webhook_base_url(&self) -> String {
        let settings = self.config_service.get_settings().await.ok();
        git_webhook_base_url(settings.as_ref())
    }

    /// Install a GitLab webhook for the given project/connection.
    /// Returns `(hook_id, encrypted_signing_token)` on success.
    async fn install_gitlab_webhook_for_connection(
        &self,
        project_id: i32,
        connection_id: i32,
        owner: &str,
        repo: &str,
    ) -> Result<(i64, String), String> {
        use temps_git::services::gitlab_webhook::{
            generate_signing_token, GitLabWebhookClient, WebhookAuthMethod,
        };

        let (base_url, access_token, auth_method_str) =
            match self.resolve_gitlab_connection(connection_id).await {
                Ok(triple) => triple,
                // Not a GitLab provider — skip silently.
                Err(e) => return Err(e),
            };

        let client = GitLabWebhookClient::new(
            base_url,
            access_token,
            WebhookAuthMethod::from_str(&auth_method_str),
        );

        // Pre-flight: verify the user has >= Maintainer (40) access.
        let access_level = client
            .get_project_access_level(owner, repo)
            .await
            .map_err(|e| format!("Could not check permissions for {}/{}: {}", owner, repo, e))?;

        if access_level < 40 {
            return Err(format!(
                "Insufficient GitLab permissions for {}/{}: access_level={} (need >= 40 Maintainer)",
                owner, repo, access_level
            ));
        }

        // Resolve the webhook URL from config.
        let webhook_url = format!(
            "{}/api/webhook/git/gitlab/events",
            self.git_webhook_base_url().await
        );

        // Generate a random 32-byte signing token.
        let signing_token = generate_signing_token()
            .map_err(|error| format!("Failed to generate GitLab webhook token: {error}"))?;

        let hook_id = client
            .install_webhook(owner, repo, &webhook_url, &signing_token)
            .await
            .map_err(|e| {
                format!(
                    "Failed to install webhook for project {}: {}",
                    project_id, e
                )
            })?;

        // Encrypt the token before storing.
        let encrypted = self
            .encryption_service
            .encrypt_string(&signing_token)
            .map_err(|e| format!("Failed to encrypt signing token: {}", e))?;

        info!(
            "Installed GitLab webhook {} for project {} ({}/{})",
            hook_id, project_id, owner, repo
        );

        Ok((hook_id, encrypted))
    }

    /// Remove a GitLab webhook.  Best-effort; 404 is treated as success.
    async fn delete_gitlab_webhook_for_connection(
        &self,
        connection_id: i32,
        owner: &str,
        repo: &str,
        hook_id: i32,
    ) -> Result<(), String> {
        use temps_git::services::gitlab_webhook::{GitLabWebhookClient, WebhookAuthMethod};

        let (base_url, access_token, auth_method_str) =
            match self.resolve_gitlab_connection(connection_id).await {
                Ok(triple) => triple,
                // Not a GitLab connection — nothing to remove.
                Err(_) => return Ok(()),
            };

        let client = GitLabWebhookClient::new(
            base_url,
            access_token,
            WebhookAuthMethod::from_str(&auth_method_str),
        );
        client
            .delete_webhook(owner, repo, hook_id as i64)
            .await
            .map_err(|e| format!("GitLab delete webhook error: {}", e))
    }

    /// Reinstall (or install for the first time) a GitLab webhook for a project.
    /// Called by `POST /projects/{id}/gitlab/reinstall-webhook`.
    pub async fn reinstall_gitlab_webhook(&self, project_id: i32) -> Result<i32, ProjectError> {
        let project = projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await?
            .ok_or(ProjectError::NotFound(format!(
                "Project {} not found",
                project_id
            )))?;

        let connection_id = project.git_provider_connection_id.ok_or_else(|| {
            ProjectError::Other(format!(
                "Project {} has no git provider connection",
                project_id
            ))
        })?;

        let owner = project.repo_owner.clone();
        let repo = project.repo_name.clone();

        // Best-effort: remove the old webhook first.
        if let Some(old_hook_id) = project.gitlab_webhook_id {
            if let Err(e) = self
                .delete_gitlab_webhook_for_connection(connection_id, &owner, &repo, old_hook_id)
                .await
            {
                warn!(
                    "Failed to remove old GitLab webhook {} during reinstall for project {}: {}",
                    old_hook_id, project_id, e
                );
            }
        }

        let (hook_id, encrypted_token) = self
            .install_gitlab_webhook_for_connection(project_id, connection_id, &owner, &repo)
            .await
            .map_err(ProjectError::Other)?;

        // Persist the new hook id + token.
        let mut active_project: projects::ActiveModel = project.into();
        active_project.gitlab_webhook_id = Set(Some(hook_id as i32));
        active_project.gitlab_webhook_signing_token = Set(Some(encrypted_token));
        active_project.update(self.db.as_ref()).await?;

        info!(
            "Reinstalled GitLab webhook {} for project {}",
            hook_id, project_id
        );

        Ok(hook_id as i32)
    }

    // ── Bitbucket Cloud webhook lifecycle ────────────────────────────────────
    //
    // Mirrors the GitLab pattern.  The delivery URL embeds a secret token in
    // the path (`…/bitbucket/events/{token}`); there is no HMAC body signing.
    // The token is generated once per project, stored encrypted, and persisted
    // together with the Bitbucket hook UUID so we can `DELETE` it on disconnect.
    //
    // Failures are always non-fatal: the project connects without the webhook
    // and the operator can re-try via the Temps UI or re-connect the repo.

    /// Resolve whether `connection_id` points to a Bitbucket provider.
    ///
    /// Returns `(access_token, auth_method_str)` for Bitbucket connections;
    /// `Err(String)` for all others (caller silently skips).
    async fn resolve_bitbucket_connection(
        &self,
        connection_id: i32,
    ) -> Result<(String, String), String> {
        use temps_entities::{git_provider_connections, git_providers};

        let connection = git_provider_connections::Entity::find_by_id(connection_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error fetching connection {connection_id}: {e}"))?
            .ok_or_else(|| format!("Connection {connection_id} not found"))?;

        let provider = git_providers::Entity::find_by_id(connection.provider_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error fetching provider {}: {e}", connection.provider_id))?
            .ok_or_else(|| format!("Provider {} not found", connection.provider_id))?;

        if provider.provider_type != "bitbucket" {
            return Err(format!(
                "Provider {} is not a Bitbucket provider (type: {})",
                provider.id, provider.provider_type
            ));
        }

        let auth_method = provider.auth_method.clone();

        let access_token = self
            .git_provider_manager
            .get_connection_token(connection_id)
            .await
            .map_err(|e| {
                format!("Failed to get access token for Bitbucket connection {connection_id}: {e}")
            })?;

        Ok((access_token, auth_method))
    }

    /// Install a Bitbucket Cloud webhook for `project_id` / `connection_id`.
    ///
    /// Steps:
    /// 1. Generate a fresh `bitbucket_webhook_token` via `OsRng`.
    /// 2. Build the delivery URL: `{public ingress URL}/api/webhook/git/bitbucket/events/{token}`.
    /// 3. Call `POST /2.0/repositories/{workspace}/{slug}/hooks`.
    /// 4. Return `(hook_uuid, encrypted_token)` to the caller for persistence.
    async fn install_bitbucket_webhook_for_connection(
        &self,
        project_id: i32,
        connection_id: i32,
        owner: &str,
        repo: &str,
    ) -> Result<(String, String), String> {
        use temps_git::services::bitbucket_provider::generate_bitbucket_webhook_token;
        use temps_git::services::git_provider::WebhookConfig;

        let (access_token, _auth_method_str) =
            match self.resolve_bitbucket_connection(connection_id).await {
                Ok(pair) => pair,
                Err(e) => return Err(e),
            };

        let webhook_base_url = self.git_webhook_base_url().await;

        // Generate a fresh secret-in-path delivery token.
        let delivery_token = generate_bitbucket_webhook_token()
            .map_err(|error| format!("Failed to generate Bitbucket webhook token: {error}"))?;

        let webhook_url = format!(
            "{}/api/webhook/git/bitbucket/events/{}",
            webhook_base_url, delivery_token
        );

        // Resolve the connection to get provider_id, then get provider service.
        let connection = {
            use temps_entities::git_provider_connections;
            git_provider_connections::Entity::find_by_id(connection_id)
                .one(self.db.as_ref())
                .await
                .map_err(|e| format!("DB error fetching connection {connection_id}: {e}"))?
                .ok_or_else(|| format!("Connection {connection_id} not found"))?
        };

        let provider_service = self
            .git_provider_manager
            .get_provider_service(connection.provider_id)
            .await
            .map_err(|e| {
                format!(
                    "Failed to get provider service for Bitbucket connection {connection_id}: {e}"
                )
            })?;

        let hook_uuid = provider_service
            .create_webhook(
                &access_token,
                owner,
                repo,
                WebhookConfig {
                    url: webhook_url,
                    secret: None, // Bitbucket uses secret-in-path; no HMAC secret
                    events: vec![
                        "repo:push".to_string(),
                        "pullrequest:created".to_string(),
                        "pullrequest:updated".to_string(),
                        "pullrequest:fulfilled".to_string(),
                        "pullrequest:rejected".to_string(),
                    ],
                },
            )
            .await
            .map_err(|e| {
                format!(
                    "Failed to register Bitbucket webhook for project {project_id} ({owner}/{repo}): {e}"
                )
            })?;

        // Encrypt the delivery token before storing.
        let encrypted_token = self
            .encryption_service
            .encrypt_string(&delivery_token)
            .map_err(|e| format!("Failed to encrypt Bitbucket webhook token: {e}"))?;

        info!(
            "Registered Bitbucket webhook {} for project {} ({}/{})",
            hook_uuid, project_id, owner, repo
        );

        Ok((hook_uuid, encrypted_token))
    }

    /// Delete a previously auto-registered Bitbucket webhook.  Best-effort;
    /// 404 is treated as success (already gone).
    async fn delete_bitbucket_webhook_for_connection(
        &self,
        connection_id: i32,
        owner: &str,
        repo: &str,
        hook_uuid: &str,
    ) -> Result<(), String> {
        let (access_token, _) = match self.resolve_bitbucket_connection(connection_id).await {
            Ok(pair) => pair,
            // Not a Bitbucket connection — nothing to do.
            Err(_) => return Ok(()),
        };

        let connection = {
            use temps_entities::git_provider_connections;
            git_provider_connections::Entity::find_by_id(connection_id)
                .one(self.db.as_ref())
                .await
                .map_err(|e| format!("DB error: {e}"))?
                .ok_or_else(|| format!("Connection {connection_id} not found"))?
        };

        let provider_service = self
            .git_provider_manager
            .get_provider_service(connection.provider_id)
            .await
            .map_err(|e| {
                format!(
                    "Failed to get provider service for Bitbucket connection {connection_id}: {e}"
                )
            })?;

        provider_service
            .delete_webhook(&access_token, owner, repo, hook_uuid)
            .await
            .map_err(|e| format!("Bitbucket delete webhook error for {owner}/{repo}: {e}"))
    }

    // ── Gitea webhook install / resolve ─────────────────────────────────────────
    //
    // BLOCKER 1 fix: Gitea webhooks use HMAC-SHA256 (unlike Bitbucket's
    // secret-in-path).  We generate a signing token with OsRng, register it
    // with Gitea's API via `create_webhook` (passing the secret in the config),
    // then store it encrypted in `projects.gitea_webhook_signing_token`.
    //
    // Failures are always non-fatal — same policy as Bitbucket/GitLab.

    /// Resolve whether `connection_id` points to a Gitea provider.
    ///
    /// Returns `(access_token, base_url)` for Gitea connections;
    /// `Err(String)` for all others (caller silently skips).
    async fn resolve_gitea_connection(
        &self,
        connection_id: i32,
    ) -> Result<(String, String), String> {
        use temps_entities::{git_provider_connections, git_providers};

        let connection = git_provider_connections::Entity::find_by_id(connection_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error fetching connection {connection_id}: {e}"))?
            .ok_or_else(|| format!("Connection {connection_id} not found"))?;

        let provider = git_providers::Entity::find_by_id(connection.provider_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error fetching provider {}: {e}", connection.provider_id))?
            .ok_or_else(|| format!("Provider {} not found", connection.provider_id))?;

        if provider.provider_type != "gitea" {
            return Err(format!(
                "Provider {} is not a Gitea provider (type: {})",
                provider.id, provider.provider_type
            ));
        }

        let base_url = provider
            .base_url
            .ok_or_else(|| format!("Gitea provider {} has no base_url configured", provider.id))?;

        let access_token = self
            .git_provider_manager
            .get_connection_token(connection_id)
            .await
            .map_err(|e| {
                format!("Failed to get access token for Gitea connection {connection_id}: {e}")
            })?;

        Ok((access_token, base_url))
    }

    /// Install a Gitea webhook for `project_id` / `connection_id`.
    ///
    /// Steps:
    /// 1. Resolve the connection to confirm it's a Gitea provider.
    /// 2. Generate a fresh 32-byte hex signing token via `generate_gitea_signing_token`.
    /// 3. Build the delivery URL: `{public ingress URL}/api/webhook/git/gitea/events`.
    /// 4. Call `create_webhook` with the HMAC secret (Gitea HMAC signing).
    /// 5. Encrypt the token and return it for storage.
    ///
    /// Returns `Ok(encrypted_token)` on success; `Err(String)` on any failure
    /// (non-fatal — caller logs and continues).
    async fn install_gitea_webhook_for_connection(
        &self,
        project_id: i32,
        connection_id: i32,
        owner: &str,
        repo: &str,
    ) -> Result<String, String> {
        use temps_git::services::git_provider::WebhookConfig;
        use temps_git::services::gitea_provider::generate_gitea_signing_token;

        let (access_token, _base_url) = match self.resolve_gitea_connection(connection_id).await {
            Ok(pair) => pair,
            Err(e) => return Err(e),
        };

        // Generate a fresh HMAC secret (used as the Gitea webhook secret).
        let signing_token = generate_gitea_signing_token()
            .map_err(|error| format!("Failed to generate Gitea webhook token: {error}"))?;

        let webhook_url = format!(
            "{}/api/webhook/git/gitea/events",
            self.git_webhook_base_url().await
        );

        // Resolve the connection to get provider_id, then get provider service.
        let connection = {
            use temps_entities::git_provider_connections;
            git_provider_connections::Entity::find_by_id(connection_id)
                .one(self.db.as_ref())
                .await
                .map_err(|e| format!("DB error fetching connection {connection_id}: {e}"))?
                .ok_or_else(|| format!("Connection {connection_id} not found"))?
        };

        let provider_service = self
            .git_provider_manager
            .get_provider_service(connection.provider_id)
            .await
            .map_err(|e| {
                format!("Failed to get provider service for Gitea connection {connection_id}: {e}")
            })?;

        // Gitea webhooks are HMAC-SHA256; pass the secret in WebhookConfig.secret.
        provider_service
            .create_webhook(
                &access_token,
                owner,
                repo,
                WebhookConfig {
                    url: webhook_url,
                    secret: Some(signing_token.clone()), // HMAC secret
                    events: vec!["push".to_string()],
                },
            )
            .await
            .map_err(|e| {
                format!(
                    "Failed to register Gitea webhook for project {project_id} ({owner}/{repo}): {e}"
                )
            })?;

        // Encrypt the signing token before storing.
        let encrypted_token = self
            .encryption_service
            .encrypt_string(&signing_token)
            .map_err(|e| format!("Failed to encrypt Gitea webhook signing token: {e}"))?;

        info!(
            "Registered Gitea webhook for project {} ({}/{})",
            project_id, owner, repo
        );

        Ok(encrypted_token)
    }

    // ── GitHub repository webhook install / delete ──────────────────────────────
    //
    // A GitHub App delivers every push through the App's own webhook. A GitHub
    // connection authenticated any other way (personal access token, OAuth)
    // has no App, so without a repository webhook nothing ever told Temps
    // about a push and auto-deploy silently did nothing (#1596). For those
    // connections we register a repository webhook signed with a fresh
    // per-project secret, stored encrypted together with the hook ID so the
    // hook can be deleted when the project's repository changes.
    //
    // Failures are non-fatal — same policy as GitLab/Gitea/Bitbucket.

    /// Resolve `connection_id` to `(provider_id, access_token)` when it is a
    /// GitHub connection that needs its own repository webhook, i.e. any
    /// GitHub connection that is not a GitHub App. `Ok(None)` for App
    /// connections and other providers (caller skips).
    async fn resolve_github_repo_webhook_connection(
        &self,
        connection_id: i32,
    ) -> Result<Option<(i32, String)>, String> {
        use temps_entities::{git_provider_connections, git_providers};

        let connection = git_provider_connections::Entity::find_by_id(connection_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error fetching connection {connection_id}: {e}"))?
            .ok_or_else(|| format!("Connection {connection_id} not found"))?;

        let provider = git_providers::Entity::find_by_id(connection.provider_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error fetching provider {}: {e}", connection.provider_id))?
            .ok_or_else(|| format!("Provider {} not found", connection.provider_id))?;

        if !github_connection_needs_repo_webhook(&provider.provider_type, &provider.auth_method) {
            return Ok(None);
        }

        let access_token = self
            .git_provider_manager
            .get_connection_token(connection_id)
            .await
            .map_err(|e| {
                format!("Failed to get access token for GitHub connection {connection_id}: {e}")
            })?;

        Ok(Some((provider.id, access_token)))
    }

    /// Register a GitHub repository webhook for `project_id` when
    /// `connection_id` is a non-App GitHub connection.
    ///
    /// With `existing_hook_id`, that hook is given the new secret in place and
    /// keeps its ID; a new hook is registered only when it no longer exists on
    /// GitHub. GitHub refuses a second hook with the same URL and events, so
    /// replacing a hook by creating its successor first would always fail.
    ///
    /// Returns `Ok(Some((hook_id, encrypted_secret)))` on success, `Ok(None)`
    /// when the connection does not need one, `Err` on failure (non-fatal —
    /// the caller logs and continues).
    async fn install_github_webhook_for_connection(
        &self,
        project_id: i32,
        connection_id: i32,
        owner: &str,
        repo: &str,
        existing_hook_id: Option<i64>,
    ) -> Result<Option<(i64, String)>, String> {
        use temps_git::services::git_provider::WebhookConfig;
        use temps_git::services::github_provider::generate_github_webhook_secret;

        let Some((provider_id, access_token)) = self
            .resolve_github_repo_webhook_connection(connection_id)
            .await?
        else {
            return Ok(None);
        };

        let secret = generate_github_webhook_secret()
            .map_err(|error| format!("Failed to generate GitHub webhook secret: {error}"))?;

        let config = WebhookConfig {
            url: github_repo_webhook_url(&self.git_webhook_base_url().await, project_id),
            secret: Some(secret.clone()),
            events: vec!["push".to_string()],
        };

        let provider_service = self
            .git_provider_manager
            .get_provider_service(provider_id)
            .await
            .map_err(|e| {
                format!("Failed to get provider service for GitHub connection {connection_id}: {e}")
            })?;

        let updated_hook_id = match existing_hook_id {
            Some(hook_id) => {
                let updated = provider_service
                    .update_webhook(
                        &access_token,
                        owner,
                        repo,
                        &hook_id.to_string(),
                        config.clone(),
                    )
                    .await
                    .map_err(|e| {
                        format!(
                            "Failed to update GitHub webhook {hook_id} for project {project_id} ({owner}/{repo}): {e}"
                        )
                    })?;
                if !updated {
                    info!(
                        "GitHub webhook {} for project {} no longer exists on {}/{}; registering a new one",
                        hook_id, project_id, owner, repo
                    );
                }
                updated.then_some(hook_id)
            }
            None => None,
        };

        let hook_id = match updated_hook_id {
            Some(hook_id) => hook_id,
            None => {
                let hook_id = provider_service
                    .create_webhook(&access_token, owner, repo, config)
                    .await
                    .map_err(|e| {
                        format!(
                            "Failed to register GitHub webhook for project {project_id} ({owner}/{repo}): {e}"
                        )
                    })?;
                hook_id.parse().map_err(|e| {
                    format!(
                        "GitHub returned a non-numeric webhook ID '{hook_id}' for project {project_id} ({owner}/{repo}): {e}"
                    )
                })?
            }
        };

        let encrypted_secret = self
            .encryption_service
            .encrypt_string(&secret)
            .map_err(|e| format!("Failed to encrypt GitHub webhook secret: {e}"))?;

        info!(
            "Installed GitHub repository webhook {} for project {} ({}/{})",
            hook_id, project_id, owner, repo
        );

        Ok(Some((hook_id, encrypted_secret)))
    }

    /// Delete a previously registered GitHub repository webhook. Best-effort.
    async fn delete_github_webhook_for_connection(
        &self,
        connection_id: i32,
        owner: &str,
        repo: &str,
        hook_id: i64,
    ) -> Result<(), String> {
        let Some((provider_id, access_token)) = self
            .resolve_github_repo_webhook_connection(connection_id)
            .await?
        else {
            // No longer a non-App GitHub connection — nothing we can delete with.
            return Ok(());
        };

        let provider_service = self
            .git_provider_manager
            .get_provider_service(provider_id)
            .await
            .map_err(|e| {
                format!("Failed to get provider service for GitHub connection {connection_id}: {e}")
            })?;

        provider_service
            .delete_webhook(&access_token, owner, repo, &hook_id.to_string())
            .await
            .map_err(|e| format!("GitHub delete webhook {hook_id} error for {owner}/{repo}: {e}"))
    }

    /// Install (or replace) the GitHub repository webhook for a project on a
    /// non-App GitHub connection. Called by
    /// `POST /projects/{id}/github/reinstall-webhook`, for when automatic
    /// registration failed (e.g. the token lacked webhook permission) or the
    /// hook was deleted on GitHub.
    pub async fn reinstall_github_webhook(&self, project_id: i32) -> Result<i64, ProjectError> {
        let project = projects::Entity::find_by_id(project_id)
            .one(self.db.as_ref())
            .await?
            .ok_or(ProjectError::NotFound(format!(
                "Project {} not found",
                project_id
            )))?;

        let connection_id = project.git_provider_connection_id.ok_or_else(|| {
            ProjectError::InvalidInput(format!(
                "Project {} has no git provider connection, so there is no repository to install a GitHub webhook on",
                project_id
            ))
        })?;

        let owner = project.repo_owner.clone();
        let repo = project.repo_name.clone();
        if owner.is_empty() || repo.is_empty() {
            return Err(ProjectError::InvalidInput(format!(
                "Project {} has no repository configured, so there is nothing to install a GitHub webhook on",
                project_id
            )));
        }

        // The stored hook is updated in place (GitHub refuses a second hook with
        // the same URL); a new one is registered only if it was deleted on
        // GitHub. Nothing is persisted unless GitHub accepted the change, so a
        // failure leaves the project with the hook and secret it had.
        let (hook_id, encrypted_secret) = self
            .install_github_webhook_for_connection(
                project_id,
                connection_id,
                &owner,
                &repo,
                project.github_webhook_id,
            )
            .await
            .map_err(ProjectError::GitHubError)?
            .ok_or_else(|| {
                ProjectError::InvalidInput(format!(
                    "Project {} is not connected through a GitHub personal access token or OAuth connection (connection {}). GitHub App connections receive pushes through the App webhook and need no repository webhook",
                    project_id, connection_id
                ))
            })?;

        let mut active_project: projects::ActiveModel = project.into();
        active_project.github_webhook_id = Set(Some(hook_id));
        active_project.github_webhook_signing_token = Set(Some(encrypted_secret));
        active_project.updated_at = Set(chrono::Utc::now());
        active_project.update(self.db.as_ref()).await?;

        info!(
            "Reinstalled GitHub repository webhook {} for project {}",
            hook_id, project_id
        );

        Ok(hook_id)
    }

    // ── Generic webhook token install ────────────────────────────────────────────
    //
    // BLOCKER 2 fix: the Generic provider has no REST API to auto-register a
    // hook.  We generate a token and store it encrypted.  The webhook URL
    // (`{public ingress URL}/api/webhook/git/generic/events/{token}`) is surfaced to
    // the operator for manual configuration.
    //
    // Returns `Ok(Some(encrypted_token))` for Generic connections,
    // `Ok(None)` for non-Generic connections, `Err` on failure.

    /// Generate and store a generic webhook delivery token for `project_id`.
    ///
    /// Returns `Ok(Some(encrypted_token))` when `connection_id` is a Generic
    /// provider; `Ok(None)` when it's any other provider type (caller skips).
    async fn install_generic_webhook_token(
        &self,
        project_id: i32,
        connection_id: i32,
    ) -> Result<Option<String>, String> {
        use temps_entities::{git_provider_connections, git_providers};
        use temps_git::services::generic_provider::generate_generic_webhook_token;

        let connection = git_provider_connections::Entity::find_by_id(connection_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error fetching connection {connection_id}: {e}"))?
            .ok_or_else(|| format!("Connection {connection_id} not found"))?;

        let provider = git_providers::Entity::find_by_id(connection.provider_id)
            .one(self.db.as_ref())
            .await
            .map_err(|e| format!("DB error fetching provider {}: {e}", connection.provider_id))?
            .ok_or_else(|| format!("Provider {} not found", connection.provider_id))?;

        if provider.provider_type != "generic" {
            // Not a Generic connection — nothing to do.
            return Ok(None);
        }

        let token = generate_generic_webhook_token()
            .map_err(|error| format!("Failed to generate Generic webhook token: {error}"))?;

        let encrypted_token = self
            .encryption_service
            .encrypt_string(&token)
            .map_err(|e| format!("Failed to encrypt Generic webhook token: {e}"))?;

        info!(
            "Generated and encrypted Generic webhook token for project {} (conn {})",
            project_id, connection_id
        );

        Ok(Some(encrypted_token))
    }

    pub async fn get_projects_paginated(
        &self,
        page: i64,
        per_page: i64,
    ) -> Result<(Vec<Project>, i64), ProjectError> {
        self.get_projects_paginated_excluding(page, per_page, &[])
            .await
    }

    /// [`Self::get_projects_paginated`], minus a caller-supplied set of
    /// project ids.
    ///
    /// `hidden` comes from
    /// [`ProjectAccessChecker::hidden_project_ids`](temps_core::ProjectAccessChecker::hidden_project_ids)
    /// and is empty on an instance with no access grants configured, which
    /// makes this identical to the unfiltered query in that case. The
    /// exclusion is applied to the **count** as well as the page, so
    /// pagination doesn't advertise rows the caller can never see.
    pub async fn get_projects_paginated_excluding(
        &self,
        page: i64,
        per_page: i64,
        hidden: &[i32],
    ) -> Result<(Vec<Project>, i64), ProjectError> {
        self.get_projects_paginated_excluding_search(page, per_page, hidden, None)
            .await
    }

    pub async fn get_projects_paginated_excluding_search(
        &self,
        page: i64,
        per_page: i64,
        hidden: &[i32],
        search: Option<&str>,
    ) -> Result<(Vec<Project>, i64), ProjectError> {
        self.get_projects_paginated_filtered(page, per_page, hidden, search, None)
            .await
    }

    /// Paginated project list with an optional group-membership filter.
    ///
    /// `group_filter`:
    /// - `None` — all projects (existing behaviour)
    /// - `Some(GroupFilter::Ungrouped)` — services not in any project group
    /// - `Some(GroupFilter::InGroup(id))` — services that are members of
    ///   the given project group
    pub async fn get_projects_paginated_filtered(
        &self,
        page: i64,
        per_page: i64,
        hidden: &[i32],
        search: Option<&str>,
        group_filter: Option<GroupFilter>,
    ) -> Result<(Vec<Project>, i64), ProjectError> {
        use sea_orm::sea_query::extension::postgres::PgExpr;
        use sea_orm::sea_query::LikeExpr;
        use sea_orm::PaginatorTrait;
        use sea_orm::QueryOrder;

        if page < 1 {
            return Err(ProjectError::InvalidInput(
                "page must be greater than or equal to 1".to_string(),
            ));
        }
        if !(1..=100).contains(&per_page) {
            return Err(ProjectError::InvalidInput(
                "per_page must be between 1 and 100".to_string(),
            ));
        }

        let offset = page
            .checked_sub(1)
            .and_then(|zero_based_page| zero_based_page.checked_mul(per_page))
            .and_then(|offset| u64::try_from(offset).ok())
            .ok_or_else(|| {
                ProjectError::InvalidInput("page produces an invalid pagination offset".to_string())
            })?;

        // Build subquery for member project_ids once; reused for both count and
        // page queries so the filter is consistently applied to both.
        let group_member_subquery = || {
            sea_orm::sea_query::Query::select()
                .column(project_group_members::Column::ProjectId)
                .from(project_group_members::Entity)
                .to_owned()
        };
        let group_member_subquery_for_group = |group_id: i32| {
            sea_orm::sea_query::Query::select()
                .column(project_group_members::Column::ProjectId)
                .from(project_group_members::Entity)
                .and_where(
                    sea_orm::sea_query::Expr::col(project_group_members::Column::GroupId)
                        .eq(group_id),
                )
                .to_owned()
        };

        let filtered = || {
            let mut query = projects::Entity::find();
            if !hidden.is_empty() {
                query = query.filter(projects::Column::Id.is_not_in(hidden.iter().copied()));
            }
            if let Some(search) = search {
                let pattern = format!("%{}%", escape_like_literal(search));
                // Table-qualified: the page query joins `git_providers`, which
                // has a `name` column of its own, and an unqualified `name`
                // here is rejected by Postgres as ambiguous.
                query = query.filter(
                    Condition::any()
                        .add(
                            sea_orm::sea_query::Expr::col((
                                projects::Entity,
                                projects::Column::Name,
                            ))
                            .ilike(LikeExpr::new(pattern.clone())),
                        )
                        .add(
                            sea_orm::sea_query::Expr::col((
                                projects::Entity,
                                projects::Column::Slug,
                            ))
                            .ilike(LikeExpr::new(pattern.clone())),
                        ),
                );
            }
            // Apply group-membership filter using a subquery so we never join
            // into the page query (that already has a git_providers join).
            match group_filter {
                None => {}
                Some(GroupFilter::Ungrouped) => {
                    query =
                        query.filter(projects::Column::Id.not_in_subquery(group_member_subquery()));
                }
                Some(GroupFilter::InGroup(group_id)) => {
                    query = query.filter(
                        projects::Column::Id.in_subquery(group_member_subquery_for_group(group_id)),
                    );
                }
            }
            query
        };

        // Get total count
        let total = filtered()
            .count(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::DatabaseConnectionError(e.to_string()))?
            as i64;

        // Get paginated projects. Never-deployed projects (NULL last_deployment)
        // sort last rather than first (a NULL under DESC would otherwise appear
        // as the most-recently-deployed project).
        // The count query above stays join-free: it counts projects, and the
        // provider join exists only to carry an extra column.
        let projects = Self::with_git_provider_type(filtered())
            .order_by_with_nulls(
                projects::Column::LastDeployment,
                sea_orm::Order::Desc,
                sea_orm::sea_query::NullOrdering::Last,
            )
            .order_by_desc(projects::Column::CreatedAt)
            .offset(offset)
            .limit(per_page as u64)
            .into_model::<ProjectWithGitProviderType>()
            .all(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::DatabaseConnectionError(e.to_string()))?;

        let projects_found: Vec<Project> = projects
            .into_iter()
            .map(Self::map_db_project_row_to_project)
            .collect();

        Ok((projects_found, total))
    }

    pub async fn get_total_projects(&self) -> Result<i64, ProjectError> {
        use sea_orm::PaginatorTrait;
        // Get total count of projects
        let paginator = projects::Entity::find().paginate(self.db.as_ref(), 1);
        let total = paginator.num_items().await?;

        Ok(total as i64)
    }

    pub async fn get_project_statistics(&self) -> Result<ProjectStatistics, ProjectError> {
        self.get_project_statistics_excluding(&[]).await
    }

    /// [`Self::get_project_statistics`], minus a caller-supplied set of
    /// project ids.
    ///
    /// The count has to honour the same exclusion as the list, or the
    /// dashboard tells a scoped user how many projects exist on the
    /// instance while showing them only their own — a smaller leak than
    /// the names, but the same leak.
    pub async fn get_project_statistics_excluding(
        &self,
        hidden: &[i32],
    ) -> Result<ProjectStatistics, ProjectError> {
        use sea_orm::PaginatorTrait;

        let query = projects::Entity::find();
        let query = if hidden.is_empty() {
            query
        } else {
            query.filter(projects::Column::Id.is_not_in(hidden.iter().copied()))
        };

        let total_count = query
            .count(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::DatabaseConnectionError(e.to_string()))?
            as i64;

        let has_completed_deployment = has_ready_deployment(self.db.as_ref(), hidden).await?;

        Ok(ProjectStatistics {
            total_count,
            has_completed_deployment,
        })
    }

    /// Reject `deployment_config` if it violates `app_settings`'s tenant
    /// resource ceilings; a no-op when no ceiling is configured.
    ///
    /// Pulled out of the two call sites below so the error-mapping (join the
    /// violations into one `InvalidInput`) has a single place to test without
    /// standing up a full `ProjectService` — the ceiling logic itself is
    /// covered independently on [`temps_entities::deployment_config::DeploymentConfig::check_against_tenant_ceilings`].
    fn enforce_tenant_ceilings(
        deployment_config: &temps_entities::deployment_config::DeploymentConfig,
        app_settings: &temps_core::AppSettings,
    ) -> Result<(), ProjectError> {
        deployment_config
            .check_against_tenant_ceilings(&app_settings.tenant_resource_ceilings)
            .map_err(|violations| ProjectError::InvalidInput(violations.join("; ")))
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

    /// Replace the editable runtime snapshot for a single-container image
    /// template and its project-level resources atomically.
    ///
    /// Both JSON columns live on `projects`, so one row update is enough. A
    /// rejected image, command, health path, resource profile, or tenant
    /// ceiling leaves every previous value intact.
    ///
    /// `caller` is required (ADR 045): this is where a project's *persisted*
    /// runtime image and command are written, and on a project this control
    /// plane declares, that command is what the next deployment runs as host
    /// root — including a deployment started by an admin, who would be
    /// executing a payload they did not write. Ordinary `ProjectsWrite` is
    /// therefore not sufficient here for a declared project.
    pub async fn update_service_template_runtime(
        &self,
        project_id: i32,
        runtime: UpdateServiceTemplateRuntimeRequest,
        ceiling_enforcement: temps_core::CeilingEnforcement,
        caller: DeployCaller,
    ) -> Result<Project, ProjectError> {
        let txn = self.db.begin().await?;
        let project = projects::Entity::find_by_id(project_id)
            .filter(projects::Column::IsDeleted.eq(false))
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                ProjectError::NotFound(format!("Project with id {} not found", project_id))
            })?;
        self.guard_granted_project_write(&project.slug, "the runtime image and command", caller)?;

        if project.source_type != temps_entities::source_type::SourceType::DockerImage
            || project.project_type != ProjectType::Service
        {
            return Err(ProjectError::InvalidInput(
                "Service runtime settings are available only for template-backed service projects"
                    .to_string(),
            ));
        }
        if project.preset != temps_entities::preset::Preset::Dockerfile {
            return Err(ProjectError::InvalidInput(
                "Single-container service templates must use the Dockerfile runtime preset"
                    .to_string(),
            ));
        }
        // A service identity is never inferred from its slug or current
        // runtime. New service projects always carry their complete release.
        applied_service_template_from_model(&project)?;

        let mut preset_config = project.preset_config.clone().unwrap_or_else(|| {
            temps_entities::preset::PresetConfig::default_for_preset(
                temps_entities::preset::Preset::Dockerfile,
            )
        });
        let temps_entities::preset::PresetConfig::Dockerfile(config) = &mut preset_config else {
            return Err(ProjectError::InvalidInput(
                "Project preset configuration does not match the Dockerfile runtime".to_string(),
            ));
        };
        config.image_runtime = Some(temps_entities::preset::ImageRuntimeConfig {
            image_ref: runtime.image_ref,
            command: (!runtime.command.is_empty()).then_some(runtime.command),
            health_check_path: Some(runtime.health_check_path),
        });
        temps_presets::validate_preset_config(project.preset, &preset_config).map_err(|error| {
            ProjectError::InvalidInput(format!("Invalid service runtime: {error}"))
        })?;

        let mut deployment_config = project.deployment_config.clone().unwrap_or_default();
        deployment_config.cpu_request = runtime.cpu_request;
        deployment_config.cpu_limit = runtime.cpu_limit;
        deployment_config.memory_request = runtime.memory_request;
        deployment_config.memory_limit = runtime.memory_limit;
        deployment_config.exposed_port = runtime.exposed_port;
        deployment_config.validate().map_err(|error| {
            ProjectError::InvalidInput(format!("Invalid deployment config: {error}"))
        })?;

        if ceiling_enforcement == temps_core::CeilingEnforcement::Enforce {
            let app_settings = self.config_service.get_settings().await.map_err(|error| {
                ProjectError::Other(format!(
                    "Failed to read instance settings to check resource ceilings for project {project_id}: {error}"
                ))
            })?;
            Self::enforce_tenant_ceilings(&deployment_config, &app_settings)?;
        }

        let mut active_project: projects::ActiveModel = project.into();
        active_project.preset_config = Set(Some(preset_config));
        active_project.deployment_config = Set(Some(deployment_config));
        let updated_project = active_project.update(&txn).await?;
        txn.commit().await?;

        let project_updated_job = Job::ProjectUpdated(ProjectUpdatedJob {
            project_id: updated_project.id,
            project_name: updated_project.name.clone(),
        });
        if let Err(error) = self.queue_service.send(project_updated_job).await {
            warn!(
                "Failed to emit ProjectUpdated job for project {}: {}",
                updated_project.id, error
            );
        }

        Ok(self.map_written_project(updated_project).await)
    }

    /// Apply a newer release of the same service template while preserving
    /// instance overrides. A field receives the new template default only when
    /// its current value still equals the previously applied default.
    pub async fn upgrade_service_template(
        &self,
        project_id: i32,
        target: temps_core::templates::ServiceTemplateInstance,
        new_environment_variables: Vec<CreateProjectEnvVar>,
        ceiling_enforcement: temps_core::CeilingEnforcement,
        caller: DeployCaller,
    ) -> Result<Project, ProjectError> {
        let txn = self.db.begin().await?;
        let project = projects::Entity::find_by_id(project_id)
            .filter(projects::Column::IsDeleted.eq(false))
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                ProjectError::NotFound(format!("Project with id {} not found", project_id))
            })?;
        // ADR 045: the sibling of `update_service_template_runtime`, and the
        // other way the persisted image and command of a service project
        // change. The target comes from the operator-installed catalog rather
        // than from the request body, so this is the weaker of the two — but
        // it still decides what a declared project runs as host root, and
        // leaving it open would make "downgrade to a release with a different
        // entrypoint" the way around the guard next to it.
        self.guard_granted_project_write(
            &project.slug,
            "the applied service template release",
            caller,
        )?;

        if project.project_type != ProjectType::Service {
            return Err(ProjectError::InvalidInput(
                "Only service projects can apply service-template upgrades".to_string(),
            ));
        }
        if target.template.kind != temps_core::templates::TemplateKind::App {
            return Err(ProjectError::InvalidInput(format!(
                "Template '{}' is not a service release",
                target.slug
            )));
        }
        target.validate().map_err(|error| {
            ProjectError::InvalidInput(format!(
                "Invalid target service template {}@{}: {error}",
                target.slug, target.version,
            ))
        })?;

        // Only one service's runtime is applied here. A release with several
        // services would persist its full snapshot without creating the extra
        // services or routes, so it can only be deployed as a whole app.
        if target.template.is_multi_service() {
            return Err(ProjectError::SingleServiceUpgradeToMultiService {
                project_id,
                template_slug: target.slug.clone(),
                target_version: target.version.clone(),
                service_count: target.template.services.len(),
            });
        }

        let applied = applied_service_template_from_model(&project)?;

        if applied.slug != target.slug {
            return Err(ProjectError::InvalidInput(format!(
                "A '{}' service cannot be changed into '{}'; select a newer release of the same template",
                applied.slug, target.slug
            )));
        }
        let is_newer = target.is_newer_than(&applied).map_err(|error| {
            ProjectError::InvalidInput(format!(
                "Cannot compare service template releases {}@{} and {}@{}: {error}",
                applied.slug, applied.version, target.slug, target.version
            ))
        })?;
        if !is_newer {
            return Err(ProjectError::InvalidInput(format!(
                "Service project {project_id} uses {}@{}; target release {}@{} must be newer",
                applied.slug, applied.version, target.slug, target.version
            )));
        }
        let applied_spec = applied.spec().cloned().ok_or_else(|| {
            ProjectError::Other(format!(
                "Stored service template for project {project_id} names a service it does not define"
            ))
        })?;
        let target_spec = target.spec().cloned().ok_or_else(|| {
            ProjectError::InvalidInput(format!(
                "Template {}@{} has no '{}' service",
                target.slug,
                target.version,
                target.service.as_deref().unwrap_or_default()
            ))
        })?;
        validate_service_binding_compatibility(&applied_spec, &target_spec)?;
        if project.source_type != temps_entities::source_type::SourceType::DockerImage
            || project.preset != temps_entities::preset::Preset::Dockerfile
        {
            return Err(ProjectError::InvalidInput(
                "This release can only upgrade a single-container image service".to_string(),
            ));
        }

        let linked_services = project_services::Entity::find()
            .filter(project_services::Column::ProjectId.eq(project_id))
            .find_also_related(external_services::Entity)
            .all(&txn)
            .await?;
        let linked_service_types = linked_services
            .iter()
            .filter_map(|(_, service)| service.as_ref())
            .map(|service| service.service_type.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let missing_services =
            missing_required_service_types(&target_spec.databases, &linked_service_types);
        if !missing_services.is_empty() {
            return Err(ProjectError::InvalidInput(format!(
                "Template {}@{} requires linked managed service(s): {}",
                target.slug,
                target.version,
                missing_services.join(", ")
            )));
        }
        for required in &target_spec.databases {
            let count = linked_services
                .iter()
                .filter_map(|(_, service)| service.as_ref())
                .filter(|service| {
                    temps_core::templates::managed_service_types_compatible(
                        required,
                        &service.service_type,
                    )
                })
                .count();
            if count > 1 {
                return Err(ProjectError::InvalidInput(format!(
                    "Template {}@{} requires exactly one linked {} service, but project {project_id} has {count}",
                    target.slug, target.version, required
                )));
            }
        }

        let mut preset_config = project.preset_config.clone().ok_or_else(|| {
            ProjectError::Other(format!(
                "Service project {project_id} has no persisted runtime configuration"
            ))
        })?;
        let temps_entities::preset::PresetConfig::Dockerfile(config) = &mut preset_config else {
            return Err(ProjectError::Other(format!(
                "Service project {project_id} has a non-Dockerfile runtime configuration"
            )));
        };
        let current_runtime = config.image_runtime.clone().ok_or_else(|| {
            ProjectError::Other(format!(
                "Service project {project_id} has no persisted image runtime"
            ))
        })?;
        let target_image = target_spec.image.clone().ok_or_else(|| {
            ProjectError::InvalidInput(format!(
                "Target service template {}@{} has no image",
                target.slug, target.version
            ))
        })?;

        config.image_runtime = Some(temps_entities::preset::ImageRuntimeConfig {
            image_ref: apply_new_template_default_when_unmodified(
                current_runtime.image_ref,
                applied_spec.image.clone().unwrap_or_default(),
                target_image,
            ),
            command: apply_new_template_default_when_unmodified(
                current_runtime.command,
                applied_spec.command.clone(),
                target_spec.command.clone(),
            ),
            health_check_path: apply_new_template_default_when_unmodified(
                current_runtime.health_check_path,
                applied_spec
                    .health_check_path
                    .clone()
                    .or_else(|| Some("/".to_string())),
                target_spec
                    .health_check_path
                    .clone()
                    .or_else(|| Some("/".to_string())),
            ),
        });
        temps_presets::validate_preset_config(project.preset, &preset_config).map_err(|error| {
            ProjectError::InvalidInput(format!("Invalid target service runtime: {error}"))
        })?;

        let mut deployment_config = project.deployment_config.clone().unwrap_or_default();
        let applied_resources = &applied_spec;
        let target_resources = &target_spec;
        deployment_config.cpu_request = apply_new_template_default_when_unmodified(
            deployment_config.cpu_request,
            template_resource_value(applied_resources, |resources| resources.cpu_request),
            template_resource_value(target_resources, |resources| resources.cpu_request),
        );
        deployment_config.cpu_limit = apply_new_template_default_when_unmodified(
            deployment_config.cpu_limit,
            template_resource_value(applied_resources, |resources| resources.cpu_limit),
            template_resource_value(target_resources, |resources| resources.cpu_limit),
        );
        deployment_config.memory_request = apply_new_template_default_when_unmodified(
            deployment_config.memory_request,
            template_resource_value(applied_resources, |resources| resources.memory_request),
            template_resource_value(target_resources, |resources| resources.memory_request),
        );
        deployment_config.memory_limit = apply_new_template_default_when_unmodified(
            deployment_config.memory_limit,
            template_resource_value(applied_resources, |resources| resources.memory_limit),
            template_resource_value(target_resources, |resources| resources.memory_limit),
        );
        deployment_config.exposed_port = apply_new_template_default_when_unmodified(
            deployment_config.exposed_port,
            applied_spec.exposed_port,
            target_spec.exposed_port,
        );
        deployment_config.validate().map_err(|error| {
            ProjectError::InvalidInput(format!("Invalid target deployment config: {error}"))
        })?;

        if ceiling_enforcement == temps_core::CeilingEnforcement::Enforce {
            let app_settings = self.config_service.get_settings().await.map_err(|error| {
                ProjectError::Other(format!(
                    "Failed to read instance settings before upgrading project {project_id}: {error}"
                ))
            })?;
            Self::enforce_tenant_ceilings(&deployment_config, &app_settings)?;
        }

        let production_environment = environments::Entity::find()
            .filter(environments::Column::ProjectId.eq(project_id))
            .filter(environments::Column::IsPreview.eq(false))
            .filter(environments::Column::DeletedAt.is_null())
            .order_by_asc(environments::Column::Id)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                ProjectError::Other(format!(
                    "Service project {project_id} has no active production environment"
                ))
            })?;
        let existing_variables = env_vars::Entity::find()
            .filter(env_vars::Column::ProjectId.eq(project_id))
            .all(&txn)
            .await?;
        let existing_variable_ids = existing_variables
            .iter()
            .map(|variable| variable.id)
            .collect::<Vec<_>>();
        let variable_links = if existing_variable_ids.is_empty() {
            Vec::new()
        } else {
            env_var_environments::Entity::find()
                .filter(env_var_environments::Column::EnvVarId.is_in(existing_variable_ids))
                .all(&txn)
                .await?
        };
        let linked_ids = variable_links
            .iter()
            .map(|link| link.env_var_id)
            .collect::<HashSet<_>>();
        let production_linked_ids = variable_links
            .iter()
            .filter(|link| link.environment_id == production_environment.id)
            .map(|link| link.env_var_id)
            .collect::<HashSet<_>>();
        let applies_to_production = |variable: &env_vars::Model| {
            variable.environment_id == Some(production_environment.id)
                || production_linked_ids.contains(&variable.id)
                || (variable.environment_id.is_none() && !linked_ids.contains(&variable.id))
        };
        let mut production_variables = BTreeMap::<String, Vec<env_vars::Model>>::new();
        for variable in existing_variables
            .iter()
            .filter(|variable| applies_to_production(variable))
        {
            production_variables
                .entry(variable.key.clone())
                .or_default()
                .push(variable.clone());
        }
        let is_production_specific = |variable: &env_vars::Model| {
            variable.environment_id == Some(production_environment.id)
                || production_linked_ids.contains(&variable.id)
        };
        let effective_index = |variables: &[env_vars::Model]| {
            variables
                .iter()
                .enumerate()
                .max_by_key(|(_, variable)| (is_production_specific(variable), variable.id))
                .map(|(index, _)| index)
        };
        let mut configured_keys = HashSet::new();
        for (key, variables) in &production_variables {
            let Some(variable) = effective_index(variables).map(|index| &variables[index]) else {
                continue;
            };
            let cleartext = if variable.is_encrypted {
                self.encryption_service
                    .decrypt_string(&variable.value)
                    .map_err(|error| {
                        ProjectError::Other(format!(
                            "Failed to decrypt production environment variable '{}' while upgrading project {project_id}: {error}",
                            variable.key
                        ))
                    })?
            } else {
                variable.value.clone()
            };
            if !cleartext.is_empty() {
                configured_keys.insert(key.clone());
            }
        }

        let target_variables = target.service_variables();
        let definitions = target_variables
            .iter()
            .map(|definition| (definition.name.as_str(), definition))
            .collect::<BTreeMap<_, _>>();
        let mut pending_keys = HashSet::new();
        let mut pending_non_empty_keys = HashSet::new();
        for variable in &new_environment_variables {
            if !pending_keys.insert(variable.key.clone()) {
                return Err(ProjectError::InvalidInput(format!(
                    "Environment variable '{}' is supplied more than once for template upgrade",
                    variable.key
                )));
            }
            if !definitions.contains_key(variable.key.as_str()) {
                return Err(ProjectError::InvalidInput(format!(
                    "Environment variable '{}' is not declared by template {}@{}",
                    variable.key, target.slug, target.version
                )));
            }
            if !variable.value.is_empty() {
                pending_non_empty_keys.insert(variable.key.clone());
            }
        }
        for definition in target_variables
            .iter()
            .filter(|definition| definition.required)
        {
            if !configured_keys.contains(&definition.name)
                && !pending_non_empty_keys.contains(&definition.name)
            {
                return Err(ProjectError::InvalidInput(format!(
                    "Required environment variable '{}' has no production value for template {}@{}",
                    definition.name, target.slug, target.version
                )));
            }
        }

        // A newer release may classify an existing configuration key as a
        // credential. Sensitivity is project/key-wide, so promote matching
        // rows in every scope before the snapshot changes; never leave a
        // preview/staging duplicate readable or demote an existing secret.
        for definition in target_variables
            .iter()
            .filter(|definition| definition.is_secret())
        {
            for existing in existing_variables
                .iter()
                .filter(|variable| variable.key == definition.name && !variable.is_secret)
            {
                let mut active: env_vars::ActiveModel = existing.clone().into();
                active.is_secret = Set(true);
                active.update(&txn).await?;
            }
        }

        for variable in new_environment_variables {
            // A concurrent settings edit may have created the input after the
            // upgrade preview. Preserve that user value instead of replacing
            // it with a stale submitted/default value.
            if configured_keys.contains(&variable.key) {
                continue;
            }
            let definition = definitions.get(variable.key.as_str()).ok_or_else(|| {
                ProjectError::InvalidInput(format!(
                    "Environment variable '{}' is not declared by template {}@{}",
                    variable.key, target.slug, target.version
                ))
            })?;
            if variable.value.is_empty() && definition.required {
                return Err(ProjectError::InvalidInput(format!(
                    "Required environment variable '{}' cannot be empty",
                    variable.key
                )));
            }
            let encrypted_value = self
                .encryption_service
                .encrypt_string(&variable.value)
                .map_err(|error| {
                    ProjectError::Other(format!(
                        "Failed to encrypt environment variable '{}' while upgrading project {project_id}: {error}",
                        variable.key
                    ))
                })?;
            if let Some(existing_variables) = production_variables.get_mut(&variable.key) {
                let existing_index = effective_index(existing_variables).ok_or_else(|| {
                    ProjectError::Other(format!(
                        "No effective production row found for environment variable '{}' while upgrading project {project_id}",
                        variable.key
                    ))
                })?;
                let existing = &mut existing_variables[existing_index];
                let mut active: env_vars::ActiveModel = existing.clone().into();
                active.value = Set(encrypted_value);
                active.is_encrypted = Set(true);
                active.is_secret =
                    Set(existing.is_secret || variable.is_secret || definition.is_secret());
                *existing = active.update(&txn).await?;
                configured_keys.insert(variable.key);
                continue;
            }
            let inserted = env_vars::ActiveModel {
                project_id: Set(project_id),
                environment_id: Set(None),
                key: Set(variable.key.clone()),
                value: Set(encrypted_value),
                include_in_preview: Set(false),
                is_encrypted: Set(true),
                is_secret: Set(variable.is_secret || definition.is_secret()),
                ..Default::default()
            }
            .insert(&txn)
            .await?;
            env_var_environments::ActiveModel {
                env_var_id: Set(inserted.id),
                environment_id: Set(production_environment.id),
                ..Default::default()
            }
            .insert(&txn)
            .await?;
            configured_keys.insert(variable.key);
        }

        let target_json = serde_json::to_value(&target).map_err(|error| {
            ProjectError::Other(format!(
                "Failed to persist target service template {}@{} for project {project_id}: {error}",
                target.slug, target.version
            ))
        })?;
        let mut active_project: projects::ActiveModel = project.into();
        active_project.project_type = Set(ProjectType::Service);
        active_project.template_slug = Set(Some(target.slug));
        active_project.service_template = Set(Some(target_json));
        active_project.preset_config = Set(Some(preset_config));
        active_project.deployment_config = Set(Some(deployment_config));
        let updated_project = active_project.update(&txn).await?;
        txn.commit().await?;

        if let Err(error) = self
            .queue_service
            .send(Job::ProjectUpdated(ProjectUpdatedJob {
                project_id: updated_project.id,
                project_name: updated_project.name.clone(),
            }))
            .await
        {
            warn!(
                project_id = updated_project.id,
                %error,
                "Failed to emit ProjectUpdated after service-template upgrade"
            );
        }

        Ok(self.map_written_project(updated_project).await)
    }

    /// Refuse a display name another active project already uses.
    ///
    /// Project names are not unique in the schema -- the slug is the
    /// identifier -- but the name is what the console, CLI, notifications and
    /// audit log show, so two active projects with the same name are
    /// indistinguishable everywhere except their URL. A duplicate name used to
    /// be accepted silently and given a random slug suffix (`whoami-3f9a1c`),
    /// which is exactly that confusion. The comparison ignores case, since
    /// `Whoami` and `whoami` read as the same project too.
    ///
    /// Deleted projects are ignored: their slug stays reserved (handled by
    /// [`Self::generate_unique_project_slug`]), but their name is free to reuse.
    /// `exclude_project_id` lets a project keep -- or re-case -- its own name.
    /// Serialize create/rename writes by normalized display name. Existing
    /// installations need no data-changing uniqueness migration.
    async fn persist_named_project(
        &self,
        active: projects::ActiveModel,
        inserting: bool,
    ) -> Result<projects::Model, ProjectError> {
        let name =
            active.name.clone().take().ok_or_else(|| {
                ProjectError::InvalidInput("Project name is required".to_string())
            })?;
        let id = active.id.clone().take();
        let slug = active.slug.clone().take().unwrap_or_default();
        let txn = self.db.begin().await?;
        let changing_name = if !inserting {
            let current = projects::Entity::find_by_id(id.ok_or_else(|| {
                ProjectError::InvalidInput("Project ID is required for an update".to_string())
            })?)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or_else(|| {
                ProjectError::NotFound("Project disappeared during update".to_string())
            })?;
            current.name.to_lowercase() != name.to_lowercase()
        } else {
            true
        };
        // Legacy duplicate display names must not block unrelated edits or
        // recasing. Only a new normalized name claims the shared namespace.
        if changing_name {
            if let Err(error) = self
                .lock_project_name_available(&txn, &name, if inserting { None } else { id })
                .await
            {
                txn.rollback().await?;
                return Err(error);
            }
        }
        let result = if inserting {
            active.insert(&txn).await
        } else {
            active.update(&txn).await
        };
        let result = result.map_err(|e| {
            if super::types::is_unique_violation(&e) {
                ProjectError::SlugConflict { slug }
            } else {
                ProjectError::DatabaseError {
                    reason: e.to_string(),
                }
            }
        })?;
        txn.commit().await?;
        Ok(result)
    }

    async fn lock_project_name_available(
        &self,
        txn: &DatabaseTransaction,
        name: &str,
        exclude: Option<i32>,
    ) -> Result<(), ProjectError> {
        use sea_orm::sea_query::{Expr, Func};
        txn.query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            [format!("temps-project-name:{}", name.to_lowercase()).into()],
        ))
        .await?;
        let mut query = projects::Entity::find()
            .filter(projects::Column::IsDeleted.eq(false))
            .filter(
                Expr::expr(Func::lower(Expr::col(projects::Column::Name))).eq(name.to_lowercase()),
            );
        if let Some(id) = exclude {
            query = query.filter(projects::Column::Id.ne(id));
        }
        if query.one(txn).await?.is_some() {
            return Err(ProjectError::NameAlreadyExists {
                name: name.to_string(),
            });
        }
        Ok(())
    }

    async fn ensure_project_name_available(
        &self,
        name: &str,
        exclude_project_id: Option<i32>,
    ) -> Result<(), ProjectError> {
        use sea_orm::sea_query::{Expr, Func};

        let mut query = projects::Entity::find()
            .filter(projects::Column::IsDeleted.eq(false))
            .filter(
                Expr::expr(Func::lower(Expr::col(projects::Column::Name))).eq(name.to_lowercase()),
            );
        if let Some(project_id) = exclude_project_id {
            query = query.filter(projects::Column::Id.ne(project_id));
        }
        match query.one(self.db.as_ref()).await? {
            Some(_) => Err(ProjectError::NameAlreadyExists {
                name: name.to_string(),
            }),
            None => Ok(()),
        }
    }

    /// Generate a unique project slug by checking for collisions and appending a short UUID if needed.
    /// Slug is truncated to 40 chars max to keep DNS labels within the 63-char limit
    /// when combined with environment slug and service name prefix.
    pub async fn generate_unique_project_slug(&self, name: &str) -> Result<String, ProjectError> {
        let base_slug = base_project_slug(name);

        // First, try the base slug
        let existing = projects::Entity::find()
            .filter(projects::Column::Slug.eq(&base_slug))
            .one(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))?;

        if existing.is_none() {
            return Ok(base_slug);
        }

        // If base slug exists, generate a short UUID suffix
        let short_uuid = Uuid::new_v4()
            .to_string()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .take(6)
            .collect::<String>()
            .to_lowercase();

        let unique_slug = format!("{}-{}", base_slug, short_uuid);

        // Double check that this unique slug doesn't exist (extremely unlikely but be safe)
        let existing_unique = projects::Entity::find()
            .filter(projects::Column::Slug.eq(&unique_slug))
            .one(self.db.as_ref())
            .await
            .map_err(|e| ProjectError::Other(e.to_string()))?;

        if existing_unique.is_some() {
            // This is extremely unlikely, but if it happens, generate a new UUID
            let retry_uuid = Uuid::new_v4()
                .to_string()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .take(8)
                .collect::<String>()
                .to_lowercase();
            Ok(format!("{}-{}", base_slug, retry_uuid))
        } else {
            Ok(unique_slug)
        }
    }

    /// Validate a new project's name and choose its slug, before any side
    /// effect (a forked repository, provisioned services) of a multi-step
    /// create. Refuses a name another active project uses, so a template
    /// deploy fails here rather than after creating resources.
    pub async fn plan_project_slug(&self, name: &str) -> Result<String, ProjectError> {
        let validated_name = validate_project_name(name)?;
        self.ensure_project_name_available(&validated_name, None)
            .await?;
        self.generate_unique_project_slug(&validated_name).await
    }

    async fn validate_expected_project_slug(
        &self,
        name: &str,
        expected_slug: &str,
    ) -> Result<(), ProjectError> {
        let base_slug = base_project_slug(name);
        let suffix = expected_slug
            .strip_prefix(&format!("{base_slug}-"))
            .filter(|suffix| matches!(suffix.len(), 6 | 8))
            .filter(|suffix| suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()));
        if expected_slug != base_slug && suffix.is_none() {
            return Err(ProjectError::InvalidInput(format!(
                "Expected slug '{expected_slug}' does not match project name '{name}'"
            )));
        }
        let exists = projects::Entity::find()
            .filter(projects::Column::Slug.eq(expected_slug))
            .one(self.db.as_ref())
            .await
            .map_err(|error| ProjectError::DatabaseError {
                reason: format!(
                    "failed to validate expected project slug '{expected_slug}': {error}"
                ),
            })?
            .is_some();
        if exists {
            return Err(ProjectError::SlugConflict {
                slug: expected_slug.to_string(),
            });
        }
        Ok(())
    }

    /// LEFT JOIN each project to the provider behind its Git connection and
    /// select that provider's type as one extra column.
    ///
    /// Neither hop can fan a project out into several rows (both are
    /// many-to-one), so this changes the row count of no query it is applied
    /// to, and it never needs a second round trip to fill the field in.
    fn with_git_provider_type(
        select: sea_orm::Select<projects::Entity>,
    ) -> sea_orm::Select<projects::Entity> {
        select
            .join(
                sea_orm::JoinType::LeftJoin,
                projects::Relation::GitProviderConnection.def(),
            )
            .join(
                sea_orm::JoinType::LeftJoin,
                git_provider_connections::Relation::Provider.def(),
            )
            .column_as(git_providers::Column::ProviderType, GIT_PROVIDER_TYPE_ALIAS)
    }

    fn map_db_project_row_to_project(row: ProjectWithGitProviderType) -> Project {
        let mut project = Self::map_db_project_to_project(row.project);
        project.git_provider_type = row.git_provider_type;
        project
    }

    /// Build the client-facing `Project` for a row a mutation just wrote.
    ///
    /// Writes return an updated `projects::Model` rather than going back
    /// through the joined read query, so the provider type is fetched here —
    /// one primary-key lookup, and none at all for a project with no
    /// connection. Answering a successful update with `git_provider_type:
    /// null` would tell a client that a connected project has no provider, and
    /// any client that trusts its own mutation response would downgrade a
    /// GitLab project back to a guess until its next read.
    ///
    /// Deliberately infallible. This runs *after* the write has committed, and
    /// it reads a display label — failing here would report a change the
    /// database has already accepted as a failed request, and would skip the
    /// `ProjectUpdated` job that several callers emit straight after this call,
    /// leaving routes and notifications unaware of a change that did happen. A
    /// lookup that fails is logged and leaves the field empty; the next read
    /// fills it in.
    async fn map_written_project(&self, db_project: projects::Model) -> Project {
        let git_provider_type = match db_project.git_provider_connection_id {
            None => None,
            Some(connection_id) => {
                match git_provider_connections::Entity::find_by_id(connection_id)
                    .select_only()
                    .column(git_providers::Column::ProviderType)
                    .inner_join(git_providers::Entity)
                    .into_tuple::<String>()
                    .one(self.db.as_ref())
                    .await
                {
                    Ok(provider_type) => provider_type,
                    Err(e) => {
                        warn!(
                            "Failed to read the git provider type for connection {} of project {} \
                             after a successful write; responding without it: {}",
                            connection_id, db_project.id, e
                        );
                        None
                    }
                }
            }
        };

        let mut project = Self::map_db_project_to_project(db_project);
        project.git_provider_type = git_provider_type;
        project
    }

    /// Maps the project row alone, leaving `git_provider_type` empty.
    ///
    /// Private on purpose: every client-facing path must go through
    /// `map_db_project_row_to_project` (joined read) or `map_written_project`
    /// (mutation), so a new endpoint cannot quietly answer with "no provider"
    /// for a connected project.
    fn map_db_project_to_project(db_project: projects::Model) -> Project {
        // Extract deployment config fields
        let deployment_config = db_project.deployment_config.clone();

        // Convert preset to the runtime/UI slug (reconstructs nixpacks-{provider})
        let preset_str =
            temps_presets::runtime_slug(db_project.preset, db_project.preset_config.as_ref());

        // Handle repo_name and repo_owner - return None for empty strings (Git-less projects)
        let repo_name = if db_project.repo_name.is_empty() {
            None
        } else {
            Some(db_project.repo_name)
        };
        let repo_owner = if db_project.repo_owner.is_empty() {
            None
        } else {
            Some(db_project.repo_owner)
        };

        // Serialize preset_config to JSON value for the response
        let preset_config_json = db_project
            .preset_config
            .as_ref()
            .and_then(|config| serde_json::to_value(config).ok());

        let service_template = db_project.service_template.as_ref().and_then(|snapshot| {
            serde_json::from_value::<temps_core::templates::ServiceTemplateInstance>(
                snapshot.clone(),
            )
            .ok()
            .filter(|instance| instance.validate().is_ok())
        });
        let service_template_image_url = service_template
            .as_ref()
            .and_then(|instance| instance.template.image_url.clone());
        let service_template_version = service_template.map(|instance| instance.version);

        Project {
            id: db_project.id,
            slug: db_project.slug,
            name: db_project.name,
            repo_name,
            repo_owner,
            directory: db_project.directory,
            pull_only_root_directory: db_project.pull_only_root_directory,
            main_branch: db_project.main_branch,
            preset: (db_project.source_type != temps_entities::source_type::SourceType::External)
                .then_some(preset_str),
            template_slug: db_project.template_slug,
            service_template_image_url,
            service_template_version,
            preset_config: preset_config_json,
            created_at: db_project.created_at,
            updated_at: db_project.updated_at,
            automatic_deploy: deployment_config
                .clone()
                .and_then(|c| c.automatic_deploy)
                .unwrap_or(false),
            cpu_request: deployment_config.clone().and_then(|c| c.cpu_request),
            cpu_limit: deployment_config.clone().and_then(|c| c.cpu_limit),
            memory_request: deployment_config.clone().and_then(|c| c.memory_request),
            memory_limit: deployment_config.clone().and_then(|c| c.memory_limit),
            performance_metrics_enabled: deployment_config
                .clone()
                .map(|c| c.performance_metrics_enabled)
                .unwrap_or(false),
            last_deployment: db_project.last_deployment,
            project_type: db_project.project_type.to_string(),
            use_default_wildcard: true, // Deprecated field, always true
            custom_domain: None,        // Deprecated field, use project_domains table
            is_public_repo: db_project.is_public_repo,
            git_url: db_project.git_url,
            git_provider_connection_id: db_project.git_provider_connection_id,
            // Lives on the connection's provider row, not on the project row,
            // so it is filled in by the caller: `map_db_project_row_to_project`
            // from the joined read query, or `map_written_project` after a
            // mutation. Both are the only ways a `Project` reaches a client.
            git_provider_type: None,
            is_on_demand: false, // Deprecated field, default to false
            deployment_config: deployment_config.clone(),
            attack_mode: db_project.attack_mode,
            shield_mode: db_project.shield_mode,
            ai_alert_summaries_enabled: db_project.ai_alert_summaries_enabled,
            ai_api_traffic_summary_enabled: db_project.ai_api_traffic_summary_enabled,
            error_source_context_enabled: db_project.error_source_context_enabled,
            vulnerability_scanning_enabled: db_project.vulnerability_scanning_enabled,
            error_source_root: db_project.error_source_root,
            enable_preview_environments: db_project.enable_preview_environments,
            preview_envs_on_demand: db_project.preview_envs_on_demand,
            preview_envs_idle_timeout_seconds: db_project.preview_envs_idle_timeout_seconds,
            preview_envs_wake_timeout_seconds: db_project.preview_envs_wake_timeout_seconds,
            source_type: db_project.source_type,
            allow_alternate_sources: db_project.allow_alternate_sources,
            hosting: db_project.hosting,
            gitlab_webhook_id: db_project.gitlab_webhook_id,
            github_webhook_id: db_project.github_webhook_id,
            cross_project_trace_sharing: db_project.cross_project_trace_sharing,
            serves_own_temps_api: db_project.serves_own_temps_api,
            image_retention_hours: db_project.image_retention_hours,
            log_ingest_lines_per_sec: db_project.log_ingest_lines_per_sec,
            log_ingest_kib_per_sec: db_project.log_ingest_kib_per_sec,
        }
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

#[cfg(test)]
mod cloudflare_creation_tests {
    use super::{chosen_delivery_provider, DeliveryChoice};

    fn choose(
        explicit: Option<&str>,
        legacy: Option<bool>,
        cloudflare_default: bool,
        bunny_default: bool,
    ) -> DeliveryChoice {
        chosen_delivery_provider(explicit, legacy, cloudflare_default, bunny_default)
            .expect("valid choice")
    }

    #[test]
    fn explicit_choice_overrides_future_project_default() {
        assert_eq!(
            choose(Some("cloudflare"), None, false, true),
            DeliveryChoice::explicit(Some("cloudflare"))
        );
        assert_eq!(
            choose(Some("none"), None, true, false),
            DeliveryChoice::explicit(None)
        );
        assert_eq!(
            choose(None, Some(true), false, false),
            DeliveryChoice::explicit(Some("cloudflare"))
        );
        assert_eq!(
            choose(None, Some(false), true, false),
            DeliveryChoice::explicit(None)
        );
        assert!(chosen_delivery_provider(Some("invalid"), None, false, false).is_err());
    }

    #[test]
    fn absent_choice_inherits_future_project_default() {
        assert_eq!(
            choose(None, None, true, false),
            DeliveryChoice::inherited(Some("cloudflare"))
        );
        assert_eq!(
            choose(None, None, false, true),
            DeliveryChoice::inherited(Some("bunny"))
        );
        assert_eq!(
            choose(None, None, false, false),
            DeliveryChoice::inherited(None)
        );
    }

    /// The legacy flag only ever meant "Cloudflare on/off"; opting out of
    /// Cloudflare must not also switch off an instance-wide Bunny default.
    #[test]
    fn legacy_cloudflare_opt_out_keeps_bunny_default() {
        assert_eq!(
            choose(None, Some(false), false, true),
            DeliveryChoice::inherited(Some("bunny"))
        );
        assert_eq!(
            choose(None, Some(false), true, true),
            DeliveryChoice::inherited(Some("bunny"))
        );
    }
}
