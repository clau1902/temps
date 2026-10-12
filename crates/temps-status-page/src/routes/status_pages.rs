// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Authenticated management of publishable status pages (#1412).
//!
//! Authorization follows ADR-028: every route resolves the owning project and
//! runs `project_permission_guard!`, so a caller needs the permission on *that
//! project*. Changing what is public (create, update/selection, publish,
//! unpublish, URL rotation, delete) needs `status_page:create`,
//! `status_page:write` or `status_page:delete`, which on a team-scoped project
//! start at the project **admin** role. By-id routes run the instance-wide
//! permission check before the ownership lookup, so a forbidden caller cannot
//! tell a missing id from another project's page.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use temps_auth::{permission_guard, project_permission_guard, project_scope_guard, RequireAuth};
use temps_core::problemdetails::Problem;
use temps_core::RequestMetadata;

use super::audit::{PublicStatusPageAction, PublicStatusPageAudit};
use super::status_page::{map_error, StatusPageAppState};
use crate::services::public_pages::{
    CreateStatusPageRequest, StatusPageListResponse, StatusPageResponse, UpdateStatusPageRequest,
};

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct StatusPageListQuery {
    /// Page number, starting at 1 (default 1).
    pub page: Option<u64>,
    /// Items per page (default 20, max 100).
    pub page_size: Option<u64>,
}

fn audit_for(
    auth: &temps_auth::AuthContext,
    metadata: &RequestMetadata,
    action: PublicStatusPageAction,
    page: &StatusPageResponse,
    include_selection: bool,
) -> PublicStatusPageAudit {
    PublicStatusPageAudit {
        actor_user_id: auth.user_id_opt(),
        ip_address: Some(metadata.ip_address.clone()),
        user_agent: metadata.user_agent.clone(),
        action,
        project_id: page.project_id,
        status_page_id: page.id,
        title: Some(page.title.clone()),
        is_published: Some(page.is_published),
        monitor_ids: include_selection
            .then(|| page.monitors.iter().map(|m| m.monitor_id).collect()),
    }
}

async fn record_audit<T: StatusPageAppState>(app_state: &T, audit: PublicStatusPageAudit) {
    if let Err(error) = app_state.audit_service().create_audit_log(&audit).await {
        tracing::error!(
            project_id = audit.project_id,
            status_page_id = audit.status_page_id,
            action = ?audit.action,
            error = %error,
            "failed to persist status page audit"
        );
    }
}

