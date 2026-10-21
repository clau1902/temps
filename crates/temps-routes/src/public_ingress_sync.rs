// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Node-facing half of direct worker ingress (ADR 058 D7).
//!
//! - `POST /internal/nodes/{id}/public-ingress/key`: a worker enrolled before
//!   worker ingress registers the public half of the key certificate bundles
//!   are encrypted to, instead of being rejoined.
//! - `POST /internal/nodes/{id}/public-ingress/report`: the host addresses the
//!   agent can bind, and what it bound, for the console.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use serde::Deserialize;
use temps_core::worker_ingress::ListenerReport;
use temps_entities::{node_public_ingress, nodes};
use tracing::{error, info};

use crate::route_sync::{authenticate_node, RouteSyncAppState};

/// Most host addresses kept from one report; a host with more is unusual and
/// the console only needs the plausible candidates.
const MAX_INTERFACES: usize = 64;

#[derive(Debug, Deserialize)]
pub struct RegisterIngressKeyRequest {
    /// Base64 X25519 public key, as `temps join` sends at enrollment.
    pub public_key: String,
}

fn internal(node_id: i32, what: &str, error: impl std::fmt::Display) -> (StatusCode, String) {
    error!(node_id, %error, "{what} failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Failed to {what} for node {node_id}: {error}"),
    )
}

pub async fn post_ingress_key(
    State(state): State<Arc<RouteSyncAppState>>,
    headers: HeaderMap,
    Path(node_id): Path<i32>,
    Json(body): Json<RegisterIngressKeyRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let node = authenticate_node(&state.db, &headers, node_id).await?;
    let public_key = body.public_key.trim().to_string();
    if let Err(error) = temps_core::ecies::EncryptionSession::new(&public_key) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Ingress key for node {node_id} is not a valid X25519 public key: {error}"),
        ));
    }
    match key_registration(node.edge_public_key.as_deref(), &public_key) {
        KeyRegistration::AlreadyRegistered => return Ok(StatusCode::NO_CONTENT),
        KeyRegistration::Refused => return Err((StatusCode::CONFLICT, replace_refused(node_id))),
        KeyRegistration::Store => {}
    }
    // Only fills an empty key: two racing registrations cannot both win.
    let stored = nodes::Entity::update_many()
        .col_expr(nodes::Column::EdgePublicKey, Expr::value(public_key))
        .filter(nodes::Column::Id.eq(node_id))
        .filter(nodes::Column::EdgePublicKey.is_null())
        .exec(state.db.as_ref())
        .await
        .map_err(|e| internal(node_id, "store the ingress key", e))?;
    if stored.rows_affected == 0 {
        return Err((StatusCode::CONFLICT, replace_refused(node_id)));
    }
    info!(node_id, "worker registered its ingress key");
    // Certificate bundles are encrypted to this key: send a fresh snapshot.
    state
        .db
        .execute_unprepared("SELECT pg_notify('route_table_changes', '')")
        .await
        .map_err(|e| internal(node_id, "announce the ingress key", e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, PartialEq, Eq)]
enum KeyRegistration {
    AlreadyRegistered,
    Store,
    Refused,
}

/// A node token may register the ingress key once. Replacing it would let
/// whoever holds the token alone redirect certificate bundles to a key they
/// control, so a different key is refused: the operator re-enrolls instead.
fn key_registration(existing: Option<&str>, offered: &str) -> KeyRegistration {
    match existing {
        None => KeyRegistration::Store,
        Some(current) if current == offered => KeyRegistration::AlreadyRegistered,
        Some(_) => KeyRegistration::Refused,
    }
}

fn replace_refused(node_id: i32) -> String {
    format!(
        "Node {node_id} already has an ingress key, and a node cannot replace it on its own. \
         If this worker lost its key (its data directory was wiped), re-enroll it: create a join \
         command in Settings → Worker Nodes and run `temps join` on the worker, which registers \
         a new key."
    )
}

pub async fn post_listener_report(
    State(state): State<Arc<RouteSyncAppState>>,
    headers: HeaderMap,
    Path(node_id): Path<i32>,
    Json(mut report): Json<ListenerReport>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    authenticate_node(&state.db, &headers, node_id).await?;
    report.interfaces.truncate(MAX_INTERFACES);
    let clip =
        |value: Option<String>, max: usize| value.map(|v| v.chars().take(max).collect::<String>());
    let listening = serde_json::json!({
        "http": clip(report.http, 64),
        "https": clip(report.https, 64),
        "error": clip(report.error, 1000),
    });
    let now = chrono::Utc::now();
    let row = node_public_ingress::ActiveModel {
        node_id: Set(node_id),
        listen_address: Set(None),
        http_port: Set(80),
        https_port: Set(443),
        public_address: Set(None),
        interfaces: Set(Some(serde_json::json!(report.interfaces))),
        listening: Set(Some(listening)),
        mode: Set(report.mode.map(|mode| mode.as_str().to_string())),
        last_check: Set(None),
        checked_at: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
    };
    node_public_ingress::Entity::insert(row)
        .on_conflict(
            OnConflict::column(node_public_ingress::Column::NodeId)
                .update_columns([
                    node_public_ingress::Column::Interfaces,
                    node_public_ingress::Column::Listening,
                    node_public_ingress::Column::Mode,
                    node_public_ingress::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(state.db.as_ref())
        .await
        .map_err(|e| internal(node_id, "store the ingress listener report", e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_registers_its_ingress_key_once() {
        assert_eq!(key_registration(None, "k1"), KeyRegistration::Store);
        assert_eq!(
            key_registration(Some("k1"), "k1"),
            KeyRegistration::AlreadyRegistered
        );
        assert_eq!(key_registration(Some("k1"), "k2"), KeyRegistration::Refused);
        assert!(replace_refused(7).contains("temps join"));
    }
}
