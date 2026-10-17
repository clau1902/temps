// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deploy the files in a chat's sandbox as a project's source, with no Git.
//!
//! A chat that builds an app in its sandbox already has the files. Publishing
//! them should not need a repository: the server packs the sandbox directory,
//! leaves out what must never be uploaded (`node_modules`, `.git`, `.env`
//! files, credentials, build output), and hands the archive to the same
//! uploaded-source pipeline that `temps deploy drop` and the application
//! workspaces use.
//!
//! The packing is done in two steps so the boundary does not depend on the
//! sandbox: the sandbox only runs `tar`, which is a size optimisation, and the
//! server re-reads the tarball entry by entry. Every path is checked, links and
//! special files are refused, and the server decides what is excluded. Whatever
//! the model or a command wrote into the tarball, the archive that is deployed
//! is the one this module builds.
//!
//! The deploy is a write to the platform, so the chat runtime asks the user to
//! approve it (or approves it automatically in the modes that do) before
//! [`SandboxLinkService::deploy_prepared`] runs; the approver's own permissions
//! are checked again at that point.

use std::io::{Read, Write};
use std::path::{Component, Path};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;
use temps_ai::ChatTool;
use temps_auth::context::AuthContext;
use temps_auth::permissions::Permission;
use temps_core::{AuditContext, SourceDropRequest};
use thiserror::Error;
use tracing::{info, warn};

use crate::sandbox_link::{resolve_path, SandboxLinkService, SANDBOX_EXEC_TOOL};

pub const SANDBOX_DEPLOY_TOOL: &str = "temps_deploy_sandbox_source";

/// Largest tarball taken out of a sandbox. It is held in memory once, so this
/// is the bound on the control plane's memory for one deploy.
const MAX_PACKED_BYTES: u64 = 200 * 1024 * 1024;
const MAX_SOURCE_FILES: usize = 100_000;
const MAX_SOURCE_BYTES: u64 = 500 * 1024 * 1024;
const PACK_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Deserialize)]
pub struct DeployArgs {
    pub project_id: i32,
    #[serde(default)]
    pub environment_id: Option<i32>,
    /// Directory in the sandbox that holds the project. Defaults to the
    /// sandbox work directory.
    #[serde(default)]
    pub directory: Option<String>,
}

#[derive(Error, Debug)]
pub enum SandboxDeployError {
    #[error(
        "`{SANDBOX_DEPLOY_TOOL}` needs {{\"project_id\": <number>}} and optionally \
         \"environment_id\" and \"directory\": {reason}. Nothing was deployed."
    )]
    Arguments { reason: String },

    #[error("Could not pack {directory} in sandbox {sandbox_id}: {reason}")]
    Pack {
        sandbox_id: String,
        directory: String,
        reason: String,
    },

    #[error(
        "{directory} in sandbox {sandbox_id} packs to {bytes} bytes, over the {max} byte limit. \
         Remove generated files (caches, build output, datasets) and try again"
    )]
    PackedTooLarge {
        sandbox_id: String,
        directory: String,
        bytes: u64,
        max: u64,
    },

    #[error("The source archive of sandbox {sandbox_id} has more than {max} files")]
    TooManyFiles { sandbox_id: String, max: usize },

    #[error("The source of sandbox {sandbox_id} is larger than {max} bytes uncompressed")]
    SourceTooLarge { sandbox_id: String, max: u64 },

    #[error(
        "Sandbox {sandbox_id} holds `{path}`, which cannot be deployed: {reason}. Remove or \
         replace it and try again"
    )]
    UnsafeEntry {
        sandbox_id: String,
        path: String,
        reason: String,
    },

    #[error(
        "{directory} in sandbox {sandbox_id} has no deployable files (node_modules, .git, \
         .env files, credentials and build output are never uploaded)"
    )]
    Empty {
        sandbox_id: String,
        directory: String,
    },

    #[error("Could not build the source archive of sandbox {sandbox_id}: {reason}")]
    Archive { sandbox_id: String, reason: String },
}