/// List a project's status pages
#[utoipa::path(
    get,
    path = "/projects/{project_id}/status-pages",
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        StatusPageListQuery,
    ),
    responses(
        (status = 200, description = "Status pages, newest first", body = StatusPageListResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn list_status_pages<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(project_id): Path<i32>,
    Query(query): Query<StatusPageListQuery>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    project_permission_guard!(
        auth,
        StatusPageRead,
        project_id,
        app_state.project_access_checker()
    );
    project_scope_guard!(auth, project_id);
    app_state
        .public_status_page_service()
        .list(project_id, query.page, query.page_size)
        .await
        .map(Json)
        .map_err(map_error)
}

/// Create a status page (unpublished)
#[utoipa::path(
    post,
    path = "/projects/{project_id}/status-pages",
    request_body = CreateStatusPageRequest,
    params(("project_id" = i32, Path, description = "Project ID")),
    responses(
        (status = 201, description = "Status page created, not yet published", body = StatusPageResponse),
        (status = 400, description = "Invalid request or monitor from another project"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 409, description = "Project already has the maximum number of status pages"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn create_status_page<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(project_id): Path<i32>,
    Json(request): Json<CreateStatusPageRequest>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    project_permission_guard!(
        auth,
        StatusPageCreate,
        project_id,
        app_state.project_access_checker()
    );
    project_scope_guard!(auth, project_id);
    let page = app_state
        .public_status_page_service()
        .create(project_id, request)
        .await
        .map_err(map_error)?;
    record_audit(
        app_state.as_ref(),
        audit_for(
            &auth,
            &metadata,
            PublicStatusPageAction::Created,
            &page,
            true,
        ),
    )
    .await;
    Ok((StatusCode::CREATED, Json(page)))
}

/// Get a status page
#[utoipa::path(
    get,
    path = "/status-pages/{status_page_id}",
    params(("status_page_id" = i32, Path, description = "Status page ID")),
    responses(
        (status = 200, description = "Status page", body = StatusPageResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Status page not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_status_page<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(status_page_id): Path<i32>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    let service = app_state.public_status_page_service();
    let project_id = service
        .get_project_id(status_page_id)
        .await
        .map_err(map_error)?;
    project_permission_guard!(
        auth,
        StatusPageRead,
        project_id,
        app_state.project_access_checker()
    );
    project_scope_guard!(auth, project_id);
    service
        .get(status_page_id)
        .await
        .map(Json)
        .map_err(map_error)
}

/// Update a status page's title, description or monitor selection
#[utoipa::path(
    patch,
    path = "/status-pages/{status_page_id}",
    request_body = UpdateStatusPageRequest,
    params(("status_page_id" = i32, Path, description = "Status page ID")),
    responses(
        (status = 200, description = "Status page updated; a published page is re-rendered immediately", body = StatusPageResponse),
        (status = 400, description = "Invalid request or monitor from another project"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Status page not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn update_status_page<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(status_page_id): Path<i32>,
    Json(request): Json<UpdateStatusPageRequest>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageWrite);
    let service = app_state.public_status_page_service();
    let project_id = service
        .get_project_id(status_page_id)
        .await
        .map_err(map_error)?;
    project_permission_guard!(
        auth,
        StatusPageWrite,
        project_id,
        app_state.project_access_checker()
    );
    project_scope_guard!(auth, project_id);
    let selection_changed = request.monitors.is_some();
    let page = service
        .update(status_page_id, request)
        .await
        .map_err(map_error)?;
    record_audit(
        app_state.as_ref(),
        audit_for(
            &auth,
            &metadata,
            PublicStatusPageAction::Updated,
            &page,
            selection_changed,
        ),
    )
    .await;
    Ok(Json(page))
}

/// Publish a status page at its public URL
#[utoipa::path(
    post,
    path = "/status-pages/{status_page_id}/publish",
    params(("status_page_id" = i32, Path, description = "Status page ID")),
    responses(
        (status = 200, description = "Status page published", body = StatusPageResponse),
        (status = 400, description = "The page has no monitors selected"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Status page not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn publish_status_page<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(status_page_id): Path<i32>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageWrite);
    let service = app_state.public_status_page_service();
    let project_id = service
        .get_project_id(status_page_id)
        .await
        .map_err(map_error)?;
    project_permission_guard!(
        auth,
        StatusPageWrite,
        project_id,
        app_state.project_access_checker()
    );
    project_scope_guard!(auth, project_id);
    let page = service.publish(status_page_id).await.map_err(map_error)?;
    app_state
        .telemetry()
        .report(temps_core::TelemetryEvent::new(
            temps_core::TelemetryEventKind::StatusPagePublished,
        ));
    record_audit(
        app_state.as_ref(),
        audit_for(
            &auth,
            &metadata,
            PublicStatusPageAction::Published,
            &page,
            true,
        ),
    )
    .await;
    Ok(Json(page))
}

/// Unpublish a status page; its public URL returns 404 immediately
#[utoipa::path(
    post,
    path = "/status-pages/{status_page_id}/unpublish",
    params(("status_page_id" = i32, Path, description = "Status page ID")),
    responses(
        (status = 200, description = "Status page unpublished", body = StatusPageResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Status page not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn unpublish_status_page<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(status_page_id): Path<i32>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageWrite);
    let service = app_state.public_status_page_service();
    let project_id = service
        .get_project_id(status_page_id)
        .await
        .map_err(map_error)?;
    project_permission_guard!(
        auth,
        StatusPageWrite,
        project_id,
        app_state.project_access_checker()
    );
    project_scope_guard!(auth, project_id);
    let page = service.unpublish(status_page_id).await.map_err(map_error)?;
    record_audit(
        app_state.as_ref(),
        audit_for(
            &auth,
            &metadata,
            PublicStatusPageAction::Unpublished,
            &page,
            false,
        ),
    )
    .await;
    Ok(Json(page))
}

/// Replace a status page's public URL; the old URL returns 404 immediately
#[utoipa::path(
    post,
    path = "/status-pages/{status_page_id}/rotate-url",
    params(("status_page_id" = i32, Path, description = "Status page ID")),
    responses(
        (status = 200, description = "Public URL replaced", body = StatusPageResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Status page not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn rotate_status_page_url<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(status_page_id): Path<i32>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageWrite);
    let service = app_state.public_status_page_service();
    let project_id = service
        .get_project_id(status_page_id)
        .await
        .map_err(map_error)?;
    project_permission_guard!(
        auth,
        StatusPageWrite,
        project_id,
        app_state.project_access_checker()
    );
    project_scope_guard!(auth, project_id);
    let page = service
        .rotate_slug(status_page_id)
        .await
        .map_err(map_error)?;
    record_audit(
        app_state.as_ref(),
        audit_for(
            &auth,
            &metadata,
            PublicStatusPageAction::UrlRotated,
            &page,
            false,
        ),
    )
    .await;
    Ok(Json(page))
}

/// Delete a status page; its public URL returns 404 immediately
#[utoipa::path(
    delete,
    path = "/status-pages/{status_page_id}",
    params(("status_page_id" = i32, Path, description = "Status page ID")),
    responses(
        (status = 204, description = "Status page deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Status page not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn delete_status_page<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(status_page_id): Path<i32>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageDelete);
    let service = app_state.public_status_page_service();
    let page = service.get(status_page_id).await.map_err(map_error)?;
    project_permission_guard!(
        auth,
        StatusPageDelete,
        page.project_id,
        app_state.project_access_checker()
    );
    project_scope_guard!(auth, page.project_id);
    service.delete(status_page_id).await.map_err(map_error)?;
    let mut audit = audit_for(
        &auth,
        &metadata,
        PublicStatusPageAction::Deleted,
        &page,
        false,
    );
    audit.is_published = Some(false);
    record_audit(app_state.as_ref(), audit).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::public::{create_public_router, PublicStatusState};
    use crate::services::public_pages::{StatusPageMonitorInput, SLUG_LEN};
    use crate::services::{
        CreateIncidentRequest, CreateMonitorRequest, PublicStatusPageService, StatusPageService,
        StatusSnapshotCache, UpdateIncidentStatusRequest,
    };
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use temps_auth::permissions::{Permission, Role};
    use temps_auth::AuthContext;
    use temps_config::ConfigService;
    use temps_core::{
        AuditLogger, AuditOperation, NoopTelemetryReporter, ProjectAccessChecker, TelemetryReporter,
    };
    use temps_database::test_utils::{is_container_runtime_unavailable, TestDatabase};
    use temps_entities::{environments, projects, upstream_config::UpstreamList, users};
    use tower::ServiceExt;

    #[derive(Default)]
    struct RecordingAuditLogger {
        entries: Mutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait]
    impl AuditLogger for RecordingAuditLogger {
        async fn create_audit_log(&self, operation: &dyn AuditOperation) -> anyhow::Result<()> {
            let payload: serde_json::Value = serde_json::from_str(&operation.serialize()?)?;
            self.entries
                .lock()
                .map_err(|_| anyhow::anyhow!("audit log mutex poisoned"))?
                .push((operation.operation_type(), payload));
            Ok(())
        }
    }

    struct Checker {
        allowed: Vec<i32>,
        permissions: Option<Vec<String>>,
    }

    #[async_trait]
    impl ProjectAccessChecker for Checker {
        async fn user_can_access_project(
            &self,
            _user_id: i32,
            project_id: i32,
        ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.allowed.contains(&project_id))
        }

        async fn effective_project_permissions(
            &self,
            _user_id: i32,
            project_id: i32,
        ) -> Result<Option<Vec<String>>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(if self.allowed.contains(&project_id) {
                self.permissions.clone()
            } else {
                Some(Vec::new())
            })
        }
    }

    struct TestState {
        status_page_service: StatusPageService,
        public_status_page_service: PublicStatusPageService,
        telemetry: Arc<dyn TelemetryReporter>,
        audit_service: Arc<dyn AuditLogger>,
        checker: Option<Arc<dyn ProjectAccessChecker>>,
    }

    impl StatusPageAppState for TestState {
        fn status_page_service(&self) -> &StatusPageService {
            &self.status_page_service
        }
        fn public_status_page_service(&self) -> &PublicStatusPageService {
            &self.public_status_page_service
        }
        fn telemetry(&self) -> &Arc<dyn TelemetryReporter> {
            &self.telemetry
        }
        fn audit_service(&self) -> &Arc<dyn AuditLogger> {
            &self.audit_service
        }
        fn project_access_checker(&self) -> Option<Arc<dyn ProjectAccessChecker>> {
            self.checker.clone()
        }
    }

    /// Two projects, each with an environment and monitors. Project A has a
    /// selected monitor and an unselected one whose names, check errors and
    /// incidents carry recognisable markers that must never reach the public.
    struct Fx {
        _db: TestDatabase,
        state: Arc<TestState>,
        public: axum::Router,
        audit: Arc<RecordingAuditLogger>,
        project_a: i32,
        project_b: i32,
        selected_monitor: i32,
        unselected_monitor: i32,
        other_project_monitor: i32,
        selected_incident: i32,
    }

    const SELECTED_INTERNAL_NAME: &str = "internal-selected-monitor-name";
    const UNSELECTED_INTERNAL_NAME: &str = "internal-unselected-monitor-name";
    const PROJECT_NAME: &str = "Internal Project Name";
    const PRIVATE_IP: &str = "10.11.12.13";
    const LEAK_MARKERS: &[&str] = &[
        SELECTED_INTERNAL_NAME,
        UNSELECTED_INTERNAL_NAME,
        PROJECT_NAME,
        PRIVATE_IP,
        "internal-env",
        "internal.example.test",
        "PRIVATE-UPDATE-TEXT",
        "UNSELECTED-PUBLIC-UPDATE",
        "Internal incident title",
        "probe error detail",
        "/healthz",
    ];

    fn user() -> users::Model {
        let now = chrono::Utc::now();
        users::Model {
            id: 1,
            name: "Test User".to_string(),
            email: "test@example.com".to_string(),
            password_hash: None,
            email_verified: true,
            email_verification_token: None,
            email_verification_expires: None,
            password_reset_token: None,
            password_reset_expires: None,
            must_change_password: false,
            deleted_at: None,
            mfa_secret: None,
            mfa_enabled: false,
            mfa_recovery_codes: None,
            oidc_subject: None,
            oidc_provider_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn admin() -> RequireAuth {
        RequireAuth(AuthContext::new_session(user(), Role::Admin))
    }

    fn member() -> RequireAuth {
        RequireAuth(AuthContext::new_session(user(), Role::User))
    }

    fn metadata() -> Extension<RequestMetadata> {
        Extension(RequestMetadata {
            ip_address: "192.0.2.30".to_string(),
            user_agent: "status-pages-test".to_string(),
            headers: axum::http::HeaderMap::new(),
            visitor_id_cookie: None,
            session_id_cookie: None,
            base_url: "http://localhost".to_string(),
            scheme: "http".to_string(),
            host: "localhost".to_string(),
            is_secure: false,
        })
    }

    async fn insert_project(db: &Arc<sea_orm::DatabaseConnection>, tag: &str) -> i32 {
        let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let slug = format!("{tag}-{nanos}");
        projects::ActiveModel {
            name: Set(PROJECT_NAME.to_string()),
            slug: Set(slug.clone()),
            directory: Set(slug),
            main_branch: Set("main".to_string()),
            preset: Set(temps_entities::preset::Preset::Nixpacks),
            repo_name: Set("repo".to_string()),
            repo_owner: Set("owner".to_string()),
            ..Default::default()
        }
        .insert(db.as_ref())
        .await
        .expect("insert project")
        .id
    }

    async fn insert_environment(
        db: &Arc<sea_orm::DatabaseConnection>,
        project_id: i32,
        name: &str,
    ) -> i32 {
        let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let slug = format!("{name}-{nanos}");
        environments::ActiveModel {
            project_id: Set(project_id),
            name: Set(slug.clone()),
            slug: Set(slug.clone()),
            subdomain: Set(slug),
            host: Set("internal.example.test".to_string()),
            upstreams: Set(UpstreamList::default()),
            ..Default::default()
        }
        .insert(db.as_ref())
        .await
        .expect("insert environment")
        .id
    }

    async fn fixture(
        checker: impl FnOnce(i32) -> Option<Arc<dyn ProjectAccessChecker>>,
    ) -> Option<Fx> {
        let test_db = match TestDatabase::with_migrations().await {
            Ok(db) => db,
            Err(e) if is_container_runtime_unavailable(&e.to_string()) => {
                println!("Docker not available, skipping status page integration test");
                return None;
            }
            Err(e) => panic!("test database setup failed: {e}"),
        };
        let db = test_db.connection_arc();
        let config = temps_config::ServerConfig::new(
            "127.0.0.1:3000".to_string(),
            "postgres://test:test@localhost/test".to_string(),
            None,
            None,
        )
        .expect("server config");
        let config_service = Arc::new(ConfigService::new(Arc::new(config), db.clone()));
        let cache = Arc::new(StatusSnapshotCache::new(db.clone()));
        let status_page_service = StatusPageService::new(db.clone(), config_service.clone());
        let public_status_page_service =
            PublicStatusPageService::new(db.clone(), config_service, cache.clone());

        let project_a = insert_project(&db, "status-a").await;
        let project_b = insert_project(&db, "status-b").await;
        let env_a = insert_environment(&db, project_a, "internal-env").await;
        let env_a2 = insert_environment(&db, project_a, "internal-env-other").await;
        let env_b = insert_environment(&db, project_b, "internal-env-b").await;

        let monitors = status_page_service.monitor_service();
        let create = |name: &str, environment_id: i32| CreateMonitorRequest {
            name: name.to_string(),
            monitor_type: "web".to_string(),
            environment_id,
            check_interval_seconds: Some(60),
            check_path: Some("/healthz".to_string()),
        };
        let selected = monitors
            .create_monitor(project_a, create(SELECTED_INTERNAL_NAME, env_a))
            .await
            .expect("selected monitor");
        let unselected = monitors
            .create_monitor(project_a, create(UNSELECTED_INTERNAL_NAME, env_a2))
            .await
            .expect("unselected monitor");
        let other = monitors
            .create_monitor(project_b, create("other-project-monitor", env_b))
            .await
            .expect("other project monitor");

        for (monitor_id, status) in [
            (selected.id, "operational"),
            (selected.id, "major_outage"),
            (selected.id, "operational"),
            (unselected.id, "major_outage"),
        ] {
            monitors
                .record_check(
                    monitor_id,
                    status.to_string(),
                    Some(42),
                    Some(format!("probe error detail from {PRIVATE_IP}")),
                )
                .await
                .expect("record check");
        }

        let incidents = status_page_service.incident_service();
        let selected_incident = incidents
            .create_incident(
                project_a,
                CreateIncidentRequest {
                    title: "Internal incident title".to_string(),
                    description: Some(format!("description mentions {PRIVATE_IP}")),
                    severity: "major".to_string(),
                    environment_id: Some(env_a),
                    monitor_id: Some(selected.id),
                },
            )
            .await
            .expect("selected incident");
        for (message, is_public) in [
            ("PRIVATE-UPDATE-TEXT", false),
            ("We are investigating elevated errors.", true),
        ] {
            incidents
                .update_incident_status(
                    selected_incident.id,
                    UpdateIncidentStatusRequest {
                        status: "identified".to_string(),
                        message: message.to_string(),
                        is_public,
                    },
                )
                .await
                .expect("incident update");
        }
        let unselected_incident = incidents
            .create_incident(
                project_a,
                CreateIncidentRequest {
                    title: "Internal incident title".to_string(),
                    description: None,
                    severity: "critical".to_string(),
                    environment_id: Some(env_a2),
                    monitor_id: Some(unselected.id),
                },
            )
            .await
            .expect("unselected incident");
        incidents
            .update_incident_status(
                unselected_incident.id,
                UpdateIncidentStatusRequest {
                    status: "identified".to_string(),
                    message: "UNSELECTED-PUBLIC-UPDATE".to_string(),
                    is_public: true,
                },
            )
            .await
            .expect("unselected incident update");

        let audit = Arc::new(RecordingAuditLogger::default());
        let state = Arc::new(TestState {
            status_page_service,
            public_status_page_service,
            telemetry: Arc::new(NoopTelemetryReporter),
            audit_service: audit.clone(),
            checker: checker(project_a),
        });
        let public = create_public_router(Arc::new(PublicStatusState::new(cache)));
        Some(Fx {
            _db: test_db,
            state,
            public,
            audit,
            project_a,
            project_b,
            selected_monitor: selected.id,
            unselected_monitor: unselected.id,
            other_project_monitor: other.id,
            selected_incident: selected_incident.id,
        })
    }

    async fn fetch(
        router: &axum::Router,
        path: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("router is infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, headers, String::from_utf8_lossy(&body).to_string())
    }

    async fn create_page(fx: &Fx, monitors: Vec<StatusPageMonitorInput>) -> StatusPageResponse {
        let response = create_status_page(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path(fx.project_a),
            Json(CreateStatusPageRequest {
                title: "Example status".to_string(),
                description: Some("Live service health".to_string()),
                monitors,
            }),
        )
        .await
        .map_err(|p| format!("{:?}", p.body))
        .expect("create status page")
        .into_response();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        serde_json::from_slice(&body).expect("status page json")
    }

    async fn decode<T: serde::de::DeserializeOwned>(response: impl IntoResponse) -> T {
        let body = axum::body::to_bytes(response.into_response().into_body(), 1 << 20)
            .await
            .expect("body");
        serde_json::from_slice(&body).expect("json body")
    }

    fn api_selection(fx: &Fx) -> Vec<StatusPageMonitorInput> {
        vec![StatusPageMonitorInput {
            monitor_id: fx.selected_monitor,
            display_name: "API".to_string(),
        }]
    }

    async fn publish(fx: &Fx, id: i32) -> StatusPageResponse {
        decode(
            publish_status_page(admin(), State(fx.state.clone()), metadata(), Path(id))
                .await
                .map_err(|p| format!("{:?}", p.body))
                .expect("publish"),
        )
        .await
    }

    fn collect_keys(value: &serde_json::Value, keys: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    keys.insert(k.clone());
                    collect_keys(v, keys);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| collect_keys(v, keys)),
            _ => {}
        }
    }

    fn assert_no_leaks(body: &str) {
        for marker in LEAK_MARKERS {
            assert!(
                !body.contains(marker),
                "public output leaked {marker:?}:\n{body}"
            );
        }
    }

    #[tokio::test]
    async fn published_page_shows_only_selected_monitors_and_public_updates() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let page = create_page(&fx, api_selection(&fx)).await;
        assert_eq!(page.slug.len(), SLUG_LEN);
        assert!(!page.is_published);
        assert!(page
            .public_url
            .ends_with(&format!("/api/status/{}", page.slug)));
        assert_eq!(page.monitors[0].monitor_name, SELECTED_INTERNAL_NAME);

        // Draft pages are not served.
        let html_path = format!("/status/{}", page.slug);
        let json_path = format!("/status/{}/summary.json", page.slug);
        assert_eq!(fetch(&fx.public, &html_path).await.0, StatusCode::NOT_FOUND);
        assert_eq!(fetch(&fx.public, &json_path).await.0, StatusCode::NOT_FOUND);

        publish(&fx, page.id).await;

        let (status, headers, json) = fetch(&fx.public, &json_path).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[axum::http::header::CACHE_CONTROL], "no-store");
        assert_eq!(
            headers[axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "*"
        );
        assert_no_leaks(&json);
        let value: serde_json::Value = serde_json::from_str(&json).expect("public json");
        let mut keys = BTreeSet::new();
        collect_keys(&value, &mut keys);
        let allowed: BTreeSet<String> = [
            "title",
            "description",
            "status",
            "components",
            "incidents",
            "history_days",
            "generated_at",
            "name",
            "uptime_percentage",
            "days",
            "date",
            "severity",
            "affected_components",
            "started_at",
            "resolved_at",
            "updates",
            "message",
            "created_at",
        ]
        .iter()
        .map(|k| k.to_string())
        .collect();
        assert!(
            keys.is_subset(&allowed),
            "unexpected public keys: {:?}",
            keys.difference(&allowed).collect::<Vec<_>>()
        );
        assert_eq!(value["components"].as_array().map(Vec::len), Some(1));
        assert_eq!(value["components"][0]["name"], "API");
        assert_eq!(value["components"][0]["status"], "operational");
        let incidents = value["incidents"].as_array().expect("incidents");
        assert_eq!(incidents.len(), 1, "only the selected monitor's incident");
        assert_eq!(
            incidents[0]["updates"].as_array().map(Vec::len),
            Some(1),
            "only the public update"
        );
        assert_eq!(
            incidents[0]["updates"][0]["message"],
            "We are investigating elevated errors."
        );

        let (status, headers, html) = fetch(&fx.public, &html_path).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[axum::http::header::CONTENT_SECURITY_POLICY]
            .to_str()
            .expect("csp")
            .starts_with("default-src 'none'"));
        assert_eq!(
            headers[axum::http::header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert!(html.contains("API"));
        assert!(html.contains("We are investigating elevated errors."));
        assert!(!html.to_ascii_lowercase().contains("<script"));
        assert_no_leaks(&html);
    }

    #[tokio::test]
    async fn unpublish_rotate_and_delete_take_the_url_down_immediately() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let page = create_page(&fx, api_selection(&fx)).await;
        publish(&fx, page.id).await;
        let path = format!("/status/{}", page.slug);
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::OK);

        unpublish_status_page(admin(), State(fx.state.clone()), metadata(), Path(page.id))
            .await
            .map_err(|p| format!("{:?}", p.body))
            .expect("unpublish");
        let (status, _, unpublished_body) = fetch(&fx.public, &path).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // A background refresh must not resurrect it.
        fx.state
            .public_status_page_service
            .refresh_project_pages(fx.project_a)
            .await;
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::NOT_FOUND);

        publish(&fx, page.id).await;
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::OK);
        let rotated: StatusPageResponse = decode(
            rotate_status_page_url(admin(), State(fx.state.clone()), metadata(), Path(page.id))
                .await
                .map_err(|p| format!("{:?}", p.body))
                .expect("rotate"),
        )
        .await;
        assert_ne!(rotated.slug, page.slug);
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::NOT_FOUND);
        let new_path = format!("/status/{}", rotated.slug);
        assert_eq!(fetch(&fx.public, &new_path).await.0, StatusCode::OK);

        delete_status_page(admin(), State(fx.state.clone()), metadata(), Path(page.id))
            .await
            .map_err(|p| format!("{:?}", p.body))
            .expect("delete");
        let (status, _, deleted_body) = fetch(&fx.public, &new_path).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(deleted_body, unpublished_body);
        assert!(
            get_status_page(admin(), State(fx.state.clone()), Path(page.id))
                .await
                .is_err()
        );

        let actions: Vec<String> = fx
            .audit
            .entries
            .lock()
            .expect("audit entries")
            .iter()
            .map(|(op, _)| op.clone())
            .collect();
        for expected in [
            "STATUS_PAGE_CREATED",
            "STATUS_PAGE_PUBLISHED",
            "STATUS_PAGE_UNPUBLISHED",
            "STATUS_PAGE_URL_ROTATED",
            "STATUS_PAGE_DELETED",
        ] {
            assert!(
                actions.iter().any(|a| a == expected),
                "missing audit {expected}: {actions:?}"
            );
        }
        let published = fx
            .audit
            .entries
            .lock()
            .expect("audit entries")
            .iter()
            .find(|(op, _)| op == "STATUS_PAGE_PUBLISHED")
            .map(|(_, payload)| payload.clone())
            .expect("publish audit");
        assert_eq!(
            published["monitor_ids"],
            serde_json::json!([fx.selected_monitor])
        );
        assert!(published.get("slug").is_none());
    }

    #[tokio::test]
    async fn enumeration_attempts_get_identical_404s() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let published = create_page(&fx, api_selection(&fx)).await;
        publish(&fx, published.id).await;
        let draft = create_page(&fx, api_selection(&fx)).await;

        let (_, _, reference_html) =
            fetch(&fx.public, &format!("/status/{}", "0".repeat(SLUG_LEN))).await;
        let (_, _, reference_json) = fetch(
            &fx.public,
            &format!("/status/{}/summary.json", "0".repeat(SLUG_LEN)),
        )
        .await;
        let candidates = [
            draft.slug.clone(),                         // real but unpublished
            published.slug.to_ascii_uppercase(),        // case variant
            format!("{}0", published.slug),             // one char too long
            published.slug[..SLUG_LEN - 1].to_string(), // one char short
            published.id.to_string(),                   // internal id
            fx.project_a.to_string(),
            fx.selected_monitor.to_string(),
            fx.unselected_monitor.to_string(),
            "..%2F..%2Fprojects".to_string(),
            "%FF%FE".to_string(), // does not decode as UTF-8
        ];
        for candidate in candidates {
            let (status, headers, body) = fetch(&fx.public, &format!("/status/{candidate}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{candidate}");
            assert_eq!(body, reference_html, "{candidate}");
            assert_eq!(headers[axum::http::header::CACHE_CONTROL], "no-store");
            let (status, _, body) =
                fetch(&fx.public, &format!("/status/{candidate}/summary.json")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{candidate}");
            assert_eq!(body, reference_json, "{candidate}");
        }
        assert_eq!(
            fetch(&fx.public, &format!("/status/{}", published.slug))
                .await
                .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn narrowing_the_selection_removes_a_monitor_from_the_public_page_immediately() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let mut selection = api_selection(&fx);
        selection.push(StatusPageMonitorInput {
            monitor_id: fx.unselected_monitor,
            display_name: "Workers".to_string(),
        });
        let page = create_page(&fx, selection).await;
        publish(&fx, page.id).await;
        let path = format!("/status/{}/summary.json", page.slug);
        let (_, _, before) = fetch(&fx.public, &path).await;
        assert!(before.contains("Workers"));
        assert!(before.contains("UNSELECTED-PUBLIC-UPDATE"));

        update_status_page(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path(page.id),
            Json(UpdateStatusPageRequest {
                monitors: Some(api_selection(&fx)),
                ..Default::default()
            }),
        )
        .await
        .map_err(|p| format!("{:?}", p.body))
        .expect("narrow selection");
        let (_, _, after) = fetch(&fx.public, &path).await;
        assert!(!after.contains("Workers"));
        assert_no_leaks(&after);

        // Emptying a published page's selection is refused rather than
        // publishing an empty page.
        let error = update_status_page(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path(page.id),
            Json(UpdateStatusPageRequest {
                monitors: Some(Vec::new()),
                ..Default::default()
            }),
        )
        .await
        .err()
        .expect("empty selection on a published page");
        assert_eq!(error.status_code, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn incident_updates_are_private_until_marked_public() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let page = create_page(&fx, api_selection(&fx)).await;
        publish(&fx, page.id).await;
        let path = format!("/status/{}/summary.json", page.slug);

        let updates = fx
            .state
            .status_page_service
            .incident_service()
            .get_incident_updates(fx.selected_incident)
            .await
            .expect("updates");
        let private = updates
            .iter()
            .find(|u| u.message == "PRIVATE-UPDATE-TEXT")
            .expect("private update");
        assert!(!private.is_public);
        let created = updates
            .iter()
            .find(|u| u.message.starts_with("Incident created"))
            .expect("automatic first update");
        assert!(!created.is_public, "the automatic first update is private");
        assert!(created.is_system);

        // It embeds the internal incident title, so it can never be published.
        let error = crate::routes::status_page::set_incident_update_visibility(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path((fx.selected_incident, created.id)),
            Json(crate::services::UpdateIncidentUpdateVisibilityRequest { is_public: true }),
        )
        .await
        .err()
        .expect("system update cannot be made public");
        assert_eq!(error.status_code, StatusCode::BAD_REQUEST);

        crate::routes::status_page::set_incident_update_visibility(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path((fx.selected_incident, private.id)),
            Json(crate::services::UpdateIncidentUpdateVisibilityRequest { is_public: true }),
        )
        .await
        .map_err(|p| format!("{:?}", p.body))
        .expect("make update public");
        let (_, _, body) = fetch(&fx.public, &path).await;
        assert!(
            body.contains("PRIVATE-UPDATE-TEXT"),
            "now explicitly public"
        );

        // An update id from another incident cannot be toggled through this one.
        let error = crate::routes::status_page::set_incident_update_visibility(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path((fx.selected_incident, i32::MAX)),
            Json(crate::services::UpdateIncidentUpdateVisibilityRequest { is_public: true }),
        )
        .await
        .err()
        .expect("unknown update");
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn selection_and_publish_validation() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let error = create_status_page(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path(fx.project_a),
            Json(CreateStatusPageRequest {
                title: "Status".to_string(),
                description: None,
                monitors: vec![StatusPageMonitorInput {
                    monitor_id: fx.other_project_monitor,
                    display_name: "Borrowed".to_string(),
                }],
            }),
        )
        .await
        .err()
        .expect("monitor from another project");
        assert_eq!(error.status_code, StatusCode::BAD_REQUEST);
        assert!(format!("{:?}", error.body).contains(&fx.project_a.to_string()));

        let draft = create_page(&fx, Vec::new()).await;
        let error =
            publish_status_page(admin(), State(fx.state.clone()), metadata(), Path(draft.id))
                .await
                .err()
                .expect("publish without monitors");
        assert_eq!(error.status_code, StatusCode::BAD_REQUEST);

        for _ in 1..crate::services::public_pages::MAX_STATUS_PAGES_PER_PROJECT {
            create_page(&fx, Vec::new()).await;
        }
        let error = create_status_page(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path(fx.project_a),
            Json(CreateStatusPageRequest {
                title: "One too many".to_string(),
                description: None,
                monitors: Vec::new(),
            }),
        )
        .await
        .err()
        .expect("page limit");
        assert_eq!(error.status_code, StatusCode::CONFLICT);

        let list: StatusPageListResponse = decode(
            list_status_pages(
                admin(),
                State(fx.state.clone()),
                Path(fx.project_a),
                Query(StatusPageListQuery {
                    page: Some(1),
                    page_size: Some(3),
                }),
            )
            .await
            .map_err(|p| format!("{:?}", p.body))
            .expect("list"),
        )
        .await;
        assert_eq!(
            list.total,
            crate::services::public_pages::MAX_STATUS_PAGES_PER_PROJECT
        );
        assert_eq!(list.items.len(), 3);
        assert!(list.items.iter().all(|p| p.project_id == fx.project_a));
        let other: StatusPageListResponse = decode(
            list_status_pages(
                admin(),
                State(fx.state.clone()),
                Path(fx.project_b),
                Query(StatusPageListQuery {
                    page: None,
                    page_size: None,
                }),
            )
            .await
            .map_err(|p| format!("{:?}", p.body))
            .expect("list other project"),
        )
        .await;
        assert_eq!(other.total, 0);
    }

    fn assert_denied(problem: Problem) {
        assert_eq!(
            problem.status_code,
            StatusCode::FORBIDDEN,
            "{:?}",
            problem.body
        );
    }

    #[tokio::test]
    async fn changing_what_is_public_requires_project_admin_permissions() {
        let viewer = |project_id: i32| -> Option<Arc<dyn ProjectAccessChecker>> {
            Some(Arc::new(Checker {
                allowed: vec![project_id],
                permissions: Some(vec![Permission::StatusPageRead.to_string()]),
            }))
        };
        let Some(fx) = fixture(viewer).await else {
            return;
        };
        let page = create_page(&fx, api_selection(&fx)).await; // as instance admin

        // A project viewer can read the page...
        assert!(
            get_status_page(member(), State(fx.state.clone()), Path(page.id))
                .await
                .is_ok()
        );
        // ...but cannot change anything that is public.
        assert_denied(
            create_status_page(
                member(),
                State(fx.state.clone()),
                metadata(),
                Path(fx.project_a),
                Json(CreateStatusPageRequest {
                    title: "x".to_string(),
                    description: None,
                    monitors: Vec::new(),
                }),
            )
            .await
            .err()
            .expect("viewer create"),
        );
        assert_denied(
            publish_status_page(member(), State(fx.state.clone()), metadata(), Path(page.id))
                .await
                .err()
                .expect("viewer publish"),
        );
        assert_denied(
            unpublish_status_page(member(), State(fx.state.clone()), metadata(), Path(page.id))
                .await
                .err()
                .expect("viewer unpublish"),
        );
        assert_denied(
            rotate_status_page_url(member(), State(fx.state.clone()), metadata(), Path(page.id))
                .await
                .err()
                .expect("viewer rotate"),
        );
        assert_denied(
            update_status_page(
                member(),
                State(fx.state.clone()),
                metadata(),
                Path(page.id),
                Json(UpdateStatusPageRequest {
                    monitors: Some(Vec::new()),
                    ..Default::default()
                }),
            )
            .await
            .err()
            .expect("viewer update"),
        );
        let delete_key = RequireAuth(AuthContext::new_api_key(
            user(),
            None,
            Some(vec![Permission::StatusPageDelete]),
            "status-page-delete-key".to_string(),
            1,
        ));
        assert_denied(
            delete_status_page(
                delete_key,
                State(fx.state.clone()),
                metadata(),
                Path(page.id),
            )
            .await
            .err()
            .expect("viewer delete"),
        );
        assert!(fx
            .audit
            .entries
            .lock()
            .expect("audit")
            .iter()
            .all(|(op, _)| op == "STATUS_PAGE_CREATED"));
    }

    #[tokio::test]
    async fn members_of_other_projects_cannot_see_or_probe_pages() {
        let outsider = |_project_id: i32| -> Option<Arc<dyn ProjectAccessChecker>> {
            Some(Arc::new(Checker {
                allowed: Vec::new(),
                permissions: None,
            }))
        };
        let Some(fx) = fixture(outsider).await else {
            return;
        };
        let page = create_page(&fx, api_selection(&fx)).await;
        assert_denied(
            get_status_page(member(), State(fx.state.clone()), Path(page.id))
                .await
                .err()
                .expect("outsider get"),
        );
        assert_denied(
            list_status_pages(
                member(),
                State(fx.state.clone()),
                Path(fx.project_a),
                Query(StatusPageListQuery {
                    page: None,
                    page_size: None,
                }),
            )
            .await
            .err()
            .expect("outsider list"),
        );
        // A caller without the coarse permission gets 403 before the lookup,
        // so a missing id and another project's id are indistinguishable.
        let no_perms = || {
            RequireAuth(AuthContext::new_api_key(
                user(),
                None,
                Some(Vec::new()),
                "no-perms-key".to_string(),
                1,
            ))
        };
        for id in [page.id, i32::MAX] {
            assert_denied(
                publish_status_page(no_perms(), State(fx.state.clone()), metadata(), Path(id))
                    .await
                    .err()
                    .expect("no permission"),
            );
        }
    }

    #[tokio::test]
    async fn pages_of_a_project_being_deleted_stop_being_served() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let page = create_page(&fx, api_selection(&fx)).await;
        publish(&fx, page.id).await;
        let path = format!("/status/{}", page.slug);
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::OK);

        // Project deletion marks the row first and may fail before the hard
        // delete; the page must not survive that.
        let db = fx._db.connection_arc();
        let mut project: projects::ActiveModel = projects::Entity::find_by_id(fx.project_a)
            .one(db.as_ref())
            .await
            .expect("load project")
            .expect("project exists")
            .into();
        project.is_deleted = Set(true);
        project.update(db.as_ref()).await.expect("mark deleted");
        fx.state
            .public_status_page_service
            .refresh_project_pages(fx.project_a)
            .await;
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::NOT_FOUND);

        let error = create_status_page(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path(fx.project_a),
            Json(CreateStatusPageRequest {
                title: "On a deleted project".to_string(),
                description: None,
                monitors: Vec::new(),
            }),
        )
        .await
        .err()
        .expect("deleted project");
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
        let error = create_status_page(
            admin(),
            State(fx.state.clone()),
            metadata(),
            Path(i32::MAX),
            Json(CreateStatusPageRequest {
                title: "Nowhere".to_string(),
                description: None,
                monitors: Vec::new(),
            }),
        )
        .await
        .err()
        .expect("missing project");
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn active_incident_on_a_selected_monitor_survives_newer_unrelated_incidents() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let incidents = fx.state.status_page_service.incident_service();
        // 30 newer incidents with public updates, all on the unselected
        // monitor: more than the per-page limit.
        for i in 0..30 {
            let incident = incidents
                .create_incident(
                    fx.project_a,
                    CreateIncidentRequest {
                        title: format!("noise {i}"),
                        description: None,
                        severity: "minor".to_string(),
                        environment_id: None,
                        monitor_id: Some(fx.unselected_monitor),
                    },
                )
                .await
                .expect("noise incident");
            incidents
                .update_incident_status(
                    incident.id,
                    UpdateIncidentStatusRequest {
                        status: "resolved".to_string(),
                        message: format!("noise update {i}"),
                        is_public: true,
                    },
                )
                .await
                .expect("noise update");
        }
        // 25 more public updates on the selected incident: above the
        // per-incident cap.
        for i in 0..25 {
            incidents
                .update_incident_status(
                    fx.selected_incident,
                    UpdateIncidentStatusRequest {
                        status: "monitoring".to_string(),
                        message: format!("progress {i:02}"),
                        is_public: true,
                    },
                )
                .await
                .expect("progress update");
        }

        let page = create_page(&fx, api_selection(&fx)).await;
        publish(&fx, page.id).await;
        let (_, _, body) = fetch(&fx.public, &format!("/status/{}/summary.json", page.slug)).await;
        let value: serde_json::Value = serde_json::from_str(&body).expect("public json");
        let incidents = value["incidents"].as_array().expect("incidents");
        assert_eq!(
            incidents.len(),
            1,
            "only the selected monitor's incident: {body}"
        );
        assert_eq!(incidents[0]["status"], "monitoring");
        let updates = incidents[0]["updates"].as_array().expect("updates");
        assert_eq!(updates.len(), 20, "updates are capped per incident");
        assert_eq!(updates[0]["message"], "progress 24", "newest first");
        assert!(!body.contains("noise"));
        assert_eq!(
            value["status"], "degraded",
            "the active incident lifts the page status"
        );
    }

    #[tokio::test]
    async fn project_deletion_hook_takes_pages_down_immediately() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let page = create_page(&fx, api_selection(&fx)).await;
        publish(&fx, page.id).await;
        let path = format!("/status/{}", page.slug);
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::OK);

        temps_core::ProjectDeletionHook::project_deletion_started(
            &fx.state.public_status_page_service,
            fx.project_a,
        )
        .await;
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unpublish_and_rotate_use_the_current_published_state() {
        let Some(fx) = fixture(|_| None).await else {
            return;
        };
        let page = create_page(&fx, api_selection(&fx)).await;
        // Rotating a draft does not publish it.
        let rotated: StatusPageResponse = decode(
            rotate_status_page_url(admin(), State(fx.state.clone()), metadata(), Path(page.id))
                .await
                .map_err(|p| format!("{:?}", p.body))
                .expect("rotate draft"),
        )
        .await;
        assert!(!rotated.is_published);
        let path = format!("/status/{}", rotated.slug);
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::NOT_FOUND);

        publish(&fx, page.id).await;
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::OK);
        let unpublished: StatusPageResponse = decode(
            unpublish_status_page(admin(), State(fx.state.clone()), metadata(), Path(page.id))
                .await
                .map_err(|p| format!("{:?}", p.body))
                .expect("unpublish"),
        )
        .await;
        assert!(!unpublished.is_published);
        // A full refresh reads the persisted state and keeps it down.
        fx.state
            .public_status_page_service
            .refresh_project_pages(fx.project_a)
            .await;
        assert_eq!(fetch(&fx.public, &path).await.0, StatusCode::NOT_FOUND);
    }
}
