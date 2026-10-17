// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A sandbox linked to one assistant chat, and the tools that run inside it.
//!
//! Some steps need a shell: cloning and committing a repository, unpacking an
//! archive, starting a server to see whether it comes up. The assistant cannot
//! reach the generic sandbox API for that, because those operations accept any
//! sandbox id, including a user's persistent workspace. Instead the user links
//! exactly one sandbox to the chat, and the tools in this module act on that
//! sandbox only.
//!
//! The link lives in `ai_conversations.metadata` under [`SANDBOX_LINK_KEY`].
//! It is re-read and re-checked on every tool call, so unlinking, a destroyed
//! sandbox or a revoked permission takes effect on the next call, and the
//! model can never name a sandbox of its own choosing. Nothing here holds
//! platform or Git credentials; the sandbox is an ordinary user sandbox.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, EntityTrait,
    QueryFilter, Statement,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use temps_ai::ChatTool;
use temps_auth::context::AuthContext;
use temps_auth::permissions::Permission;
use temps_core::{AuditContext, AuditLogger};
use temps_entities::ai_conversations;
use temps_sandbox::error::SandboxError;
use temps_sandbox::services::{CreateSandboxRequest, ExecOptions};
use temps_sandbox::SandboxService;
use thiserror::Error;
use tracing::{info, warn};
use utoipa::ToSchema;

use crate::providers::assistant::ASSISTANT_CONTEXT_TYPE;

/// Key under `ai_conversations.metadata` that holds the link.
pub const SANDBOX_LINK_KEY: &str = "sandbox_link";
/// Key holding the last port the assistant confirmed serving a page in the
/// linked sandbox. The chat's Preview tab shows it.
pub const SANDBOX_PREVIEW_KEY: &str = "sandbox_preview_port";
/// Key recording that the user has already been offered a sandbox in this chat.
pub const SANDBOX_OFFERED_KEY: &str = "sandbox_offered_at";

pub const SANDBOX_EXEC_TOOL: &str = "sandbox_exec";
pub const SANDBOX_READ_TOOL: &str = "sandbox_read_file";
pub const SANDBOX_WRITE_TOOL: &str = "sandbox_write_file";
pub const SANDBOX_PREVIEW_TOOL: &str = "sandbox_preview";
pub const SANDBOX_OFFER_TOOL: &str = "temps_offer_sandbox";
pub const SANDBOX_CREATE_TOOL: &str = "temps_create_sandbox";

/// Most bytes of command output or file content handed back to the model.
const MAX_TOOL_OUTPUT_BYTES: usize = 128 * 1024;
/// Largest file the model may write in one call.
const MAX_WRITE_BYTES: usize = 1024 * 1024;
const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 120;
const MAX_EXEC_TIMEOUT_SECS: u64 = 600;
/// Idle lifetime of a sandbox created from a chat.
const CREATED_SANDBOX_TIMEOUT_SECS: u64 = 3600;
/// How long a request waits for a new sandbox to come up before it reports
/// that it is still starting. The sandbox keeps starting and shows up in the
/// user's sandbox list; only this request stops waiting.
const CREATE_WAIT_SECS: u64 = 90;

/// How long a tool call waits for a sleeping sandbox to wake before it tells
/// the model and the user it did not.
const RESUME_WAIT_SECS: u64 = 120;
/// Prefix of the persistent per-user application workspace sandboxes.
const APPLICATION_WORKSPACE_PREFIX: &str = "ai-application:";
/// Context type of chats scoped to one project.
pub const PROJECT_CONTEXT_TYPE: &str = "project";

/// Whether a chat of this kind can have a sandbox linked to it: the Temps
/// assistant, and chats about one project (whose sandbox is seeded from the
/// project's repository).
pub fn context_supports_sandbox(context_type: &str) -> bool {
    context_type == ASSISTANT_CONTEXT_TYPE || context_type == PROJECT_CONTEXT_TYPE
}

/// The project a new sandbox for this chat is created from: only project chats
/// carry one, and the sandbox then starts as a clone of its repository.
pub fn project_for_new_sandbox(conversation: &ai_conversations::Model) -> Option<i32> {
    if conversation.context_type == PROJECT_CONTEXT_TYPE {
        conversation.project_id
    } else {
        None
    }
}

#[derive(Error, Debug)]
pub enum SandboxLinkError {
    #[error(
        "Conversation {conversation_id} cannot use a sandbox: this server has no sandbox backend"
    )]
    Unavailable { conversation_id: String },

    #[error("Sandbox {sandbox_id} was not found among the sandboxes of user {user_id}")]
    SandboxNotFound { sandbox_id: String, user_id: i32 },

    #[error("Sandbox {sandbox_id} cannot be linked to conversation {conversation_id}: {reason}")]
    NotLinkable {
        sandbox_id: String,
        conversation_id: String,
        reason: String,
    },

    #[error("Conversation {conversation_id} has no linked sandbox")]
    NotLinked { conversation_id: String },

    #[error("Invalid sandbox link request for conversation {conversation_id}: {message}")]
    Validation {
        conversation_id: String,
        message: String,
    },

    #[error("Sandbox operation failed for conversation {conversation_id}: {reason}")]
    Sandbox {
        conversation_id: String,
        reason: String,
    },

    #[error("Database error while updating the sandbox link of conversation {conversation_id}: {source}")]
    Database {
        conversation_id: String,
        #[source]
        source: DbErr,
    },
}

/// The sandbox a chat is linked to, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SandboxLink {
    /// Public id of the sandbox (`sbx_…`).
    pub sandbox_id: String,
    /// Display name of the sandbox when it was linked.
    pub name: String,
    /// ISO 8601 time the link was made.
    pub linked_at: DateTime<Utc>,
}

/// Read the link out of a conversation's metadata, if there is a well-formed one.
pub fn link_from_metadata(metadata: Option<&Value>) -> Option<SandboxLink> {
    let link = metadata?.get(SANDBOX_LINK_KEY)?;
    serde_json::from_value(link.clone()).ok()
}

/// The port the assistant last confirmed serving a page, if one is recorded.
pub fn preview_port_from_metadata(metadata: Option<&Value>) -> Option<u16> {
    let port = metadata?.get(SANDBOX_PREVIEW_KEY)?.as_u64()?;
    u16::try_from(port).ok().filter(|port| *port >= 1)
}

pub(crate) fn offered_in_metadata(metadata: Option<&Value>) -> bool {
    metadata
        .and_then(|value| value.get(SANDBOX_OFFERED_KEY))
        .is_some_and(|value| !value.is_null())
}

/// Why a sandbox may not be linked, if it may not.
///
/// Application workspaces are the persistent per-user environments managed by
/// the application API and agent-run sandboxes belong to their run; neither is
/// something a chat should shell into.
pub fn linkability_problem(
    name: &str,
    agent_run_id: Option<i32>,
    status: &str,
) -> Option<&'static str> {
    if name.starts_with(APPLICATION_WORKSPACE_PREFIX) {
        return Some("it is an application workspace, which is managed from its application");
    }
    if agent_run_id.is_some() {
        return Some("it belongs to an agent run, which manages its lifecycle");
    }
    if status == "destroyed" {
        return Some("it has been destroyed");
    }
    None
}

