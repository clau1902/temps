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
use temps_entities::ai_conversations;
use temps_sandbox::services::{CreateSandboxRequest, ExecOptions};
use temps_sandbox::SandboxService;
use thiserror::Error;
use tracing::{info, warn};
use utoipa::ToSchema;

use crate::providers::assistant::ASSISTANT_CONTEXT_TYPE;

/// Key under `ai_conversations.metadata` that holds the link.
pub const SANDBOX_LINK_KEY: &str = "sandbox_link";
/// Key recording that the user has already been offered a sandbox in this chat.
pub const SANDBOX_OFFERED_KEY: &str = "sandbox_offered_at";

pub const SANDBOX_EXEC_TOOL: &str = "sandbox_exec";
pub const SANDBOX_READ_TOOL: &str = "sandbox_read_file";
pub const SANDBOX_WRITE_TOOL: &str = "sandbox_write_file";
pub const SANDBOX_OFFER_TOOL: &str = "temps_offer_sandbox";

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
struct OfferArgs {
    reason: String,
}

/// Whether `name` is one of the tools this module serves.
pub fn is_sandbox_tool(name: &str) -> bool {
    matches!(
        name,
        SANDBOX_EXEC_TOOL | SANDBOX_READ_TOOL | SANDBOX_WRITE_TOOL | SANDBOX_OFFER_TOOL
    )
}

/// The tools a chat gets for its sandbox situation.
///
/// Linked: the three scoped tools, if the caller may execute in sandboxes.
/// Unlinked: only the offer tool, if the caller could link one. Either way the
/// model never picks the sandbox.
pub fn tools_for(
    linked: bool,
    offered: bool,
    backend_available: bool,
    may_exec: bool,
) -> Vec<ChatTool> {
    if !backend_available || !may_exec {
        return Vec::new();
    }
    if linked {
        return vec![
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
        ];
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

/// Links a sandbox to a chat and runs the chat's scoped sandbox tools.
#[derive(Clone)]
pub struct SandboxLinkService {
    db: Arc<DatabaseConnection>,
    sandboxes: Arc<SandboxService>,
}

impl SandboxLinkService {
    pub fn new(db: Arc<DatabaseConnection>, sandboxes: Arc<SandboxService>) -> Self {
        Self { db, sandboxes }
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
    pub async fn create_and_link(
        &self,
        conversation: &ai_conversations::Model,
        user_id: i32,
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
        let creating = self.sandboxes.create_sandbox(
            user_id,
            CreateSandboxRequest {
                name: Some(name),
                timeout_secs: Some(CREATED_SANDBOX_TIMEOUT_SECS),
                lifecycle: Some("workspace".to_string()),
                project_id,
                ..Default::default()
            },
        );
        let row = tokio::time::timeout(Duration::from_secs(CREATE_WAIT_SECS), creating)
            .await
            .map_err(|_| SandboxLinkError::Sandbox {
                conversation_id: conversation.public_id.clone(),
                reason: format!(
                    "the sandbox was still starting after {CREATE_WAIT_SECS} seconds. It keeps \
                     starting in the background: link it from your sandbox list once it is \
                     running, or check that this server's sandbox backend is healthy"
                ),
            })?
            .map_err(|error| SandboxLinkError::Sandbox {
                conversation_id: conversation.public_id.clone(),
                reason: format!("could not create a sandbox: {error}"),
            })?;
        self.store_link(conversation, row.public_id, row.name).await
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
                    "`{tool}` is not available: no sandbox is linked to this chat. Offer one \
                     with `{SANDBOX_OFFER_TOOL}` if a shell step is really needed."
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
        match tool {
            SANDBOX_EXEC_TOOL => {
                self.run_exec(arguments, &link, &row.work_dir, user_id)
                    .await
            }
            SANDBOX_READ_TOOL => {
                self.run_read(arguments, &link, &row.work_dir, user_id)
                    .await
            }
            SANDBOX_WRITE_TOOL => {
                self.run_write(arguments, &link, &row.work_dir, user_id)
                    .await
            }
            other => format!("Unknown sandbox tool `{other}`."),
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
            Ok(Err(error)) => format!("The command could not run: {error}"),
            Ok(Ok(result)) => {
                info!(
                    sandbox_id = %link.sandbox_id,
                    exit_code = result.exit_code,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "assistant chat ran a command in its linked sandbox"
                );
                json!({
                    "exit_code": result.exit_code,
                    "stdout": cap_tail(&result.stdout, MAX_TOOL_OUTPUT_BYTES / 2),
                    "stderr": cap_tail(&result.stderr, MAX_TOOL_OUTPUT_BYTES / 2),
                })
                .to_string()
            }
        }
    }

    async fn run_read(
        &self,
        arguments: &str,
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
            Err(error) => format!("Could not read {path}: {error}"),
        }
    }

    async fn run_write(
        &self,
        arguments: &str,
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
            Err(error) => format!("Could not write {path}: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
        assert_eq!(
            names(tools_for(true, false, true, true)),
            [SANDBOX_EXEC_TOOL, SANDBOX_READ_TOOL, SANDBOX_WRITE_TOOL]
        );
    }

    #[test]
    fn an_unlinked_chat_gets_only_the_offer_until_it_was_made() {
        assert_eq!(
            names(tools_for(false, false, true, true)),
            [SANDBOX_OFFER_TOOL]
        );
        assert!(tools_for(false, true, true, true).is_empty());
    }

    #[test]
    fn no_backend_or_no_exec_permission_offers_nothing() {
        assert!(tools_for(true, false, false, true).is_empty());
        assert!(tools_for(false, false, false, true).is_empty());
        assert!(tools_for(true, false, true, false).is_empty());
        assert!(tools_for(false, false, true, false).is_empty());
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