pub fn deploy_tool() -> ChatTool {
    ChatTool {
        name: SANDBOX_DEPLOY_TOOL.to_string(),
        description: "Deploy the files in the linked sandbox as the source of a Temps project, \
            with no Git repository: Temps builds them with the project's preset and serves the \
            result. Use it to publish an app you built in the sandbox. The server packs the \
            directory itself and leaves out node_modules, .git, .env files, credentials and \
            build output, so do not zip or upload anything yourself and do not commit the code \
            just to deploy it. The user is asked to approve the deployment. The project must \
            deploy from uploaded source (create it with `source_type` `uploaded_source`, or \
            `static_files` for a prebuilt static site) or have alternate sources enabled. \
            Returns the deployment; watch it with a goal on the deployment's state."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "integer", "description": "The Temps project to deploy to." },
                "environment_id": { "type": "integer", "description": "Target environment. Defaults to the project's production environment." },
                "directory": { "type": "string", "description": "Directory in the sandbox that holds the project (the one with package.json, index.html, a Dockerfile, …). Defaults to the sandbox work directory." }
            },
            "required": ["project_id"]
        }),
    }
}

/// Files packed from a sandbox, as the ZIP the uploaded-source pipeline takes.
pub struct PackedSource {
    pub archive: tempfile::NamedTempFile,
    pub files: usize,
    pub source_bytes: u64,
    /// Entries left out because they must not be uploaded.
    pub left_out: usize,
}

/// A deployment the chat runtime may now ask the user to approve.
pub struct PreparedDeploy {
    pub packed: PackedSource,
    pub args: DeployArgs,
    pub sandbox_id: String,
    pub directory: String,
}

impl PreparedDeploy {
    /// The sentence on the approval card.
    pub fn summary(&self) -> String {
        let target = match self.args.environment_id {
            Some(environment_id) => format!(" (environment {environment_id})"),
            None => String::new(),
        };
        format!(
            "Deploy {} files ({}) from the chat's sandbox to project {}{target}, without Git. \
             node_modules, .git, .env files and credentials are left out.",
            self.packed.files,
            human_bytes(self.packed.source_bytes),
            self.args.project_id
        )
    }
}

fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{} KiB", bytes.div_ceil(KIB).max(1))
    }
}

/// Single-quote `value` for `sh -c`.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The command that tars `directory` into `output` and prints the size of the
/// tarball. Excluding `node_modules` and `.git` here only keeps the transfer
/// small; the server filters every entry again.
fn pack_command(directory: &str, output: &str) -> String {
    let output = shell_quote(output);
    format!(
        "tar -czf {output} -C {} --exclude=node_modules --exclude=.git . && wc -c < {output}",
        shell_quote(directory)
    )
}

fn normalized_entry_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn is_left_out(path: &str) -> bool {
    path.split('/')
        .any(|part| part.eq_ignore_ascii_case("node_modules"))
        || crate::applications::is_sensitive_workspace_path(path)
}