/// Whether a linked sandbox can take a command right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxReadiness {
    /// Running: run the tool as is.
    Ready,
    /// Stopped ("sleeping"): wake it first, and say so.
    Asleep,
}

/// Classify a sandbox row's status. Only an explicitly stopped sandbox needs
/// waking; `destroyed` is rejected earlier by [`linkability_problem`].
pub fn readiness(status: &str) -> SandboxReadiness {
    if status == "stopped" {
        SandboxReadiness::Asleep
    } else {
        SandboxReadiness::Ready
    }
}

/// Put a notice for the user and the model in front of a tool result.
fn with_notice(notice: Option<String>, result: String) -> String {
    match notice {
        Some(notice) => format!("{notice}\n\n{result}"),
        None => result,
    }
}

/// Services that publish a sandbox's port through someone else's network.
/// Temps serves previews itself, so a command that fetches or starts one is
/// refused. This is a guard on the common tools, not a security boundary.
const EXTERNAL_EXPOSURE_TOOLS: &[&str] = &[
    "cloudflared",
    "trycloudflare",
    "ngrok",
    "localtunnel",
    "loca.lt",
    "untun",
    "serveo",
    "localhost.run",
    "pinggy",
    "tunnelto",
    "bore.pub",
];

fn is_word_boundary(c: Option<char>) -> bool {
    c.map(|c| !c.is_ascii_alphanumeric()).unwrap_or(true)
}

/// The third-party tunnel service a shell command uses, if any.
pub fn third_party_exposure(command: &str) -> Option<&'static str> {
    let lowered = command.to_ascii_lowercase();
    for name in EXTERNAL_EXPOSURE_TOOLS {
        let mut from = 0;
        while let Some(found) = lowered[from..].find(name) {
            let start = from + found;
            let end = start + name.len();
            let before = lowered[..start].chars().next_back();
            let after = lowered[end..].chars().next();
            if is_word_boundary(before) && is_word_boundary(after) {
                return Some(name);
            }
            from = end;
        }
    }
    if lowered.contains("tailscale funnel") {
        return Some("tailscale funnel");
    }
    None
}

/// Where a port is listening inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenScope {
    /// Nothing listens on the port.
    Nothing,
    /// Only on the loopback address: the preview cannot reach it.
    LoopbackOnly,
    /// On a non-loopback address: the preview can reach it.
    Reachable,
}

/// Read a port's listener out of the text of `/proc/net/tcp` and
/// `/proc/net/tcp6` (concatenated).
pub fn listen_scope(proc_net_tcp: &str, port: u16) -> ListenScope {
    let wanted = format!("{port:04X}");
    let mut scope = ListenScope::Nothing;
    for line in proc_net_tcp.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        // Column 3 is the socket state; 0A is LISTEN.
        if columns.len() < 4 || columns[3] != "0A" {
            continue;
        }
        let Some((address, listening_port)) = columns[1].rsplit_once(':') else {
            continue;
        };
        if !listening_port.eq_ignore_ascii_case(&wanted) {
            continue;
        }
        // /proc prints each 32-bit word little-endian: 127.0.0.1 is
        // `0100007F`, ::1 is `00000000000000000000000001000000`, and a
        // v4-mapped loopback ends the same way as the v4 one.
        let loopback = address.eq_ignore_ascii_case("00000000000000000000000001000000")
            || address.to_ascii_uppercase().ends_with("0100007F")
            || (address.len() == 8 && address.to_ascii_uppercase().ends_with("7F"));
        if loopback {
            if scope == ListenScope::Nothing {
                scope = ListenScope::LoopbackOnly;
            }
        } else {
            scope = ListenScope::Reachable;
        }
    }
    scope
}

/// Keep the end of `text` (errors and results come last), at most `max` bytes.
fn cap_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "[earlier output truncated: showing the last {} of {} bytes]\n{}",
        text.len() - start,
        text.len(),
        &text[start..]
    )
}

/// Keep the start of `text` (a file's head), at most `max` bytes.
fn cap_head(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[truncated: showing the first {} of {} bytes]",
        &text[..end],
        end,
        text.len()
    )
}

/// Resolve a model-supplied path against the sandbox work dir.
fn resolve_path(work_dir: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", work_dir.trim_end_matches('/'), path)
    }
}

#[derive(Debug, Deserialize)]
struct ExecArgs {
    command: String,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    executable: bool,
}

#[derive(Debug, Deserialize)]
struct PreviewArgs {
    port: Option<u16>,
}

/// Separates the listener table from the HTTP status in the probe's output.
const PROBE_SEPARATOR: &str = "---temps-probe---";

/// The tool result for a preview check: the address, whether it will work,
/// and what to do when it will not.
fn preview_report(url: &str, port: u16, scope: ListenScope, status: Option<u16>) -> String {
    let (ready, advice) = match (scope, status) {
        (ListenScope::Nothing, _) => (
            false,
            format!(
                "Nothing is listening on port {port} in the sandbox. Start the server in the \
                 background (`nohup … &`) bound to 0.0.0.0, wait for it to finish starting, and \
                 call this again."
            ),
        ),
        (ListenScope::LoopbackOnly, _) => (
            false,
            format!(
                "The server on port {port} listens only on localhost, so the preview cannot \
                 reach it. Restart it bound to 0.0.0.0 (for example `-H 0.0.0.0` for Next.js or \
                 `--host 0.0.0.0` for Vite)."
            ),
        ),
        (ListenScope::Reachable, None) => (
            false,
            format!(
                "Port {port} is open but the server did not answer an HTTP request. It may \
                 still be starting: check its log, then call this again."
            ),
        ),
        (ListenScope::Reachable, Some(code)) => (
            true,
            format!(
                "The server answers on port {port} (HTTP {code}). Give the user this URL; it is \
                 served by Temps, so no tunnel or outside service is needed."
            ),
        ),
    };
    json!({ "url": url, "ready": ready, "advice": advice }).to_string()
}

#[derive(Debug, Deserialize)]
struct OfferArgs {
    reason: String,
}

/// Whether `name` is one of the tools this module serves.
pub fn is_sandbox_tool(name: &str) -> bool {
    matches!(
        name,
        SANDBOX_EXEC_TOOL
            | SANDBOX_READ_TOOL
            | SANDBOX_WRITE_TOOL
            | SANDBOX_PREVIEW_TOOL
            | SANDBOX_OFFER_TOOL
            | SANDBOX_CREATE_TOOL
    )
}

