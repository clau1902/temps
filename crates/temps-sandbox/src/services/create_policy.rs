// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Operator limits on `POST /v1/sandboxes`.
//!
//! The handler rejects values that can never be valid (non-positive, over the
//! built-in ceilings). What an *operator* allows lives in the `agent_sandbox`
//! settings (`max_cpu_limit`, `max_memory_limit_mb`, `max_pids_limit`,
//! `max_sandboxes_per_user`) and in the memory of the host the sandbox lands
//! on, so it is checked here, by the service, with the node already chosen.
//!
//! Only the HTTP create path asks for these checks
//! (`CreateSandboxRequest::enforce_operator_limits`): AI application
//! workspaces and agent runs are created by the platform itself with limits
//! the platform chose.

use std::sync::OnceLock;

use temps_core::AgentSandboxSettings;

use crate::error::SandboxError;
use crate::services::sandbox_service::CreateSandboxRequest;

/// Memory (MB) of the machine Temps runs on: the smaller of its physical
/// memory and the cgroup limit it runs under, if any. `None` when the OS
/// reports none. Read once: neither changes while the process runs.
pub fn control_plane_memory_mb() -> Option<u64> {
    static MEMORY_MB: OnceLock<Option<u64>> = OnceLock::new();
    *MEMORY_MB.get_or_init(|| {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let physical = system.total_memory();
        let limit = system
            .cgroup_limits()
            .map(|cgroup| cgroup.total_memory)
            .filter(|bytes| *bytes > 0)
            .map_or(physical, |cgroup| cgroup.min(physical));
        (limit > 0).then_some(limit / (1024 * 1024))
    })
}

/// Refuse a request that asks for more than the operator allows.
///
/// `host_memory_mb` is the memory of the machine the sandbox will run on,
/// when it is known (the control plane; a worker reports no figure).
pub fn check_requested_limits(
    req: &CreateSandboxRequest,
    settings: &AgentSandboxSettings,
    host_memory_mb: Option<u64>,
) -> Result<(), SandboxError> {
    if let (Some(cpu), Some(max)) = (req.cpu_limit, settings.max_cpu_limit) {
        if cpu > max {
            return Err(SandboxError::Validation {
                message: format!(
                    "cpu_limit/vcpus {cpu} exceeds the maximum of {max} cores an administrator \
                     allows for a sandbox (agent_sandbox.max_cpu_limit)"
                ),
            });
        }
    }
    if let Some(memory) = req.memory_limit_mb {
        if let Some(max) = settings.max_memory_limit_mb {
            if memory > max {
                return Err(SandboxError::Validation {
                    message: format!(
                        "memory_limit_mb/memory {memory} exceeds the maximum of {max} MB an \
                         administrator allows for a sandbox (agent_sandbox.max_memory_limit_mb)"
                    ),
                });
            }
        }
        if let Some(host) = host_memory_mb {
            if memory > host {
                return Err(SandboxError::Validation {
                    message: format!(
                        "memory_limit_mb/memory {memory} exceeds the {host} MB of memory on the \
                         host this sandbox would run on"
                    ),
                });
            }
        }
    }
    if let (Some(pids), Some(max)) = (req.pids_limit, settings.max_pids_limit) {
        if pids > max {
            return Err(SandboxError::Validation {
                message: format!(
                    "pids_limit {pids} exceeds the maximum of {max} an administrator allows for \
                     a sandbox (agent_sandbox.max_pids_limit)"
                ),
            });
        }
    }
    Ok(())
}

/// Refuse a create that would take `user_id` past `max_sandboxes_per_user`.
/// `live` counts the sandboxes the user already has that are not destroyed.
pub fn check_user_sandbox_cap(
    user_id: i32,
    live: u64,
    settings: &AgentSandboxSettings,
) -> Result<(), SandboxError> {
    match settings.max_sandboxes_per_user {
        Some(limit) if live >= u64::from(limit) => Err(SandboxError::UserSandboxLimitReached {
            user_id,
            limit,
            live,
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(cpu: Option<f64>, memory: Option<u64>, pids: Option<i64>) -> CreateSandboxRequest {
        CreateSandboxRequest {
            cpu_limit: cpu,
            memory_limit_mb: memory,
            pids_limit: pids,
            ..Default::default()
        }
    }

    fn settings() -> AgentSandboxSettings {
        AgentSandboxSettings {
            max_cpu_limit: Some(2.0),
            max_memory_limit_mb: Some(4096),
            max_pids_limit: Some(256),
            max_sandboxes_per_user: Some(3),
            ..Default::default()
        }
    }

    fn message(err: SandboxError) -> String {
        match err {
            SandboxError::Validation { message } => message,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    #[test]
    fn requests_within_the_operator_maximums_pass() {
        let req = request(Some(2.0), Some(4096), Some(256));
        assert!(check_requested_limits(&req, &settings(), Some(8192)).is_ok());
        assert!(check_requested_limits(&request(None, None, None), &settings(), None).is_ok());
    }

    #[test]
    fn unset_maximums_and_unknown_host_memory_add_no_limit() {
        let req = request(Some(16.0), Some(32_768), Some(4096));
        assert!(check_requested_limits(&req, &AgentSandboxSettings::default(), None).is_ok());
    }

    #[test]
    fn a_request_over_an_operator_maximum_names_the_setting() {
        let cpu =
            check_requested_limits(&request(Some(2.5), None, None), &settings(), None).unwrap_err();
        assert!(message(cpu).contains("max_cpu_limit"));
        let memory = check_requested_limits(&request(None, Some(4097), None), &settings(), None)
            .unwrap_err();
        assert!(message(memory).contains("max_memory_limit_mb"));
        let pids =
            check_requested_limits(&request(None, None, Some(257)), &settings(), None).unwrap_err();
        assert!(message(pids).contains("max_pids_limit"));
    }

    #[test]
    fn memory_above_the_host_is_refused_even_without_an_operator_maximum() {
        let err = check_requested_limits(
            &request(None, Some(16_384), None),
            &AgentSandboxSettings::default(),
            Some(2048),
        )
        .unwrap_err();
        let message = message(err);
        assert!(
            message.contains("16384") && message.contains("2048"),
            "{message}"
        );
    }

    #[test]
    fn the_user_cap_refuses_at_the_limit_and_not_below_it() {
        assert!(check_user_sandbox_cap(7, 2, &settings()).is_ok());
        let err = check_user_sandbox_cap(7, 3, &settings()).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::UserSandboxLimitReached {
                user_id: 7,
                limit: 3,
                live: 3
            }
        ));
        assert!(check_user_sandbox_cap(7, 1_000, &AgentSandboxSettings::default()).is_ok());
    }

    #[test]
    fn this_machine_reports_some_memory() {
        let memory = control_plane_memory_mb().expect("sysinfo reports memory");
        assert!(memory > 0);
    }
}
