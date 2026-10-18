// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Audit events for AI chat write operations.
//!
//! Per the project audit convention, every mutating handler records an audit
//! entry. These cover creating a conversation, sending a message (which runs an
//! AI turn and incurs token cost), and archiving a conversation — so there's a
//! trail of who started/drove/closed an AI chat. Emission is best-effort: a
//! failed audit log never fails the underlying request.

use anyhow::Result;
use serde::Serialize;

use temps_core::{AuditContext, AuditOperation};

#[derive(Debug, Clone, Serialize)]
pub struct ConversationCreatedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    /// Public id of the created conversation.
    pub conversation_id: String,
    pub context_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessageSentAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationArchivedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationRestoredAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationRenamedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    /// New human-facing title after the rename.
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationPermissionModeChangedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    pub permission_mode: String,
    pub applied_to_active_turn: bool,
}

/// A watched goal failed and the assistant started a fix turn on the owner's
/// behalf. Recorded because it is the one case where the assistant acts while
/// the user is not looking.
#[derive(Debug, Clone, Serialize)]
pub struct AiGoalHealStartedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    pub goal_id: String,
    pub attempt: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiGoalCancelledAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    pub goal_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiGoalRetriedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    pub goal_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiSandboxLinkedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    pub sandbox_id: String,
    /// True when the sandbox was created for the chat rather than picked.
    pub created: bool,
}

/// A chat's sleeping sandbox was started again, by the console or the assistant.
#[derive(Debug, Clone, Serialize)]
pub struct AiSandboxWokenAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    pub sandbox_id: String,
}

/// A chat's older messages were folded into its running summary. Records how
/// many, never what they said.
#[derive(Debug, Clone, Serialize)]
pub struct AiConversationCompactedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    pub folded_messages: usize,
    pub kept_messages: usize,
    pub summary_model: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiSandboxUnlinkedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    pub sandbox_id: Option<String>,
}

/// A chat's sandbox was connected to a project's services (ADR-051). Names the
/// variables the sandbox's commands run with, never their values.
#[derive(Debug, Clone, Serialize)]
pub struct AiSandboxServicesConnectedAudit {
    pub context: AuditContext,
    /// The project the services were connected through, if any.
    pub project_id: Option<i32>,
    pub conversation_id: String,
    pub sandbox_id: String,
    pub connected_service_ids: Vec<i32>,
    pub variable_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiSandboxServicesDisconnectedAudit {
    pub context: AuditContext,
    /// The project whose services were disconnected, when the whole project was.
    pub project_id: Option<i32>,
    /// The single service disconnected, when it was disconnected on its own.
    pub service_id: Option<i32>,
    pub conversation_id: String,
    pub sandbox_id: String,
}

/// The files of a chat's sandbox were deployed as a project's source, with no
/// Git repository. Records who approved it and how much was uploaded, never
/// file names or contents.
#[derive(Debug, Clone, Serialize)]
pub struct AiSandboxSourceDeployedAudit {
    pub context: AuditContext,
    pub project_id: i32,
    /// The project the chat belongs to, when it is a project chat.
    pub chat_project_id: Option<i32>,
    pub conversation_id: String,
    pub sandbox_id: String,
    pub environment_id: i32,
    pub deployment_id: i32,
    pub files: usize,
    pub source_bytes: u64,
}

macro_rules! impl_audit_operation {
    ($type:ty, $op:expr) => {
        impl AuditOperation for $type {
            fn operation_type(&self) -> String {
                $op.to_string()
            }
            fn user_id(&self) -> Option<i32> {
                Some(self.context.user_id)
            }
            fn ip_address(&self) -> Option<String> {
                self.context.ip_address.clone()
            }
            fn user_agent(&self) -> &str {
                &self.context.user_agent
            }
            fn serialize(&self) -> Result<String> {
                serde_json::to_string(self)
                    .map_err(|e| anyhow::anyhow!("Failed to serialize audit operation: {e}"))
            }
        }
    };
}

impl_audit_operation!(ConversationCreatedAudit, "AI_CHAT_CONVERSATION_CREATED");
impl_audit_operation!(ChatMessageSentAudit, "AI_CHAT_MESSAGE_SENT");
impl_audit_operation!(ConversationArchivedAudit, "AI_CHAT_CONVERSATION_ARCHIVED");
impl_audit_operation!(ConversationRestoredAudit, "AI_CHAT_CONVERSATION_RESTORED");
impl_audit_operation!(ConversationRenamedAudit, "AI_CHAT_CONVERSATION_RENAMED");
impl_audit_operation!(
    ConversationPermissionModeChangedAudit,
    "AI_CHAT_PERMISSION_MODE_CHANGED"
);

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationCreatedAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub project_ids: Vec<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationArchivedAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub project_ids: Vec<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationRestoredAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub project_ids: Vec<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadArtifactCreatedAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub conversation_id: String,
    pub artifact_id: String,
    pub kind: String,
}