/// Rebuild a gzip-compressed tarball as the ZIP the uploaded-source pipeline
/// takes, keeping only files that may be uploaded.
///
/// Nothing the tarball says is trusted: paths are rebuilt from normal
/// components only, links and special files are refused, and file counts and
/// sizes are bounded.
pub fn zip_from_tar_gz(
    sandbox_id: &str,
    directory: &str,
    tarball: &[u8],
) -> Result<PackedSource, SandboxDeployError> {
    let archive_error = |reason: String| SandboxDeployError::Archive {
        sandbox_id: sandbox_id.to_string(),
        reason,
    };
    let output = tempfile::NamedTempFile::new().map_err(|e| archive_error(e.to_string()))?;
    let mut writer = zip::ZipWriter::new(
        output
            .reopen()
            .map_err(|e| archive_error(format!("could not open the staging file: {e}")))?,
    );
    let mut files = 0_usize;
    let mut left_out = 0_usize;
    let mut source_bytes = 0_u64;

    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    let entries = archive
        .entries()
        .map_err(|e| archive_error(format!("the tarball is not readable: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| archive_error(format!("bad tarball entry: {e}")))?;
        let raw_path = entry
            .path()
            .map_err(|e| archive_error(format!("bad tarball path: {e}")))?
            .into_owned();
        let kind = entry.header().entry_type();
        // Directories carry no content; files recreate their parents.
        if kind.is_dir() {
            continue;
        }
        let Some(path) = normalized_entry_path(&raw_path) else {
            return Err(SandboxDeployError::UnsafeEntry {
                sandbox_id: sandbox_id.to_string(),
                path: raw_path.display().to_string(),
                reason: "its path leaves the project directory".to_string(),
            });
        };
        if is_left_out(&path) {
            left_out += 1;
            continue;
        }
        if !(kind.is_file() || kind.is_contiguous()) {
            return Err(SandboxDeployError::UnsafeEntry {
                sandbox_id: sandbox_id.to_string(),
                path,
                reason: "it is a link or a special file, and uploads do not follow links"
                    .to_string(),
            });
        }
        files += 1;
        if files > MAX_SOURCE_FILES {
            return Err(SandboxDeployError::TooManyFiles {
                sandbox_id: sandbox_id.to_string(),
                max: MAX_SOURCE_FILES,
            });
        }
        let executable = entry
            .header()
            .mode()
            .map(|m| m & 0o111 != 0)
            .unwrap_or(false);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(if executable { 0o755 } else { 0o644 });
        writer
            .start_file(path.clone(), options)
            .map_err(|e| archive_error(format!("could not add {path}: {e}")))?;
        let remaining = MAX_SOURCE_BYTES.saturating_sub(source_bytes);
        let copied = std::io::copy(&mut (&mut entry).take(remaining + 1), &mut writer)
            .map_err(|e| archive_error(format!("could not copy {path}: {e}")))?;
        if copied > remaining {
            return Err(SandboxDeployError::SourceTooLarge {
                sandbox_id: sandbox_id.to_string(),
                max: MAX_SOURCE_BYTES,
            });
        }
        source_bytes += copied;
    }
    if files == 0 {
        return Err(SandboxDeployError::Empty {
            sandbox_id: sandbox_id.to_string(),
            directory: directory.to_string(),
        });
    }
    writer
        .finish()
        .map_err(|e| archive_error(format!("could not finish the archive: {e}")))?
        .flush()
        .map_err(|e| archive_error(format!("could not flush the archive: {e}")))?;
    Ok(PackedSource {
        archive: output,
        files,
        source_bytes,
        left_out,
    })
}

/// Whether `auth` may deploy to `project_id` right now: the permission, and
/// live access to the project. Checked when the tool is called and again for
/// the user who approves.
pub async fn authorize_deploy(
    auth: &AuthContext,
    project_id: i32,
    chat_project_id: Option<i32>,
    checker: Option<&Arc<dyn temps_core::ProjectAccessChecker>>,
) -> Result<(), String> {
    if !auth.has_permission(&Permission::DeploymentsCreate) {
        return Err(format!(
            "`{SANDBOX_DEPLOY_TOOL}` is not available: deploying needs the {} permission.",
            Permission::DeploymentsCreate
        ));
    }
    if let Some(chat_project_id) = chat_project_id {
        if chat_project_id != project_id {
            return Err(format!(
                "This chat belongs to project {chat_project_id}; it can deploy only to that \
                 project, not to project {project_id}."
            ));
        }
    }
    if auth.is_admin() || auth.has_role(&temps_auth::permissions::Role::PlatformAdmin) {
        return Ok(());
    }
    let Some(checker) = checker else {
        // No team layer on this server: the instance permission decides.
        return Ok(());
    };
    let Some(user_id) = auth.user_id_opt() else {
        return Err(format!(
            "`{SANDBOX_DEPLOY_TOOL}` is not available: this credential is not tied to a user."
        ));
    };
    match checker.user_can_access_project(user_id, project_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "You do not have access to project {project_id}, so nothing was deployed to it."
        )),
        Err(error) => {
            warn!(project_id, user_id, "project access check failed: {error}");
            Err(format!(
                "Access to project {project_id} could not be verified, so nothing was deployed."
            ))
        }
    }
}

