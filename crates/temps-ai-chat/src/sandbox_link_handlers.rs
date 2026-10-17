// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Endpoints behind the assistant's sandbox control: see whether a chat can use
//! a sandbox, link one the user owns or create one, and unlink it again.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};
use temps_auth::permissions::Permission;
use temps_auth::{
    deny_deployment_token, permission_guard, project_access_guard, project_scope_guard, RequireAuth,
};
use temps_core::{
    problemdetails::{self, Problem, ProblemDetails},
    AuditContext, RequestMetadata,
};
use utoipa::{OpenApi, ToSchema};

use crate::audit::{AiSandboxLinkedAudit, AiSandboxUnlinkedAudit};
use crate::handlers::{ensure_user_conversation_access, AppState};
use crate::sandbox_link::{
    context_supports_sandbox, link_from_metadata, project_for_new_sandbox, SandboxLink,
    SandboxLinkError, SandboxLinkService,
};

impl From<SandboxLinkError> for Problem {
    fn from(error: SandboxLinkError) -> Self {
        match error {
            SandboxLinkError::SandboxNotFound { .. } => problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Sandbox Not Found")
                .with_detail(error.to_string()),
            SandboxLinkError::Validation { .. } => problemdetails::new(StatusCode::BAD_REQUEST)
                .with_title("Invalid Sandbox Link")
                .with_detail(error.to_string()),
            SandboxLinkError::NotLinkable { .. } | SandboxLinkError::NotLinked { .. } => {
                problemdetails::new(StatusCode::CONFLICT)
                    .with_title("Sandbox Cannot Be Linked")
                    .with_detail(error.to_string())
            }
            SandboxLinkError::Unavailable { .. } => {
                problemdetails::new(StatusCode::SERVICE_UNAVAILABLE)
                    .with_title("Sandboxes Unavailable")
                    .with_detail(error.to_string())
            }
            SandboxLinkError::Sandbox { .. } | SandboxLinkError::Database { .. } => {
                problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                    .with_title("Internal Server Error")
                    .with_detail(error.to_string())
            }
        }
    }
}

/// What a chat can do with a sandbox, and the one it is linked to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ChatSandboxStatus {
    /// Whether this chat can use a sandbox at all.
    pub configured: bool,
    /// Why it cannot, when `configured` is false.
    pub reason: Option<String>,
    /// Console path where the user can fix it, when there is one.
    pub setup_path: Option<String>,
    /// The sandbox currently linked to the chat.
    pub linked: Option<SandboxLink>,
    /// True once the assistant has asked the user to link a sandbox.
    pub offered: bool,
}

/// Link an existing sandbox (`sandbox_id`) or create a new one (`create`).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LinkSandboxRequest {
    /// Public id of one of the caller's own sandboxes.
    pub sandbox_id: Option<String>,
    /// Create a fresh sandbox for this chat instead.
    #[serde(default)]
    pub create: bool,
}

struct Availability {
    configured: bool,
    reason: Option<String>,
}

fn availability(
    state: &AppState,
    auth: &temps_auth::context::AuthContext,
    context_type: &str,
) -> Availability {
    if !context_supports_sandbox(context_type) {
        return Availability {
            configured: false,
            reason: Some(
                "Sandboxes can be linked to Temps assistant chats and project chats.".to_string(),
            ),
        };
    }
    if state.application_sandboxes.is_none() {
        return Availability {
            configured: false,
            reason: Some("This server has no sandbox backend.".to_string()),
        };
    }
    if !auth.has_permission(&Permission::SandboxesExec) {
        return Availability {
            configured: false,
            reason: Some(format!(
                "Your role cannot run commands in sandboxes ({}).",
                Permission::SandboxesExec
            )),
        };
    }
    Availability {
        configured: true,
        reason: None,
    }
}

fn status_of(
    availability: Availability,
    metadata: Option<&serde_json::Value>,
) -> ChatSandboxStatus {
    ChatSandboxStatus {
        configured: availability.configured,
        reason: availability.reason,
        setup_path: None,
        linked: link_from_metadata(metadata),
        offered: crate::sandbox_link::offered_in_metadata(metadata),
    }
}

