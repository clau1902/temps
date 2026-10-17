// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The endpoint behind a chat's "made in this chat" canvas.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use temps_auth::{deny_deployment_token, permission_guard, RequireAuth};
use temps_core::problemdetails::{self, Problem, ProblemDetails};
use utoipa::OpenApi;

use crate::chat_resources::{
    ChatResource, ChatResourceError, ChatResourceKind, ChatResourceService, ChatResourcesResponse,
};
use crate::handlers::{ensure_user_conversation_access, AppState};

impl From<ChatResourceError> for Problem {
    fn from(error: ChatResourceError) -> Self {
        match error {
            ChatResourceError::Database { .. } => {
                problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                    .with_title("Internal Server Error")
                    .with_detail(error.to_string())
            }
        }
    }
}

/// The projects, databases and services, repositories and sandbox this chat
/// made, with their current state.
#[utoipa::path(
    get, tag = "AI Chat", path = "/ai/conversations/{public_id}/resources",
    params(("public_id" = String, Path, description = "Conversation public id")),
    responses(
        (status = 200, description = "Pieces the conversation made", body = ChatResourcesResponse),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 404, description = "Conversation not found", body = ProblemDetails),
        (status = 500, description = "Internal server error", body = ProblemDetails)
    ),
    security(("bearer_auth" = []))
)]
pub async fn list_chat_resources(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Path(public_id): Path<String>,
) -> Result<Json<ChatResourcesResponse>, Problem> {
    permission_guard!(auth, ProjectsRead);
    deny_deployment_token!(auth);
    let conversation = state
        .service
        .get_owned_by_public_id(auth.user_id(), &public_id)
        .await?;
    ensure_user_conversation_access(&state, &auth, &conversation).await?;
    let resources = ChatResourceService::new(state.db.clone())
        .list(&conversation, auth.user_id())
        .await?;
    Ok(Json(resources))
}

pub fn configure_routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/ai/conversations/{public_id}/resources",
        get(list_chat_resources),
    )
}

#[derive(OpenApi)]
#[openapi(
    paths(list_chat_resources),
    components(schemas(ChatResource, ChatResourceKind, ChatResourcesResponse))
)]
pub struct ChatResourcesApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_lookup_is_an_internal_error_naming_the_chat() {
        let problem = Problem::from(ChatResourceError::Database {
            conversation_id: "conv_1".into(),
            reason: "connection closed".into(),
        });
        assert_eq!(problem.status_code, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(problem
            .body
            .get("detail")
            .and_then(|detail| detail.as_str())
            .is_some_and(|detail| detail.contains("conv_1")));
    }
}
