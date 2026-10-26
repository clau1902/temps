// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Node-facing half of the Temps gate (ADR 065 D8).
//!
//! - `POST /internal/nodes/{id}/gate/redeem`: an agent exchanges a sign-in
//!   code for a gate token. Only for an address in that agent's own route
//!   snapshot, so a code seen in a log or a `Referer` is useless without the
//!   state cookie *and* the token of a node that serves the address.
//! - `POST /internal/nodes/{id}/gate/logout`: an agent revokes the session
//!   behind a token a visitor logged out with.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use temps_core::gate::{GateLogoutRequest, GateRedeemRequest};
use temps_entities::nodes;
use tracing::{error, warn};

use crate::gate::GateServiceError;
use crate::route_sync::{authenticate_node, reports_capability, RouteSyncAppState};

fn refusal(error: GateServiceError) -> (StatusCode, String) {
    let status =
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if status.is_server_error() {
        error!(%error, "gate request from a node failed");
    }
    (status, error.to_string())
}

/// Whether `node` is given the signed-in address `address_id` in its
/// snapshot: it enforces the gate, takes public traffic (directly or
/// through a connector), and the address is one of the worker routes.
async fn node_serves_address(
    state: &RouteSyncAppState,
    node: &nodes::Model,
    address_id: &str,
) -> Result<bool, (StatusCode, String)> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let internal = |operation: &str, error: sea_orm::DbErr| {
        error!(node_id = node.id, %error, "{operation} failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to {operation} for node {}: {error}", node.id),
        )
    };
    let ingress = temps_entities::node_public_ingress::Entity::find_by_id(node.id)
        .one(state.db.as_ref())
        .await
        .map_err(|e| internal("load the node's capabilities", e))?;
    if !reports_capability(ingress.as_ref(), temps_core::gate::GATE_CAPABILITY) {
        return Ok(false);
    }
    let runs_connector = temps_entities::ingress_connector_nodes::Entity::find()
        .filter(temps_entities::ingress_connector_nodes::Column::NodeId.eq(node.id))
        .one(state.db.as_ref())
        .await
        .map_err(|e| internal("check ingress connector assignment", e))?
        .is_some();
    if !node.public_ingress_enabled && !runs_connector {
        return Ok(false);
    }
    let routes = crate::route_sync::worker_public_routes(
        state.db.as_ref(),
        &state.peer_table,
        !state.request_policy_gate.supports_worker_ingress(),
    )
    .await
    .map_err(|(operation, source)| internal(operation, source))?;
    Ok(routes.routes.iter().any(|route| {
        route
            .gate
            .as_ref()
            .is_some_and(|gate| gate.address_id == address_id)
    }))
}

pub async fn post_gate_redeem(
    State(state): State<Arc<RouteSyncAppState>>,
    headers: HeaderMap,
    Path(node_id): Path<i32>,
    Json(request): Json<GateRedeemRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let node = authenticate_node(&state.db, &headers, node_id).await?;
    if !node_serves_address(&state, &node, &request.address_id).await? {
        warn!(
            node_id,
            address_id = %request.address_id,
            "node tried to redeem a sign-in code for an address it does not serve"
        );
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "Node {node_id} does not serve address {}, so it cannot redeem its sign-in codes",
                request.address_id
            ),
        ));
    }
    let redeemed = state.gate.redeem(&request).await.map_err(refusal)?;
    Ok(Json(redeemed))
}

pub async fn post_gate_logout(
    State(state): State<Arc<RouteSyncAppState>>,
    headers: HeaderMap,
    Path(node_id): Path<i32>,
    Json(request): Json<GateLogoutRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let node = authenticate_node(&state.db, &headers, node_id).await?;
    if !node_serves_address(&state, &node, &request.address_id).await? {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "Node {node_id} does not serve address {}, so it cannot sign anyone out of it",
                request.address_id
            ),
        ));
    }
    state.gate.logout(&request).await.map_err(refusal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_map_to_client_and_server_errors() {
        let address_id = "custom:1".to_string();
        assert_eq!(
            refusal(GateServiceError::InvalidCode {
                address_id: address_id.clone()
            })
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            refusal(GateServiceError::NotGated {
                address_id: address_id.clone(),
                hostname: "a.example.test".into()
            })
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            refusal(GateServiceError::Key {
                operation: "unsealed",
                reason: "x".into()
            })
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