impl SandboxLinkService {
    /// Pack the linked sandbox's project directory for a deployment. Nothing is
    /// deployed and nothing outside the sandbox changes.
    pub async fn prepare_deploy(
        &self,
        arguments: &str,
        conversation_id: i64,
        auth: &AuthContext,
    ) -> Result<PreparedDeploy, String> {
        let args: DeployArgs = serde_json::from_str(arguments).map_err(|error| {
            SandboxDeployError::Arguments {
                reason: error.to_string(),
            }
            .to_string()
        })?;
        let user_id = auth.user_id();
        if !auth.has_permission(&Permission::SandboxesExec) {
            return Err(format!(
                "`{SANDBOX_DEPLOY_TOOL}` is not available: reading a sandbox's files needs the {} permission.",
                Permission::SandboxesExec
            ));
        }
        let ready = self
            .ready_sandbox(SANDBOX_DEPLOY_TOOL, conversation_id, user_id)
            .await?;
        let sandbox_id = ready.link.sandbox_id.clone();
        let directory = resolve_path(&ready.work_dir, args.directory.as_deref().unwrap_or("."));
        let directory = directory.trim_end_matches('/').to_string();
        let pack_error = |reason: String| {
            SandboxDeployError::Pack {
                sandbox_id: sandbox_id.clone(),
                directory: directory.clone(),
                reason,
            }
            .to_string()
        };

        let staging = format!("/tmp/temps-deploy-{}.tgz", uuid::Uuid::new_v4().simple());
        let options = temps_sandbox::services::ExecOptions {
            cmd: vec![
                "sh".to_string(),
                "-c".to_string(),
                pack_command(&directory, &staging),
            ],
            env: Default::default(),
            cwd: Some(ready.work_dir.clone()),
        };
        let packed_size = tokio::time::timeout(
            std::time::Duration::from_secs(PACK_TIMEOUT_SECS),
            self.sandboxes.exec(&sandbox_id, user_id, options),
        )
        .await;
        let outcome = match packed_size {
            Err(_) => Err(pack_error(format!(
                "`tar` did not finish within {PACK_TIMEOUT_SECS} seconds"
            ))),
            Ok(Err(error)) => Err(self
                .tool_failure(
                    &format!("Could not pack {directory}"),
                    error,
                    conversation_id,
                    &ready.link,
                )
                .await),
            Ok(Ok(result)) if result.exit_code != 0 => Err(pack_error(format!(
                "`tar` exited with {}: {}. Check the directory exists (look with `{SANDBOX_EXEC_TOOL}`)",
                result.exit_code,
                result.stderr.trim()
            ))),
            Ok(Ok(result)) => result.stdout.trim().parse::<u64>().map_err(|_| {
                pack_error(format!(
                    "could not read the tarball size from `{}`",
                    result.stdout.trim()
                ))
            }),
        };
        let bytes = match outcome {
            Ok(bytes) if bytes > MAX_PACKED_BYTES => {
                self.remove_staging(&sandbox_id, user_id, &staging).await;
                return Err(SandboxDeployError::PackedTooLarge {
                    sandbox_id,
                    directory,
                    bytes,
                    max: MAX_PACKED_BYTES,
                }
                .to_string());
            }
            Ok(bytes) => bytes,
            Err(message) => {
                self.remove_staging(&sandbox_id, user_id, &staging).await;
                return Err(message);
            }
        };

        let tarball = self.sandboxes.fs_read(&sandbox_id, user_id, &staging).await;
        self.remove_staging(&sandbox_id, user_id, &staging).await;
        let tarball = tarball.map_err(|error| {
            pack_error(format!(
                "could not read the {bytes} byte tarball back from the sandbox: {error}"
            ))
        })?;

        let (sandbox_for_task, directory_for_task) = (sandbox_id.clone(), directory.clone());
        let packed = tokio::task::spawn_blocking(move || {
            zip_from_tar_gz(&sandbox_for_task, &directory_for_task, &tarball)
        })
        .await
        .map_err(|error| pack_error(format!("the archive task failed: {error}")))?
        .map_err(|error| error.to_string())?;
        info!(
            sandbox_id = %sandbox_id,
            project_id = args.project_id,
            files = packed.files,
            left_out = packed.left_out,
            source_bytes = packed.source_bytes,
            "packed a chat sandbox's files for deployment"
        );
        Ok(PreparedDeploy {
            packed,
            args,
            sandbox_id,
            directory,
        })
    }