fn link_service(state: &AppState, conversation_id: &str) -> Result<SandboxLinkService, Problem> {
    let sandboxes =
        state
            .application_sandboxes
            .clone()
            .ok_or_else(|| SandboxLinkError::Unavailable {
                conversation_id: conversation_id.to_string(),
            })?;
    Ok(SandboxLinkService::new(state.db.clone(), sandboxes))
}

/// Whether the chat can use a sandbox, and which one it is linked to.
#[utoipa::path(
    get, tag = "AI Chat", path = "/ai/conversations/{public_id}/sandbox",
    params(("public_id" = String, Path, description = "Conversation public id")),
    responses(
        (status = 200, description = "Sandbox status of the conversation", body = ChatSandboxStatus),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 404, description = "Conversation not found", body = ProblemDetails),
        (status = 500, description = "Internal server error", body = ProblemDetails)
    ),
    security(("bearer_auth" = []))
)]
pub async fn get_chat_sandbox(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Path(public_id): Path<String>,
) -> Result<Json<ChatSandboxStatus>, Problem> {
    permission_guard!(auth, ProjectsRead);
    deny_deployment_token!(auth);
    let conversation = state
        .service
        .get_owned_by_public_id(auth.user_id(), &public_id)
        .await?;
    ensure_user_conversation_access(&state, &auth, &conversation).await?;
    Ok(Json(status_of(
        availability(&state, &auth, &conversation.context_type),
        conversation.metadata.as_ref(),
    )))
}

/// Link a sandbox to the chat, or create one for it. Replaces any earlier link.
#[utoipa::path(
    post, tag = "AI Chat", path = "/ai/conversations/{public_id}/sandbox",
    params(("public_id" = String, Path, description = "Conversation public id")),
    request_body = LinkSandboxRequest,
    responses(
        (status = 200, description = "Sandbox linked", body = ChatSandboxStatus),
        (status = 400, description = "Neither or both of sandbox_id and create were sent", body = ProblemDetails),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 404, description = "Conversation or sandbox not found", body = ProblemDetails),
        (status = 409, description = "The sandbox cannot be linked", body = ProblemDetails),
        (status = 500, description = "Internal server error", body = ProblemDetails),
        (status = 503, description = "This server has no sandbox backend", body = ProblemDetails)
    ),
    security(("bearer_auth" = []))
)]
pub async fn link_chat_sandbox(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(public_id): Path<String>,
    Json(request): Json<LinkSandboxRequest>,
) -> Result<Json<ChatSandboxStatus>, Problem> {
    permission_guard!(auth, ProjectsRead);
    permission_guard!(auth, SandboxesExec);
    deny_deployment_token!(auth);
    let conversation = state
        .service
        .get_owned_by_public_id(auth.user_id(), &public_id)
        .await?;
    ensure_user_conversation_access(&state, &auth, &conversation).await?;
    if !context_supports_sandbox(&conversation.context_type) {
        return Err(SandboxLinkError::Validation {
            conversation_id: conversation.public_id.clone(),
            message: format!(
                "sandboxes can be linked to assistant and project chats only, not '{}' chats",
                conversation.context_type
            ),
        }
        .into());
    }
    let service = link_service(&state, &conversation.public_id)?;
    let sandbox_id = request
        .sandbox_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let link = match (sandbox_id, request.create) {
        (Some(sandbox_id), false) => {
            service
                .link_existing(&conversation, auth.user_id(), sandbox_id)
                .await?
        }
        (None, true) => {
            permission_guard!(auth, SandboxesWrite);
            // A sandbox made for a project chat starts as a clone of the
            // project's repository, which reads its stored Git credential: the
            // same gates as creating a sandbox from a project directly.
            if let Some(project_id) = project_for_new_sandbox(&conversation) {
                permission_guard!(auth, GitRepositoriesRead);
                project_scope_guard!(auth, project_id);
                project_access_guard!(auth, project_id, state.project_access_checker);
            }
            service
                .create_and_link(&conversation, auth.user_id(), None)
                .await?
        }
        _ => {
            return Err(SandboxLinkError::Validation {
                conversation_id: conversation.public_id.clone(),
                message: "send either `sandbox_id` to link one of your sandboxes or `create: true` to make a new one".to_string(),
            }
            .into())
        }
    };
    state
        .audit(&AiSandboxLinkedAudit {
            context: AuditContext {
                user_id: auth.user_id(),
                ip_address: Some(metadata.ip_address.clone()),
                user_agent: metadata.user_agent.clone(),
            },
            project_id: conversation.project_id,
            conversation_id: conversation.public_id.clone(),
            sandbox_id: link.sandbox_id.clone(),
            created: request.create,
        })
        .await;
    Ok(Json(ChatSandboxStatus {
        configured: true,
        reason: None,
        setup_path: None,
        linked: Some(link),
        offered: true,
    }))
}