impl_audit_operation!(ApplicationCreatedAudit, "ai.application.created");
impl_audit_operation!(ApplicationArchivedAudit, "ai.application.archived");
impl_audit_operation!(ApplicationRestoredAudit, "ai.application.restored");
impl_audit_operation!(ThreadArtifactCreatedAudit, "ai.thread_artifact.created");

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationTopologyChangedAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub action: String,
    pub project_id: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationWorkspaceChangedAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub action: String,
    pub sandbox_id: Option<String>,
    pub runtime: Option<String>,
    pub cpu_limit: Option<f64>,
    pub memory_limit_mb: Option<i64>,
    pub pids_limit: Option<i64>,
    pub disk_limit_mb: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationWorkspaceDeployedAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub project_id: i32,
    pub environment_id: i32,
    pub deployment_id: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationPreviewLinkCreatedAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub sandbox_id: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct GlobalWorkspacePreviewLinkCreatedAudit {
    pub context: AuditContext,
    pub workspace_id: String,
    pub sandbox_id: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationWorkspaceSourceImportedAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub project_id: i32,
    pub used_git_connection: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplicationWorkspaceFilesWrittenAudit {
    pub context: AuditContext,
    pub application_id: String,
    pub project_id: Option<i32>,
    pub file_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct GlobalWorkspaceFilesWrittenAudit {
    pub context: AuditContext,
    pub workspace_id: String,
    pub file_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationAttachmentUploadedAudit {
    pub context: AuditContext,
    pub conversation_id: String,
    pub attachment_id: String,
    pub size_bytes: u64,
}

impl_audit_operation!(
    ApplicationTopologyChangedAudit,
    "ai.application.topology_changed"
);
impl_audit_operation!(
    ApplicationWorkspaceChangedAudit,
    "ai.application.workspace_changed"
);
impl_audit_operation!(
    ApplicationWorkspaceDeployedAudit,
    "ai.application.workspace_deployed"
);
impl_audit_operation!(
    ApplicationPreviewLinkCreatedAudit,
    "ai.application.preview_link.created"
);
impl_audit_operation!(
    GlobalWorkspacePreviewLinkCreatedAudit,
    "ai.workspace.preview_link.created"
);
impl_audit_operation!(
    ApplicationWorkspaceSourceImportedAudit,
    "ai.application.workspace_source.imported"
);
impl_audit_operation!(
    ApplicationWorkspaceFilesWrittenAudit,
    "ai.application.workspace_files.written"
);
impl_audit_operation!(
    GlobalWorkspaceFilesWrittenAudit,
    "ai.workspace.files.written"
);
impl_audit_operation!(
    ConversationAttachmentUploadedAudit,
    "ai.conversation.attachment.uploaded"
);

// --- Pending-action audit events -------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct AiActionConfirmedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    /// Public id of the confirmed action.
    pub action_id: String,
    pub operation_id: String,
    /// Final status (`"executed"` or `"failed"`).
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiActionRejectedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub action_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiActionTransitionFailedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub action_id: String,
    pub operation_id: String,
    pub attempted_transition: String,
    /// Sanitized diagnostic only; raw upstream bodies and credentials are
    /// never written to the audit stream.
    pub error: String,
}

impl_audit_operation!(AiGoalHealStartedAudit, "ai.goal.heal_started");
impl_audit_operation!(AiGoalCancelledAudit, "ai.goal.cancelled");
impl_audit_operation!(AiGoalRetriedAudit, "ai.goal.retried");
impl_audit_operation!(AiConversationCompactedAudit, "ai.conversation.compacted");
impl_audit_operation!(AiSandboxLinkedAudit, "ai.sandbox.linked");
impl_audit_operation!(AiSandboxUnlinkedAudit, "ai.sandbox.unlinked");
impl_audit_operation!(AiSandboxWokenAudit, "ai.sandbox.woken");
impl_audit_operation!(
    AiSandboxServicesConnectedAudit,
    "ai.sandbox.services_connected"
);
impl_audit_operation!(
    AiSandboxServicesDisconnectedAudit,
    "ai.sandbox.services_disconnected"
);
impl_audit_operation!(AiSandboxSourceDeployedAudit, "ai.sandbox.source_deployed");
impl_audit_operation!(AiActionConfirmedAudit, "ai.pending_action.confirmed");
impl_audit_operation!(AiActionRejectedAudit, "ai.pending_action.rejected");
impl_audit_operation!(
    AiActionTransitionFailedAudit,
    "ai.pending_action.transition_failed"
);

// --- Interactive permission-bridge audit events ----------------------------

/// Audit entry for resolving an interactive permission request (ADR-038 Phase 2).
///
/// Only the permission kind and id are recorded — never the raw tool input or
/// answer content, which may carry sensitive data (credentials, PII, code).
#[derive(Debug, Clone, Serialize)]
pub struct PermissionResolvedAudit {
    pub context: AuditContext,
    pub project_id: Option<i32>,
    pub conversation_id: String,
    /// The CLI's `request_id` (same value used as the registry key and in the
    /// resolve endpoint URL as `{permission_id}`).
    pub permission_id: String,
    /// Sanitized provider tool name. Inputs are never logged.
    pub tool_name: Option<String>,
    /// Decision kind: `"allow_tool"`, `"deny_tool"`, `"answer_question"`,
    /// `"approve_plan"`, or `"reject_plan"`.
    pub decision_kind: String,
}

impl_audit_operation!(PermissionResolvedAudit, "ai.permission.resolved");