/// The tools a chat gets for its sandbox situation.
///
/// Linked: the three scoped tools, if the caller may execute in sandboxes.
/// Unlinked, and the caller has approved changes without asking (`may_create`):
/// the same three plus `temps_create_sandbox`, which makes a brand-new sandbox
/// for this chat. They are offered together so the model can create one and use
/// it in the same turn; each call re-checks the link.
/// Unlinked otherwise: only the offer tool, once, and the user decides.
/// Either way the model never picks an existing sandbox.
pub fn tools_for(
    linked: bool,
    offered: bool,
    backend_available: bool,
    may_exec: bool,
    may_create: bool,
) -> Vec<ChatTool> {
    if !backend_available || !may_exec {
        return Vec::new();
    }
    if linked || may_create {
        let mut tools = vec![
            ChatTool {
                name: SANDBOX_EXEC_TOOL.to_string(),
                description: "Run a shell command in the sandbox the user linked to this chat. \
                    Use it for Git work (clone, branch, commit), unpacking files, installing \
                    dependencies and starting or testing a server. The command runs with `sh -c` \
                    in the sandbox work directory unless `cwd` is set. Returns the exit code, \
                    stdout and stderr. A non-zero exit code is a result, not a tool failure: read \
                    it and decide what to do. The sandbox has no Git or platform credentials, so \
                    private clones and pushes go through the Temps repository operations instead."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to run." },
                        "cwd": { "type": "string", "description": "Absolute working directory. Defaults to the sandbox work directory." },
                        "timeout_secs": { "type": "integer", "description": "Give up after this many seconds (1 to 600, default 120). Start long-lived servers in the background (`nohup … &`) instead of waiting on them." }
                    },
                    "required": ["command"]
                }),
            },
            ChatTool {
                name: SANDBOX_READ_TOOL.to_string(),
                description: "Read a text file from the linked sandbox. Relative paths resolve \
                    against the sandbox work directory."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path." }
                    },
                    "required": ["path"]
                }),
            },
            ChatTool {
                name: SANDBOX_WRITE_TOOL.to_string(),
                description: "Write a text file in the linked sandbox, replacing any existing \
                    file. Relative paths resolve against the sandbox work directory. At most \
                    1 MiB per call."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path." },
                        "content": { "type": "string", "description": "Full file content." },
                        "executable": { "type": "boolean", "description": "Mark the file executable." }
                    },
                    "required": ["path", "content"]
                }),
            },
            ChatTool {
                name: SANDBOX_PREVIEW_TOOL.to_string(),
                description: "Get the address the user opens in their browser to see a server \
                    running in the sandbox, and check that the server is listening. Start the \
                    server first, in the background and bound to 0.0.0.0. Call this every time you \
                    tell the user a page is running: it opens the page in the chat's Preview tab. \
                    Always give the user the URL this returns: it is served by Temps, so no \
                    tunnel or outside service is ever needed."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "port": { "type": "integer", "description": "Port the server listens on inside the sandbox (1 to 65535)." }
                    },
                    "required": ["port"]
                }),
            },
        ];
        if !linked {
            tools.push(create_tool());
        }
        return tools;
    }
    if offered {
        return Vec::new();
    }
    vec![ChatTool {
        name: SANDBOX_OFFER_TOOL.to_string(),
        description: "Offer the user a sandbox for this chat. Call it only when the next step \
            needs a shell and cannot be done with the platform tools: several Git operations \
            on a working copy, unpacking an uploaded archive, or running and testing a server. \
            Never call it for plain platform reads and changes. The user decides: they can \
            create a sandbox, link one of their own, or decline. It has no side effects and \
            you cannot link anything yourself. After calling it, tell the user in one sentence \
            what you want to do with the sandbox and stop; the shell tools appear on your next \
            turn once they link one. It can be called once per chat."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "reason": { "type": "string", "description": "One sentence on what you would do in the sandbox, e.g. 'unpack the uploaded archive and commit it to the new repository'." }
            },
            "required": ["reason"]
        }),
    }]
}

fn create_tool() -> ChatTool {
    ChatTool {
        name: SANDBOX_CREATE_TOOL.to_string(),
        description: "Create a new sandbox for this chat and link it, when none is linked. \
            Call it right away, without asking, when the user wants you to build or change an \
            app or site and the work needs a shell: writing code, installing dependencies, \
            running it and testing it. In a project chat the sandbox starts as a clone of the \
            project's repository; otherwise it is empty. It creates a new sandbox only; you \
            cannot pick or link an existing one. Do not call it for plain platform reads and \
            changes. When it returns, `sandbox_exec`, `sandbox_read_file` and \
            `sandbox_write_file` work in this same turn."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "reason": { "type": "string", "description": "One sentence on what you will build or run there." }
            },
            "required": ["reason"]
        }),
    }
}

/// Links a sandbox to a chat and runs the chat's scoped sandbox tools.
#[derive(Clone)]
pub struct SandboxLinkService {
    db: Arc<DatabaseConnection>,
    sandboxes: Arc<SandboxService>,
    audit: Option<Arc<dyn AuditLogger>>,
}

impl SandboxLinkService {
    pub fn new(db: Arc<DatabaseConnection>, sandboxes: Arc<SandboxService>) -> Self {
        Self {
            db,
            sandboxes,
            audit: None,
        }
    }

    /// Record sandboxes the assistant creates itself in the audit log.
    pub fn with_audit(mut self, audit: Arc<dyn AuditLogger>) -> Self {
        self.audit = Some(audit);
        self
    }

    fn db_err(conversation: &ai_conversations::Model, source: DbErr) -> SandboxLinkError {
        SandboxLinkError::Database {
            conversation_id: conversation.public_id.clone(),
            source,
        }
    }