/// Unlink the chat's sandbox. The sandbox itself is left running.
#[utoipa::path(
    delete, tag = "AI Chat", path = "/ai/conversations/{public_id}/sandbox",
    params(("public_id" = String, Path, description = "Conversation public id")),
    responses(
        (status = 200, description = "Sandbox unlinked", body = ChatSandboxStatus),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 404, description = "Conversation not found", body = ProblemDetails),
        (status = 500, description = "Internal server error", body = ProblemDetails),
        (status = 503, description = "This server has no sandbox backend", body = ProblemDetails)
    ),
    security(("bearer_auth" = []))
)]
pub async fn unlink_chat_sandbox(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(public_id): Path<String>,
) -> Result<Json<ChatSandboxStatus>, Problem> {
    permission_guard!(auth, ProjectsRead);
    deny_deployment_token!(auth);
    let conversation = state
        .service
        .get_owned_by_public_id(auth.user_id(), &public_id)
        .await?;
    ensure_user_conversation_access(&state, &auth, &conversation).await?;
    let previous = link_from_metadata(conversation.metadata.as_ref());
    let service = link_service(&state, &conversation.public_id)?;
    service.unlink(&conversation).await?;
    state
        .audit(&AiSandboxUnlinkedAudit {
            context: AuditContext {
                user_id: auth.user_id(),
                ip_address: Some(metadata.ip_address.clone()),
                user_agent: metadata.user_agent.clone(),
            },
            project_id: conversation.project_id,
            conversation_id: conversation.public_id.clone(),
            sandbox_id: previous.map(|link| link.sandbox_id),
        })
        .await;
    Ok(Json(ChatSandboxStatus {
        configured: true,
        reason: None,
        setup_path: None,
        linked: None,
        offered: true,
    }))
}

pub fn configure_routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/ai/conversations/{public_id}/sandbox",
        get(get_chat_sandbox)
            .post(link_chat_sandbox)
            .delete(unlink_chat_sandbox),
    )
}

#[derive(OpenApi)]
#[openapi(
    paths(get_chat_sandbox, link_chat_sandbox, unlink_chat_sandbox),
    components(schemas(ChatSandboxStatus, LinkSandboxRequest, SandboxLink))
)]
pub struct SandboxLinkApiDoc;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn link_errors_map_to_the_status_a_client_can_act_on() {
        let status = |error: SandboxLinkError| Problem::from(error).status_code;
        assert_eq!(
            status(SandboxLinkError::SandboxNotFound {
                sandbox_id: "sbx_1".into(),
                user_id: 3
            }),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(SandboxLinkError::Validation {
                conversation_id: "conv_1".into(),
                message: "bad".into()
            }),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(SandboxLinkError::NotLinkable {
                sandbox_id: "sbx_1".into(),
                conversation_id: "conv_1".into(),
                reason: "it has been destroyed".into()
            }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(SandboxLinkError::Unavailable {
                conversation_id: "conv_1".into()
            }),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn status_carries_the_link_and_the_offer_flag_from_metadata() {
        let metadata = json!({
            "sandbox_link": {
                "sandbox_id": "sbx_abc",
                "name": "scratch",
                "linked_at": "2026-10-17T09:30:00Z"
            },
            "sandbox_offered_at": "2026-10-17T09:00:00Z"
        });
        let status = status_of(
            Availability {
                configured: true,
                reason: None,
            },
            Some(&metadata),
        );
        assert_eq!(status.linked.map(|l| l.sandbox_id), Some("sbx_abc".into()));
        assert!(status.offered);

        let empty = status_of(
            Availability {
                configured: false,
                reason: Some("This server has no sandbox backend.".into()),
            },
            None,
        );
        assert!(empty.linked.is_none() && !empty.offered && !empty.configured);
        assert!(empty.reason.is_some());
    }
}
