// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `GET /settings/preview-domain/status` (ADR 068 D2).

use std::sync::Arc;

use axum::{extract::State, response::IntoResponse, routing::get, Json, Router};
use temps_auth::{permission_guard, RequireAuth};
use temps_core::environment_address::{
    AddressKind, PreviewDomainStatus, PrimaryAddress, ReachabilityFix, ReachabilityFixKind,
    ReachabilityWarning,
};
use temps_core::problemdetails::Problem;
use utoipa::OpenApi;

use super::environment_address::{EnvironmentAddressService, PreviewDomainStatusResponse};

pub struct AddressAppState {
    pub service: Arc<EnvironmentAddressService>,
}

#[derive(OpenApi)]
#[openapi(
    paths(get_preview_domain_status),
    components(schemas(
        PreviewDomainStatusResponse,
        PreviewDomainStatus,
        PrimaryAddress,
        AddressKind,
        ReachabilityWarning,
        ReachabilityFix,
        ReachabilityFixKind,
    )),
    tags((name = "Reachability", description = "Making a project reachable (ADR 064)"))
)]
pub struct AddressApiDoc;

pub fn configure_routes() -> Router<Arc<AddressAppState>> {
    Router::new().route(
        "/settings/preview-domain/status",
        get(get_preview_domain_status),
    )
}

/// Whether the preview domain can be reached.
///
/// `local_only` (`localhost`, `localho.st`), `unresolved` (a name under it
/// does not resolve), `no_way_in` (it resolves, but to nothing this install
/// serves publicly) or `usable`. Recomputed when the setting, the tunnels or
/// the nodes' public ingress change, and every 30 minutes.
#[utoipa::path(
    tag = "Reachability",
    get,
    path = "/settings/preview-domain/status",
    operation_id = "get_preview_domain_status",
    responses(
        (status = 200, body = PreviewDomainStatusResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
async fn get_preview_domain_status(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AddressAppState>>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, SettingsRead);
    Ok(Json(state.service.preview_domain_status().await?))
}