    async fn remove_staging(&self, sandbox_id: &str, user_id: i32, path: &str) {
        let options = temps_sandbox::services::ExecOptions {
            cmd: vec!["rm".to_string(), "-f".to_string(), path.to_string()],
            env: Default::default(),
            cwd: None,
        };
        if let Err(error) = self.sandboxes.exec(sandbox_id, user_id, options).await {
            warn!(
                sandbox_id,
                path, "could not remove the staged tarball: {error}"
            );
        }
    }

    /// Hand a prepared, approved archive to the uploaded-source pipeline and
    /// say what happened in a sentence the model can act on.
    pub async fn deploy_prepared(
        &self,
        prepared: PreparedDeploy,
        approver: &AuthContext,
        conversation_id: i64,
        chat_project_id: Option<i32>,
    ) -> String {
        let Some(deployer) = self.source_drop.as_ref() else {
            return "Deploying from a sandbox is not available: this server has no deployments \
                    plugin. Nothing was deployed."
                .to_string();
        };
        let project_id = prepared.args.project_id;
        let deployed = deployer
            .deploy_source_drop(SourceDropRequest {
                project_id,
                environment_id: prepared.args.environment_id,
                archive_path: prepared.packed.archive.path().to_path_buf(),
                original_filename: format!("sandbox-{}.zip", prepared.sandbox_id),
                promote_manual_source: true,
            })
            .await;
        match deployed {
            Ok(deployment) => {
                self.audit_deployed(
                    &prepared,
                    approver,
                    conversation_id,
                    chat_project_id,
                    deployment.id,
                    deployment.environment_id,
                )
                .await;
                json!({
                    "status": "started",
                    "deployment_id": deployment.id,
                    "project_id": deployment.project_id,
                    "environment_id": deployment.environment_id,
                    "state": deployment.state,
                    "files": prepared.packed.files,
                    "left_out": prepared.packed.left_out,
                    "instruction": "Temps is building the project from those files. Tell the user \
                        it started; check progress with the `temps` tool (the deployment's state) \
                        and watch it with a goal if it can fail later.",
                })
                .to_string()
            }
            Err(error) => {
                warn!(project_id, sandbox_id = %prepared.sandbox_id, "sandbox source deploy failed: {error}");
                let hint = match &error {
                    temps_core::SourceDropError::SourceNotAllowed { .. } => {
                        " Create the project with source_type `uploaded_source` (or \
                         `static_files`), or enable alternate sources on it."
                    }
                    _ => "",
                };
                format!("Nothing was deployed: {error}.{hint}")
            }
        }
    }