    /// Merge one key into the conversation metadata in a single statement, so
    /// it cannot overwrite concurrent writers of other metadata keys.
    async fn merge_metadata_key(
        &self,
        conversation: &ai_conversations::Model,
        key: &str,
        value: &Value,
    ) -> Result<(), SandboxLinkError> {
        self.db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE ai_conversations \
                 SET metadata = (CASE WHEN jsonb_typeof(metadata) = 'object' THEN metadata ELSE '{}'::jsonb END) \
                     || jsonb_build_object($1::text, $2::jsonb) \
                 WHERE id = $3",
                [key.into(), value.clone().into(), conversation.id.into()],
            ))
            .await
            .map_err(|source| Self::db_err(conversation, source))?;
        Ok(())
    }

    async fn remove_metadata_key(
        &self,
        conversation: &ai_conversations::Model,
        key: &str,
    ) -> Result<(), SandboxLinkError> {
        self.db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE ai_conversations \
                 SET metadata = metadata - $1::text \
                 WHERE id = $2 AND jsonb_typeof(metadata) = 'object'",
                [key.into(), conversation.id.into()],
            ))
            .await
            .map_err(|source| Self::db_err(conversation, source))?;
        Ok(())
    }

    /// Link a sandbox the user already owns.
    pub async fn link_existing(
        &self,
        conversation: &ai_conversations::Model,
        user_id: i32,
        sandbox_id: &str,
    ) -> Result<SandboxLink, SandboxLinkError> {
        let row = self
            .sandboxes
            .find_by_public_id(sandbox_id, user_id)
            .await
            .map_err(|_| SandboxLinkError::SandboxNotFound {
                sandbox_id: sandbox_id.to_string(),
                user_id,
            })?;
        if let Some(reason) = linkability_problem(&row.name, row.agent_run_id, &row.status) {
            return Err(SandboxLinkError::NotLinkable {
                sandbox_id: sandbox_id.to_string(),
                conversation_id: conversation.public_id.clone(),
                reason: reason.to_string(),
            });
        }
        self.store_link(conversation, row.public_id, row.name).await
    }

    /// Create a fresh sandbox for the chat and link it.
    ///
    /// The create runs in a detached task. It inserts the sandbox row before
    /// the container exists, and a slow first image pull can outlast the wait
    /// below (or the chat turn can be cancelled): dropping the future at that
    /// point would leave a row with no container, which can never be resumed.
    /// The task finishes the create and links the sandbox regardless, and the
    /// caller only waits `CREATE_WAIT_SECS` for the answer.
    ///
    /// `assistant_reason` is `Some` when the model asked for the sandbox; the
    /// task then audits it, so a slow create is audited too.
    pub async fn create_and_link(
        &self,
        conversation: &ai_conversations::Model,
        user_id: i32,
        assistant_reason: Option<String>,
    ) -> Result<SandboxLink, SandboxLinkError> {
        let project_id = project_for_new_sandbox(conversation);
        let prefix = if project_id.is_some() {
            "project-chat"
        } else {
            "assistant-chat"
        };
        let name: String = format!("{prefix}-{}", conversation.public_id)
            .chars()
            .take(48)
            .collect();
        let request = CreateSandboxRequest {
            name: Some(name),
            timeout_secs: Some(CREATED_SANDBOX_TIMEOUT_SECS),
            lifecycle: Some("workspace".to_string()),
            project_id,
            ..Default::default()
        };
        let service = self.clone();
        let target = conversation.clone();
        let task = tokio::spawn(async move {
            let row = service
                .sandboxes
                .create_sandbox(user_id, request)
                .await
                .map_err(|error| SandboxLinkError::Sandbox {
                    conversation_id: target.public_id.clone(),
                    reason: format!("could not create a sandbox: {error}"),
                })?;
            let link = service.store_link(&target, row.public_id, row.name).await?;
            if let Some(reason) = assistant_reason {
                service
                    .audit_created(&target, user_id, &link.sandbox_id, &reason)
                    .await;
            }
            Ok::<SandboxLink, SandboxLinkError>(link)
        });
        match tokio::time::timeout(Duration::from_secs(CREATE_WAIT_SECS), task).await {
            Ok(Ok(result)) => result,
            Ok(Err(join_error)) => Err(SandboxLinkError::Sandbox {
                conversation_id: conversation.public_id.clone(),
                reason: format!("the sandbox creation stopped unexpectedly: {join_error}"),
            }),
            Err(_) => Err(SandboxLinkError::Sandbox {
                conversation_id: conversation.public_id.clone(),
                reason: format!(
                    "the sandbox was still starting after {CREATE_WAIT_SECS} seconds (the first \
                     one pulls its image). It keeps starting in the background and is linked to \
                     this chat automatically when it is ready: tell the user, then try the \
                     sandbox tools again in a minute"
                ),
            }),
        }
    }

    async fn store_link(
        &self,
        conversation: &ai_conversations::Model,
        sandbox_id: String,
        name: String,
    ) -> Result<SandboxLink, SandboxLinkError> {
        let link = SandboxLink {
            sandbox_id,
            name,
            linked_at: Utc::now(),
        };
        let value = serde_json::to_value(&link).map_err(|error| SandboxLinkError::Validation {
            conversation_id: conversation.public_id.clone(),
            message: format!("could not encode the link: {error}"),
        })?;
        self.merge_metadata_key(conversation, SANDBOX_LINK_KEY, &value)
            .await?;
        // A port confirmed on the previous sandbox says nothing about this one.
        self.remove_metadata_key(conversation, SANDBOX_PREVIEW_KEY)
            .await?;
        info!(
            conversation_id = %conversation.public_id,
            sandbox_id = %link.sandbox_id,
            "sandbox linked to chat"
        );
        Ok(link)
    }

    /// Remove the link. The sandbox itself is left running and untouched.
    pub async fn unlink(
        &self,
        conversation: &ai_conversations::Model,
    ) -> Result<(), SandboxLinkError> {
        self.remove_metadata_key(conversation, SANDBOX_LINK_KEY)
            .await?;
        self.remove_metadata_key(conversation, SANDBOX_PREVIEW_KEY)
            .await?;
        info!(
            conversation_id = %conversation.public_id,
            "sandbox unlinked from assistant chat"
        );
        Ok(())
    }

    /// Record that the user has been offered a sandbox. Returns `false` when
    /// they already had been.
    pub async fn mark_offered(&self, conversation_id: i64) -> Result<bool, SandboxLinkError> {
        let Some(conversation) = ai_conversations::Entity::find_by_id(conversation_id)
            .one(self.db.as_ref())
            .await
            .map_err(|source| SandboxLinkError::Database {
                conversation_id: conversation_id.to_string(),
                source,
            })?
        else {
            return Ok(false);
        };
        if offered_in_metadata(conversation.metadata.as_ref()) {
            return Ok(false);
        }
        self.merge_metadata_key(
            &conversation,
            SANDBOX_OFFERED_KEY,
            &json!(Utc::now().to_rfc3339()),
        )
        .await?;
        Ok(true)
    }

    /// The link of an owned conversation, read fresh from the database.
    async fn current_link(
        &self,
        conversation_id: i64,
        user_id: i32,
    ) -> Result<Option<SandboxLink>, DbErr> {
        let row = ai_conversations::Entity::find_by_id(conversation_id)
            .filter(ai_conversations::Column::CreatedBy.eq(user_id))
            .one(self.db.as_ref())
            .await?;
        Ok(row.and_then(|row| link_from_metadata(row.metadata.as_ref())))
    }

    /// Run one sandbox tool call from the model and return the text it reads
    /// back. Failures are sentences written to be acted on, never panics.
    pub async fn dispatch(
        &self,
        tool: &str,
        arguments: &str,
        conversation_id: i64,
        auth: &AuthContext,
    ) -> String {
        let user_id = auth.user_id();
        if tool == SANDBOX_OFFER_TOOL {
            return self.dispatch_offer(arguments, conversation_id).await;
        }
        if tool == SANDBOX_CREATE_TOOL {
            return self.dispatch_create(arguments, conversation_id, auth).await;
        }
        if !auth.has_permission(&Permission::SandboxesExec) {
            return format!(
                "`{tool}` is not available: running commands in a sandbox needs the {} permission.",
                Permission::SandboxesExec
            );
        }
        let link = match self.current_link(conversation_id, user_id).await {
            Ok(Some(link)) => link,
            Ok(None) => {
                return format!(
                    "`{tool}` is not available: no sandbox is linked to this chat. Create one \
                     with `{SANDBOX_CREATE_TOOL}` if you have it, otherwise offer one with \
                     `{SANDBOX_OFFER_TOOL}`, when a shell step is really needed."
                )
            }
            Err(error) => {
                warn!(conversation_id, "could not read the sandbox link: {error}");
                return format!(
                    "`{tool}` failed: could not read this chat's sandbox link: {error}"
                );
            }
        };
        // Ownership and linkability are checked again on every call: the
        // sandbox may have been destroyed or handed to a run since linking.
        let row = match self
            .sandboxes
            .find_by_public_id(&link.sandbox_id, user_id)
            .await
        {
            Ok(row) => row,
            Err(_) => {
                return format!(
                    "The linked sandbox {} no longer exists. Ask the user to link another one.",
                    link.sandbox_id
                )
            }
        };
        if let Some(reason) = linkability_problem(&row.name, row.agent_run_id, &row.status) {
            return format!(
                "The linked sandbox {} can no longer be used: {reason}.",
                link.sandbox_id
            );
        }
        let notice = if readiness(&row.status) == SandboxReadiness::Asleep {
            match self.wake(conversation_id, &link, user_id).await {
                Ok(notice) => Some(notice),
                Err(message) => return message,
            }
        } else {
            None
        };
        let result = match tool {
            SANDBOX_EXEC_TOOL => {
                self.run_exec(arguments, conversation_id, &link, &row.work_dir, user_id)
                    .await
            }
            SANDBOX_READ_TOOL => {
                self.run_read(arguments, conversation_id, &link, &row.work_dir, user_id)
                    .await
            }
            SANDBOX_WRITE_TOOL => {
                self.run_write(arguments, conversation_id, &link, &row.work_dir, user_id)
                    .await
            }
            SANDBOX_PREVIEW_TOOL => {
                self.run_preview(arguments, conversation_id, &link, user_id)
                    .await
            }
            other => format!("Unknown sandbox tool `{other}`."),
        };
        with_notice(notice, result)
    }

    /// Wake a sleeping linked sandbox. `Ok` is the notice that goes in front
    /// of the tool result, which is how the user sees in the chat that it
    /// happened; `Err` is the complete tool result for a sandbox that stayed
    /// down.
    async fn wake(
        &self,
        conversation_id: i64,
        link: &SandboxLink,
        user_id: i32,
    ) -> Result<String, String> {
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(RESUME_WAIT_SECS),
            self.sandboxes.resume_sandbox(&link.sandbox_id, user_id),
        )
        .await;
        match outcome {
            Ok(Ok(_)) => {
                let seconds = started.elapsed().as_secs();
                info!(
                    conversation_id,
                    sandbox_id = %link.sandbox_id,
                    seconds,
                    "resumed the chat's sleeping sandbox"
                );
                Ok(format!(
                    "Sandbox {} was asleep and has been resumed (took {seconds}s). Its files \
                     are intact. Mention in one short sentence that you woke it.",
                    link.name
                ))
            }
            Ok(Err(SandboxError::NotFound { .. })) => {
                Err(self.container_gone(conversation_id, link).await)
            }
            Ok(Err(error)) => {
                warn!(
                    conversation_id,
                    sandbox_id = %link.sandbox_id,
                    "could not resume the chat's sandbox: {error}"
                );
                Err(format!(
                    "The linked sandbox {} is asleep and could not be resumed: {error}. Tell the \
                     user, and that they can press Resume on the Sandboxes page. Do not move the \
                     work to Git providers or other places because of this.",
                    link.name
                ))
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    sandbox_id = %link.sandbox_id,
                    "the chat's sandbox did not resume within {RESUME_WAIT_SECS}s"
                );
                Err(format!(
                    "The linked sandbox {} is asleep and did not resume within {RESUME_WAIT_SECS} \
                     seconds, so the container runtime on this server may be unhealthy. Tell the \
                     user, and that they can press Resume on the Sandboxes page and then ask \
                     again. Do not move the work to Git providers or other places because of \
                     this.",
                    link.name
                ))
            }
        }
    }

    /// The sandbox row exists but its container does not, so it can never run
    /// again. Unlink it, so the chat can get a working one, and say so.
    async fn container_gone(&self, conversation_id: i64, link: &SandboxLink) -> String {
        match ai_conversations::Entity::find_by_id(conversation_id)
            .one(self.db.as_ref())
            .await
        {
            Ok(Some(conversation)) => {
                if let Err(error) = self.unlink(&conversation).await {
                    warn!(
                        conversation_id,
                        "could not unlink a vanished sandbox: {error}"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => warn!(
                conversation_id,
                "could not load the conversation to unlink a vanished sandbox: {error}"
            ),
        }
        warn!(
            conversation_id,
            sandbox_id = %link.sandbox_id,
            "the chat's sandbox has no container any more; unlinked it"
        );
        format!(
            "The linked sandbox {} ({}) has no container on this server any more, so it cannot be \
             resumed or used. It has been unlinked from this chat. Tell the user in one sentence \
             and ask them to send their request again: a new sandbox is created for it. Do not \
             move the work to Git providers or other places because of this.",
            link.name, link.sandbox_id
        )
    }

    /// A sandbox tool failed. A sandbox that is gone is handled like one that
    /// cannot be woken; anything else is reported as it happened.
    async fn tool_failure(
        &self,
        what: &str,
        error: SandboxError,
        conversation_id: i64,
        link: &SandboxLink,
    ) -> String {
        if matches!(error, SandboxError::NotFound { .. }) {
            return self.container_gone(conversation_id, link).await;
        }
        format!("{what}: {error}")
    }

    /// Create a sandbox for the chat on the model's request. The sandbox is
    /// always new and always owned by the caller, so the model never chooses
    /// which sandbox it gets.
    async fn dispatch_create(
        &self,
        arguments: &str,
        conversation_id: i64,
        auth: &AuthContext,
    ) -> String {
        let user_id = auth.user_id();
        let reason = serde_json::from_str::<OfferArgs>(arguments)
            .map(|args| args.reason)
            .unwrap_or_default();
        for permission in [Permission::SandboxesExec, Permission::SandboxesWrite] {
            if !auth.has_permission(&permission) {
                return format!(
                    "`{SANDBOX_CREATE_TOOL}` is not available: creating a sandbox needs the \
                     {permission} permission. Tell the user, and carry on without a shell."
                );
            }
        }
        let conversation = match ai_conversations::Entity::find_by_id(conversation_id)
            .filter(ai_conversations::Column::CreatedBy.eq(user_id))
            .one(self.db.as_ref())
            .await
        {
            Ok(Some(conversation)) => conversation,
            Ok(None) => {
                return format!(
                    "`{SANDBOX_CREATE_TOOL}` failed: conversation {conversation_id} was not found."
                )
            }
            Err(error) => {
                warn!(conversation_id, "could not load the conversation: {error}");
                return format!(
                    "`{SANDBOX_CREATE_TOOL}` failed: could not load this chat: {error}"
                );
            }
        };
        if !context_supports_sandbox(&conversation.context_type) {
            return format!("`{SANDBOX_CREATE_TOOL}` is not available in this kind of chat.");
        }
        if project_for_new_sandbox(&conversation).is_some()
            && !auth.has_permission(&Permission::GitRepositoriesRead)
        {
            return format!(
                "`{SANDBOX_CREATE_TOOL}` is not available: cloning the project's repository \
                 needs the {} permission.",
                Permission::GitRepositoriesRead
            );
        }
        if let Some(link) = link_from_metadata(conversation.metadata.as_ref()) {
            return format!(
                "A sandbox is already linked to this chat: {} ({}). Use the sandbox tools on it.",
                link.name, link.sandbox_id
            );
        }
        match self
            .create_and_link(&conversation, user_id, Some(reason))
            .await
        {
            Ok(link) => {
                let seeded = if project_for_new_sandbox(&conversation).is_some() {
                    "It starts as a clone of the project's repository."
                } else {
                    "It is empty."
                };
                format!(
                    "Created sandbox {} ({}) and linked it to this chat. {seeded} Use \
                     `{SANDBOX_EXEC_TOOL}`, `{SANDBOX_READ_TOOL}` and `{SANDBOX_WRITE_TOOL}` on it \
                     now. Tell the user in one sentence that you created it.",
                    link.name, link.sandbox_id
                )
            }
            Err(error) => {
                warn!(
                    conversation_id,
                    "could not create a sandbox for the chat: {error}"
                );
                format!(
                    "`{SANDBOX_CREATE_TOOL}` failed: {error}. Tell the user why and carry on \
                     without a shell; they can link a sandbox themselves from the chat's \
                     Sandbox button."
                )
            }
        }
    }

    async fn audit_created(
        &self,
        conversation: &ai_conversations::Model,
        user_id: i32,
        sandbox_id: &str,
        reason: &str,
    ) {
        let Some(audit) = self.audit.as_ref() else {
            return;
        };
        let entry = crate::audit::AiSandboxLinkedAudit {
            context: AuditContext {
                user_id,
                ip_address: None,
                user_agent: "temps-ai-assistant".to_string(),
            },
            project_id: conversation.project_id,
            conversation_id: conversation.public_id.clone(),
            sandbox_id: sandbox_id.to_string(),
            created: true,
        };
        if let Err(error) = audit.create_audit_log(&entry).await {
            warn!(
                conversation_id = %conversation.public_id,
                sandbox_id,
                reason,
                "failed to audit the sandbox the assistant created: {error}"
            );
        }
    }

    async fn dispatch_offer(&self, arguments: &str, conversation_id: i64) -> String {
        let args: OfferArgs = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(error) => {
                return format!(
                "`{SANDBOX_OFFER_TOOL}` needs {{\"reason\": \"…\"}}: {error}. Nothing was offered."
            )
            }
        };
        match self.mark_offered(conversation_id).await {
            Ok(true) => format!(
                "The user was shown an offer to create or link a sandbox so you can: {}. Tell \
                 them in one sentence what you want to do there and stop. Do not run shell \
                 steps until the sandbox tools appear.",
                args.reason.trim()
            ),
            Ok(false) => "A sandbox was already offered in this chat. Do not offer again; \
                continue with the platform tools or tell the user what you could do with a \
                sandbox if they link one."
                .to_string(),
            Err(error) => format!("`{SANDBOX_OFFER_TOOL}` failed: {error}. Nothing was offered."),
        }
    }

    async fn run_exec(
        &self,
        arguments: &str,
        conversation_id: i64,
        link: &SandboxLink,
        work_dir: &str,
        user_id: i32,
    ) -> String {
        let args: ExecArgs = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(error) => {
                return format!(
                    "`{SANDBOX_EXEC_TOOL}` needs {{\"command\": \"…\"}}: {error}. Nothing ran."
                )
            }
        };
        if args.command.trim().is_empty() {
            return format!("`{SANDBOX_EXEC_TOOL}` needs a non-empty command. Nothing ran.");
        }
        if let Some(service) = third_party_exposure(&args.command) {
            return format!(
                "Nothing ran: `{service}` would publish the sandbox through a third party, and \
                 Temps does not use one. Use `{SANDBOX_PREVIEW_TOOL}` with the server's port to \
                 get the address the user opens (it is served by Temps; the `show-a-running-app` \
                 playbook has the steps), or deploy the code as a Temps project to publish it. If the user asked for that service by name, tell \
                 them this sandbox does not run it."
            );
        }
        let timeout = Duration::from_secs(
            args.timeout_secs
                .unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)
                .clamp(1, MAX_EXEC_TIMEOUT_SECS),
        );
        let options = ExecOptions {
            cmd: vec!["sh".to_string(), "-c".to_string(), args.command],
            env: Default::default(),
            cwd: Some(
                args.cwd
                    .map(|cwd| resolve_path(work_dir, &cwd))
                    .unwrap_or_else(|| work_dir.to_string()),
            ),
        };
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            timeout,
            self.sandboxes.exec(&link.sandbox_id, user_id, options),
        )
        .await;
        match outcome {
            Err(_) => format!(
                "The command did not finish within {} seconds and was abandoned; it may still \
                 be running in the sandbox. Start long-lived servers in the background with \
                 `nohup … &` and check them with a second command.",
                timeout.as_secs()
            ),
            Ok(Err(error)) => {
                self.tool_failure("The command could not run", error, conversation_id, link)
                    .await
            }
            Ok(Ok(result)) => {
                info!(
                    sandbox_id = %link.sandbox_id,
                    exit_code = result.exit_code,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "assistant chat ran a command in its linked sandbox"
                );
                // The address a server started in the sandbox answers on. The
                // model must quote this one: the gateway behind it only
                // accepts the Temps proxy, so any other host or port it
                // invents is refused.
                let preview_url = self
                    .sandboxes
                    .preview_parts()
                    .await
                    .host_template(&link.sandbox_id);
                json!({
                    "exit_code": result.exit_code,
                    "stdout": cap_tail(&result.stdout, MAX_TOOL_OUTPUT_BYTES / 2),
                    "stderr": cap_tail(&result.stderr, MAX_TOOL_OUTPUT_BYTES / 2),
                    "preview_url_template": preview_url,
                    "preview_note": format!(
                        "To show the user a page, call `{SANDBOX_PREVIEW_TOOL}` with its port: it \
                         checks the server and opens the page in the chat's Preview tab. Do not \
                         fill in the template yourself."
                    ),
                })
                .to_string()
            }
        }
    }

    async fn run_preview(
        &self,
        arguments: &str,
        conversation_id: i64,
        link: &SandboxLink,
        user_id: i32,
    ) -> String {
        let args: PreviewArgs = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(error) => {
                return format!("`{SANDBOX_PREVIEW_TOOL}` needs {{\"port\": 3000}}: {error}.")
            }
        };
        let Some(port) = args.port.filter(|port| *port >= 1) else {
            return format!("`{SANDBOX_PREVIEW_TOOL}` needs a port between 1 and 65535.");
        };
        // One command answers both questions: where the port listens, and
        // whether the server answers on it.
        let probe = format!(
            "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null; echo '{PROBE_SEPARATOR}'; \
             curl -s -m 5 -o /dev/null -w '%{{http_code}}' http://127.0.0.1:{port}/ 2>/dev/null \
             || curl -s -m 5 -o /dev/null -w '%{{http_code}}' 'http://[::1]:{port}/' 2>/dev/null; \
             true"
        );
        let options = ExecOptions {
            cmd: vec!["sh".to_string(), "-c".to_string(), probe],
            env: Default::default(),
            cwd: None,
        };
        let output = match tokio::time::timeout(
            Duration::from_secs(20),
            self.sandboxes.exec(&link.sandbox_id, user_id, options),
        )
        .await
        {
            Err(_) => {
                return "Could not check the server: the sandbox did not answer within 20 seconds."
                    .to_string()
            }
            Ok(Err(error)) => {
                return self
                    .tool_failure("Could not check the server", error, conversation_id, link)
                    .await
            }
            Ok(Ok(result)) => result.stdout,
        };
        let (table, status) = output
            .split_once(PROBE_SEPARATOR)
            .unwrap_or((output.as_str(), ""));
        let status = status.trim().parse::<u16>().ok().filter(|code| *code != 0);
        let scope = listen_scope(table, port);
        let url = self
            .sandboxes
            .preview_parts()
            .await
            .url_for(&link.sandbox_id, port);
        if scope == ListenScope::Reachable && status.is_some() {
            self.remember_preview(conversation_id, port).await;
        }
        preview_report(&url, port, scope, status)
    }

    /// Record the port the assistant just confirmed, for the Preview tab. The
    /// tool result does not depend on it, so a failure is logged and dropped.
    async fn remember_preview(&self, conversation_id: i64, port: u16) {
        let conversation = match ai_conversations::Entity::find_by_id(conversation_id)
            .one(self.db.as_ref())
            .await
        {
            Ok(Some(conversation)) => conversation,
            Ok(None) => return,
            Err(error) => {
                warn!(conversation_id, port, error = %error, "could not load the chat to record its preview port");
                return;
            }
        };
        if let Err(error) = self
            .merge_metadata_key(&conversation, SANDBOX_PREVIEW_KEY, &json!(port))
            .await
        {
            warn!(conversation_id, port, error = %error, "could not record the chat's preview port");
        }
    }

    async fn run_read(
        &self,
        arguments: &str,
        conversation_id: i64,
        link: &SandboxLink,
        work_dir: &str,
        user_id: i32,
    ) -> String {
        let args: ReadArgs = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(error) => {
                return format!("`{SANDBOX_READ_TOOL}` needs {{\"path\": \"…\"}}: {error}.")
            }
        };
        let path = resolve_path(work_dir, &args.path);
        match self
            .sandboxes
            .fs_read(&link.sandbox_id, user_id, &path)
            .await
        {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => cap_head(&text, MAX_TOOL_OUTPUT_BYTES),
                Err(error) => format!(
                    "{path} is not UTF-8 text ({} bytes). Inspect it with `{SANDBOX_EXEC_TOOL}` \
                     (for example `file` or `head -c 200 | xxd`).",
                    error.as_bytes().len()
                ),
            },
            Err(error) => {
                self.tool_failure(
                    &format!("Could not read {path}"),
                    error,
                    conversation_id,
                    link,
                )
                .await
            }
        }
    }

    async fn run_write(
        &self,
        arguments: &str,
        conversation_id: i64,
        link: &SandboxLink,
        work_dir: &str,
        user_id: i32,
    ) -> String {
        let args: WriteArgs = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(error) => {
                return format!(
                    "`{SANDBOX_WRITE_TOOL}` needs {{\"path\": \"…\", \"content\": \"…\"}}: {error}. Nothing was written."
                )
            }
        };
        if args.content.len() > MAX_WRITE_BYTES {
            return format!(
                "The content is {} bytes; `{SANDBOX_WRITE_TOOL}` accepts at most {MAX_WRITE_BYTES}. \
                 Split the file or generate it with `{SANDBOX_EXEC_TOOL}`. Nothing was written.",
                args.content.len()
            );
        }
        let path = resolve_path(work_dir, &args.path);
        let mode = if args.executable { 0o755 } else { 0o644 };
        match self
            .sandboxes
            .fs_write(
                &link.sandbox_id,
                user_id,
                &path,
                args.content.as_bytes(),
                mode,
            )
            .await
        {
            Ok(()) => format!("Wrote {} bytes to {path}.", args.content.len()),
            Err(error) => {
                self.tool_failure(
                    &format!("Could not write {path}"),
                    error,
                    conversation_id,
                    link,
                )
                .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn only_a_stopped_sandbox_needs_waking() {
        assert_eq!(readiness("stopped"), SandboxReadiness::Asleep);
        assert_eq!(readiness("running"), SandboxReadiness::Ready);
    }

    #[test]
    fn a_notice_goes_in_front_of_the_tool_result() {
        let result = with_notice(
            Some("Resumed.".to_string()),
            "{\"exit_code\":0}".to_string(),
        );
        assert_eq!(result, "Resumed.\n\n{\"exit_code\":0}");
        assert_eq!(with_notice(None, "ok".to_string()), "ok");
    }

    #[test]
    fn third_party_tunnels_are_recognised() {
        for command in [
            "./cloudflared tunnel --url http://localhost:3100",
            "curl -L -o cf https://github.com/cloudflare/cloudflared/releases/latest/download/x",
            "npx -y untun@latest tunnel http://localhost:3000",
            "ngrok http 3000",
            "NGROK_AUTHTOKEN=x ngrok http 3000",
            "npx localtunnel --port 3000",
            "ssh -R 80:localhost:3000 serveo.net",
            "ssh -R 80:localhost:3000 nokey@localhost.run",
            "tailscale funnel 3000",
        ] {
            assert!(third_party_exposure(command).is_some(), "{command}");
        }
    }

    #[test]
    fn ordinary_commands_are_not_mistaken_for_tunnels() {
        for command in [
            "npm run dev -- -H 0.0.0.0 -p 3000",
            "curl -s localhost:3000",
            "git clone https://github.com/example/app && cd app && npm install",
            "echo uptuning && ls",
            "grep -r 'ngrokish' .",
        ] {
            assert_eq!(third_party_exposure(command), None, "{command}");
        }
    }

    const PROC_TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000:1388 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12 1 0000000000000000 100 0 0 10 0\n\
   2: 0300A8C0:2710 0100A8C0:C350 01 00000000:00000000 00:00000000 00000000  1000        0 13 1 0000000000000000 100 0 0 10 0\n\
   3: 00000000000000000000000000000000:0C1C 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 14 1 0000000000000000 100 0 0 10 0\n\
   4: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 15 1 0000000000000000 100 0 0 10 0\n";

    #[test]
    fn listen_scope_tells_loopback_from_reachable() {
        // 3000 (0x0BB8) listens on 127.0.0.1 only.
        assert_eq!(listen_scope(PROC_TCP, 3000), ListenScope::LoopbackOnly);
        // 5000 (0x1388) on every IPv4 address.
        assert_eq!(listen_scope(PROC_TCP, 5000), ListenScope::Reachable);
        // 3100 (0x0C1C) on every IPv6 address.
        assert_eq!(listen_scope(PROC_TCP, 3100), ListenScope::Reachable);
        // 8080 (0x1F90) on ::1 only.
        assert_eq!(listen_scope(PROC_TCP, 8080), ListenScope::LoopbackOnly);
        // 10000 (0x2710) is an established connection, not a listener.
        assert_eq!(listen_scope(PROC_TCP, 10000), ListenScope::Nothing);
        assert_eq!(listen_scope(PROC_TCP, 9999), ListenScope::Nothing);
        assert_eq!(listen_scope("", 3000), ListenScope::Nothing);
    }

    #[test]
    fn the_recorded_preview_port_must_be_a_real_port() {
        let port =
            |value: Value| preview_port_from_metadata(Some(&json!({ SANDBOX_PREVIEW_KEY: value })));
        assert_eq!(port(json!(3000)), Some(3000));
        assert_eq!(port(json!(0)), None);
        assert_eq!(port(json!(70000)), None);
        assert_eq!(port(json!("3000")), None);
        assert_eq!(preview_port_from_metadata(None), None);
        assert_eq!(preview_port_from_metadata(Some(&json!({}))), None);
    }

    #[test]
    fn preview_report_explains_every_failure_and_only_blesses_a_working_server() {
        let url = "http://ws-abc-3000.localho.st:8080";
        let ok: Value = serde_json::from_str(&preview_report(
            url,
            3000,
            ListenScope::Reachable,
            Some(200),
        ))
        .unwrap();
        assert_eq!(ok["ready"], true);
        assert_eq!(ok["url"], url);
        for (scope, status, needle) in [
            (ListenScope::Nothing, None, "Nothing is listening"),
            (ListenScope::LoopbackOnly, Some(200), "only on localhost"),
            (ListenScope::Reachable, None, "did not answer"),
        ] {
            let report: Value =
                serde_json::from_str(&preview_report(url, 3000, scope, status)).unwrap();
            assert_eq!(report["ready"], false);
            assert!(report["advice"].as_str().unwrap().contains(needle));
        }
    }

    fn link() -> SandboxLink {
        SandboxLink {
            sandbox_id: "sbx_abc123".to_string(),
            name: "assistant-chat-x".to_string(),
            linked_at: Utc.with_ymd_and_hms(2026, 10, 17, 9, 30, 0).unwrap(),
        }
    }

    fn conversation(context_type: &str, project_id: Option<i32>) -> ai_conversations::Model {
        let now = Utc::now();
        ai_conversations::Model {
            id: 1,
            public_id: "conv_test".to_string(),
            project_id,
            application_id: None,
            context_type: context_type.to_string(),
            context_id: "ctx".to_string(),
            title: None,
            status: "active".to_string(),
            created_by: 7,
            metadata: None,
            created_at: now,
            last_activity_at: now,
            ai_provider: "anthropic".to_string(),
            ai_model: "model".to_string(),
            ai_thinking_level: None,
            ai_permission_mode: "auto".to_string(),
            cli_session_id: None,
            cli_session_fingerprint: None,
            turn_status: "idle".to_string(),
            active_turn_id: None,
            last_turn_id: None,
            turn_started_at: None,
        }
    }

    #[test]
    fn only_assistant_and_project_chats_support_sandboxes() {
        assert!(context_supports_sandbox("assistant"));
        assert!(context_supports_sandbox("project"));
        for other in ["deployment", "alert", "application", "global", ""] {
            assert!(!context_supports_sandbox(other), "{other}");
        }
    }

    #[test]
    fn a_new_sandbox_is_seeded_from_the_project_of_a_project_chat_only() {
        assert_eq!(
            project_for_new_sandbox(&conversation("project", Some(42))),
            Some(42)
        );
        assert_eq!(
            project_for_new_sandbox(&conversation("project", None)),
            None
        );
        assert_eq!(
            project_for_new_sandbox(&conversation("assistant", Some(42))),
            None
        );
    }

    #[test]
    fn link_round_trips_through_conversation_metadata() {
        let metadata = json!({
            "context_usage": { "tokens": 10 },
            SANDBOX_LINK_KEY: serde_json::to_value(link()).unwrap(),
        });
        assert_eq!(link_from_metadata(Some(&metadata)), Some(link()));
    }

    #[test]
    fn missing_or_malformed_link_reads_as_unlinked() {
        assert_eq!(link_from_metadata(None), None);
        assert_eq!(link_from_metadata(Some(&json!({}))), None);
        assert_eq!(
            link_from_metadata(Some(&json!({ SANDBOX_LINK_KEY: "sbx_abc123" }))),
            None
        );
        assert_eq!(
            link_from_metadata(Some(&json!({ SANDBOX_LINK_KEY: { "sandbox_id": 4 } }))),
            None
        );
    }

    #[test]
    fn offered_flag_is_read_from_metadata() {
        assert!(!offered_in_metadata(None));
        assert!(!offered_in_metadata(Some(&json!({}))));
        assert!(!offered_in_metadata(Some(
            &json!({ SANDBOX_OFFERED_KEY: null })
        )));
        assert!(offered_in_metadata(Some(
            &json!({ SANDBOX_OFFERED_KEY: "2026-10-17T09:30:00Z" })
        )));
    }

    #[test]
    fn application_workspaces_agent_runs_and_destroyed_sandboxes_are_not_linkable() {
        assert!(linkability_problem("ai-application:global-user-7", None, "running").is_some());
        assert!(linkability_problem("scratch", Some(12), "running").is_some());
        assert!(linkability_problem("scratch", None, "destroyed").is_some());
        assert!(linkability_problem("scratch", None, "running").is_none());
        assert!(linkability_problem("scratch", None, "stopped").is_none());
    }

    #[test]
    fn long_output_keeps_its_tail_on_a_char_boundary() {
        let text = format!("{}end", "é".repeat(100));
        let capped = cap_tail(&text, 11);
        assert!(capped.ends_with("end"));
        assert!(capped.starts_with("[earlier output truncated"));
        assert_eq!(cap_tail("short", 100), "short");
    }

    #[test]
    fn long_files_keep_their_head_on_a_char_boundary() {
        let text = "é".repeat(100);
        let capped = cap_head(&text, 11);
        assert!(capped.starts_with("ééééé"));
        assert!(capped.contains("[truncated"));
        assert_eq!(cap_head("short", 100), "short");
    }

    #[test]
    fn relative_paths_resolve_against_the_work_dir() {
        assert_eq!(
            resolve_path("/workspace", "src/a.rs"),
            "/workspace/src/a.rs"
        );
        assert_eq!(resolve_path("/workspace/", "a"), "/workspace/a");
        assert_eq!(resolve_path("/workspace", "/etc/hosts"), "/etc/hosts");
    }

    fn names(tools: Vec<ChatTool>) -> Vec<String> {
        tools.into_iter().map(|tool| tool.name).collect()
    }

    #[test]
    fn a_linked_chat_gets_the_scoped_tools_and_no_offer() {
        for may_create in [false, true] {
            assert_eq!(
                names(tools_for(true, false, true, true, may_create)),
                [
                    SANDBOX_EXEC_TOOL,
                    SANDBOX_READ_TOOL,
                    SANDBOX_WRITE_TOOL,
                    SANDBOX_PREVIEW_TOOL
                ]
            );
        }
    }

    #[test]
    fn an_unlinked_chat_gets_only_the_offer_until_it_was_made() {
        assert_eq!(
            names(tools_for(false, false, true, true, false)),
            [SANDBOX_OFFER_TOOL]
        );
        assert!(tools_for(false, true, true, true, false).is_empty());
    }

    #[test]
    fn an_unlinked_chat_that_may_create_gets_create_and_the_shell_tools_together() {
        // Together, so the sandbox it creates is usable in the same turn.
        for offered in [false, true] {
            assert_eq!(
                names(tools_for(false, offered, true, true, true)),
                [
                    SANDBOX_EXEC_TOOL,
                    SANDBOX_READ_TOOL,
                    SANDBOX_WRITE_TOOL,
                    SANDBOX_PREVIEW_TOOL,
                    SANDBOX_CREATE_TOOL
                ]
            );
        }
        assert!(is_sandbox_tool(SANDBOX_CREATE_TOOL));
    }

    #[test]
    fn no_backend_or_no_exec_permission_offers_nothing() {
        for may_create in [false, true] {
            assert!(tools_for(true, false, false, true, may_create).is_empty());
            assert!(tools_for(false, false, false, true, may_create).is_empty());
            assert!(tools_for(true, false, true, false, may_create).is_empty());
            assert!(tools_for(false, false, true, false, may_create).is_empty());
        }
    }

    #[test]
    fn error_messages_name_the_conversation_and_sandbox() {
        let error = SandboxLinkError::NotLinkable {
            sandbox_id: "sbx_1".into(),
            conversation_id: "conv_1".into(),
            reason: "it has been destroyed".into(),
        };
        let message = error.to_string();
        assert!(message.contains("sbx_1") && message.contains("conv_1"));
        assert!(SandboxLinkError::NotLinked {
            conversation_id: "conv_2".into()
        }
        .to_string()
        .contains("conv_2"));
    }
}