    async fn audit_deployed(
        &self,
        prepared: &PreparedDeploy,
        approver: &AuthContext,
        conversation_id: i64,
        chat_project_id: Option<i32>,
        deployment_id: i32,
        environment_id: i32,
    ) {
        let Some(audit) = self.audit.as_ref() else {
            return;
        };
        let entry = crate::audit::AiSandboxSourceDeployedAudit {
            context: AuditContext {
                user_id: approver.user_id(),
                ip_address: None,
                user_agent: "temps-ai-assistant".to_string(),
            },
            project_id: prepared.args.project_id,
            chat_project_id,
            conversation_id: conversation_id.to_string(),
            sandbox_id: prepared.sandbox_id.clone(),
            environment_id,
            deployment_id,
            files: prepared.packed.files,
            source_bytes: prepared.packed.source_bytes,
        };
        if let Err(error) = audit.create_audit_log(&entry).await {
            warn!(
                deployment_id,
                "failed to audit a sandbox source deployment: {error}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tarball(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        build(&mut builder);
        let bytes = builder.into_inner().expect("tar finishes");
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bytes).expect("gzip writes");
        encoder.finish().expect("gzip finishes")
    }

    fn add_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, body: &[u8], mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, path, body)
            .expect("file appended");
    }

    fn names(packed: &PackedSource) -> Vec<String> {
        let file = std::fs::File::open(packed.archive.path()).expect("archive opens");
        let mut zip = zip::ZipArchive::new(file).expect("valid zip");
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).expect("entry").name().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn source_files_are_packed_and_what_must_not_be_uploaded_is_left_out() {
        let data = tarball(|b| {
            add_file(b, "./package.json", b"{}", 0o644);
            add_file(b, "./src/index.js", b"console.log(1)", 0o644);
            add_file(b, "./start.sh", b"#!/bin/sh", 0o755);
            add_file(b, "./.env", b"SECRET=1", 0o600);
            add_file(b, "./.env.example", b"SECRET=", 0o644);
            add_file(b, "./.env.production", b"SECRET=2", 0o600);
            add_file(b, "./node_modules/left-pad/index.js", b"x", 0o644);
            add_file(b, "./web/Node_Modules/x.js", b"x", 0o644);
            add_file(b, "./.git/config", b"[core]", 0o644);
            add_file(b, "./.ssh/id_rsa", b"key", 0o600);
            add_file(b, "./dist/bundle.js", b"x", 0o644);
        });
        let packed = zip_from_tar_gz("sbx_t", "/work", &data).expect("packs");
        assert_eq!(
            names(&packed),
            vec![".env.example", "package.json", "src/index.js", "start.sh"]
        );
        assert_eq!(packed.files, 4);
        assert_eq!(packed.left_out, 7);
    }

    #[test]
    fn the_executable_bit_survives() {
        let data = tarball(|b| {
            add_file(b, "./run.sh", b"x", 0o755);
            add_file(b, "./notes.txt", b"x", 0o600);
        });
        let packed = zip_from_tar_gz("sbx_t", "/work", &data).expect("packs");
        let file = std::fs::File::open(packed.archive.path()).expect("opens");
        let mut zip = zip::ZipArchive::new(file).expect("zip");
        assert_eq!(
            zip.by_name("run.sh").expect("run").unix_mode(),
            Some(0o100755)
        );
        assert_eq!(
            zip.by_name("notes.txt").expect("notes").unix_mode(),
            Some(0o100644)
        );
    }

    #[test]
    fn a_symlink_outside_excluded_paths_is_refused_by_name() {
        let data = tarball(|b| {
            add_file(b, "./index.html", b"<html>", 0o644);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            b.append_link(&mut header, "./config", "/etc/passwd")
                .expect("link appended");
        });
        let error = zip_from_tar_gz("sbx_t", "/work", &data)
            .err()
            .expect("refused");
        assert!(
            matches!(&error, SandboxDeployError::UnsafeEntry { path, .. } if path == "config"),
            "{error}"
        );
        assert!(error.to_string().contains("sbx_t"));
    }

    #[test]
    fn a_symlink_inside_node_modules_is_ignored_with_the_rest_of_it() {
        let data = tarball(|b| {
            add_file(b, "./index.html", b"<html>", 0o644);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            b.append_link(&mut header, "./node_modules/.bin/tool", "../tool/cli.js")
                .expect("link appended");
        });
        let packed = zip_from_tar_gz("sbx_t", "/work", &data).expect("packs");
        assert_eq!(names(&packed), vec!["index.html"]);
    }

    #[test]
    fn a_path_that_leaves_the_directory_is_refused() {
        // `tar::Builder` refuses to write `..`, so write the name bytes
        // directly, as a hostile tarball would.
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        let name = b"../escape.txt";
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        let mut raw = Vec::new();
        raw.extend_from_slice(header.as_bytes());
        let mut block = [0_u8; 512];
        block[0] = b'x';
        raw.extend_from_slice(&block);
        raw.extend_from_slice(&[0_u8; 1024]);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&raw).expect("gzip writes");
        let data = encoder.finish().expect("gzip finishes");

        let error = zip_from_tar_gz("sbx_t", "/work", &data)
            .err()
            .expect("refused");
        assert!(
            matches!(error, SandboxDeployError::UnsafeEntry { .. }),
            "{error}"
        );
    }

    #[test]
    fn nothing_deployable_is_an_error_that_names_the_directory() {
        let data = tarball(|b| {
            add_file(b, "./.env", b"SECRET=1", 0o600);
            add_file(b, "./node_modules/a.js", b"x", 0o644);
        });
        let error = zip_from_tar_gz("sbx_t", "/work/app", &data)
            .err()
            .expect("empty");
        assert!(matches!(error, SandboxDeployError::Empty { .. }));
        assert!(error.to_string().contains("/work/app"));
    }

    #[test]
    fn something_that_is_not_a_tarball_is_reported_not_panicked_on() {
        let error = zip_from_tar_gz("sbx_t", "/work", b"definitely not gzip")
            .err()
            .expect("rejected");
        assert!(
            matches!(error, SandboxDeployError::Archive { .. }),
            "{error}"
        );
    }

    #[test]
    fn the_pack_command_quotes_hostile_directories() {
        let command = pack_command("/work/it's a dir; rm -rf /", "/tmp/out.tgz");
        assert!(command.contains(r"'/work/it'\''s a dir; rm -rf /'"));
        assert!(command.starts_with("tar -czf '/tmp/out.tgz' -C "));
        assert!(command.ends_with("wc -c < '/tmp/out.tgz'"));
    }

    #[test]
    fn the_approval_sentence_says_what_will_be_uploaded_and_where() {
        let data = tarball(|b| add_file(b, "./index.html", &[b'x'; 3000], 0o644));
        let packed = zip_from_tar_gz("sbx_t", "/work", &data).expect("packs");
        let prepared = PreparedDeploy {
            packed,
            args: DeployArgs {
                project_id: 7,
                environment_id: Some(3),
                directory: None,
            },
            sandbox_id: "sbx_t".to_string(),
            directory: "/work".to_string(),
        };
        let summary = prepared.summary();
        assert!(summary.contains("1 files"), "{summary}");
        assert!(summary.contains("project 7"));
        assert!(summary.contains("environment 3"));
        assert!(summary.contains("without Git"));
    }

    #[test]
    fn arguments_without_a_project_are_explained() {
        let error = serde_json::from_str::<DeployArgs>("{}")
            .map_err(|e| {
                SandboxDeployError::Arguments {
                    reason: e.to_string(),
                }
                .to_string()
            })
            .err()
            .expect("missing project");
        assert!(error.contains("project_id"), "{error}");
        assert!(error.contains("Nothing was deployed"));
    }

    #[test]
    fn the_tool_requires_a_project_and_is_a_sandbox_tool() {
        let tool = deploy_tool();
        assert_eq!(tool.name, SANDBOX_DEPLOY_TOOL);
        assert_eq!(tool.parameters["required"], json!(["project_id"]));
        assert!(crate::sandbox_link::is_sandbox_tool(SANDBOX_DEPLOY_TOOL));
    }
}
