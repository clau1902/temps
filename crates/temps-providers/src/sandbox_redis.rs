// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Credentials a sandbox can use against a linked Redis service (ADR-051).
//!
//! Redis has no per-sandbox database, so the sandbox gets an ACL user that may
//! touch keys under its own prefix and nothing else: no other tenant's keys,
//! no `FLUSHALL`, `CONFIG` or `ACL`. Code written in the sandbox must prefix
//! its keys with `REDIS_KEY_PREFIX` (most clients have a `keyPrefix` option);
//! the rest of the contract matches a deployment's.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use temps_entities::external_services;
use tracing::info;

use crate::externalsvc::redis::redis_container_name;
use crate::sandbox_engines::{
    derive_password, require_local, text_parameter, SandboxAccessEngine, SandboxAccessRequest,
};
use crate::services::{ExternalServiceError, ExternalServiceManager};

const REDIS_INTERNAL_PORT: &str = "6379";
const USER_PREFIX: &str = "sbx_";
const PASSWORD_TAG: &str = "temps-scoped-redis-v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Gives each sandbox an ACL user limited to a key prefix.
pub struct RedisSandboxAccess;

/// What the ACL grants one sandbox.
#[derive(Clone, PartialEq, Eq)]
pub struct RedisSandboxPlan {
    pub user: String,
    /// Every key the user may touch starts with this.
    pub key_prefix: String,
    password: String,
}

impl std::fmt::Debug for RedisSandboxPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedisSandboxPlan")
            .field("user", &self.user)
            .field("key_prefix", &self.key_prefix)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl RedisSandboxPlan {
    pub fn new(service_id: i32, namespace: &str, admin_password: &str) -> Result<Self, String> {
        if namespace.is_empty()
            || namespace.len() > 48
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(format!(
                "the namespace '{namespace}' is not a plain lowercase slug ([a-z0-9_], at most 48 \
                 characters)"
            ));
        }
        if admin_password.is_empty() {
            return Err(
                "the service has no password, so every client is the default user and a sandbox \
                 cannot be limited to its own keys; set a password on the service first"
                    .to_string(),
            );
        }
        let user = format!("{USER_PREFIX}{namespace}");
        Ok(Self {
            password: derive_password(admin_password, PASSWORD_TAG, service_id, &user)?,
            key_prefix: format!("sbx:{namespace}:"),
            user,
        })
    }

    /// The arguments of `ACL SETUSER`. `reset` first, so a user changed by hand
    /// is put back exactly as granted, and the whole replacement is atomic.
    pub fn acl_arguments(&self) -> Vec<String> {
        vec![
            self.user.clone(),
            "reset".to_string(),
            "on".to_string(),
            format!(">{}", self.password),
            format!("~{}*", self.key_prefix),
            format!("&{}*", self.key_prefix),
            "+@all".to_string(),
            "-@admin".to_string(),
            "-@dangerous".to_string(),
        ]
    }

    /// Variables with the names a deployment gets, plus the prefix to use.
    pub fn environment(&self, host: &str) -> HashMap<String, String> {
        let url = format!(
            "redis://{}:{}@{}:{}/0",
            urlencoding::encode(&self.user),
            urlencoding::encode(&self.password),
            host,
            REDIS_INTERNAL_PORT
        );
        HashMap::from([
            ("REDIS_URL".to_string(), url),
            ("REDIS_HOST".to_string(), host.to_string()),
            ("REDIS_PORT".to_string(), REDIS_INTERNAL_PORT.to_string()),
            ("REDIS_DATABASE".to_string(), "0".to_string()),
            ("REDIS_USER".to_string(), self.user.clone()),
            ("REDIS_PASSWORD".to_string(), self.password.clone()),
            ("REDIS_KEY_PREFIX".to_string(), self.key_prefix.clone()),
        ])
    }
}

fn failure(service_id: i32, what: &str, reason: impl std::fmt::Display) -> ExternalServiceError {
    ExternalServiceError::InternalError {
        reason: format!("Failed to {what} on Redis service {service_id}: {reason}"),
    }
}

#[async_trait]
impl SandboxAccessEngine for RedisSandboxAccess {
    fn service_types(&self) -> &'static [&'static str] {
        &["redis"]
    }

    async fn containers(
        &self,
        manager: &ExternalServiceManager,
        service: &external_services::Model,
    ) -> Result<Vec<String>, ExternalServiceError> {
        if service.node_id.is_some() {
            return Ok(Vec::new());
        }
        let parameters = manager.get_service_parameters(service.id).await?;
        Ok(vec![text_parameter(&parameters, "container_name")
            .unwrap_or_else(|| redis_container_name(&service.name))])
    }

    async fn issue(
        &self,
        manager: &ExternalServiceManager,
        request: SandboxAccessRequest<'_>,
    ) -> Result<HashMap<String, String>, ExternalServiceError> {
        let SandboxAccessRequest {
            service,
            namespace,
            project_id,
            parameters,
        } = request;
        require_local(service)?;
        let _ = manager;
        let admin_password = text_parameter(parameters, "password").unwrap_or_default();
        let plan =
            RedisSandboxPlan::new(service.id, namespace, &admin_password).map_err(|reason| {
                ExternalServiceError::SandboxAccessUnsupported {
                    service_id: service.id,
                    service_type: service.service_type.clone(),
                    reason,
                }
            })?;
        let container = text_parameter(parameters, "container_name")
            .unwrap_or_else(|| redis_container_name(&service.name));
        let host = text_parameter(parameters, "host").unwrap_or_else(|| "localhost".to_string());
        let port =
            text_parameter(parameters, "port").unwrap_or_else(|| REDIS_INTERNAL_PORT.to_string());
        let url = format!(
            "redis://:{}@{}:{}",
            urlencoding::encode(&admin_password),
            host,
            port
        );

        let client = redis::Client::open(url.as_str())
            .map_err(|error| failure(service.id, "open a connection", error))?;
        let mut connection =
            tokio::time::timeout(CONNECT_TIMEOUT, client.get_multiplexed_async_connection())
                .await
                .map_err(|_| {
                    failure(
                        service.id,
                        &format!("connect to {host}:{port}"),
                        format!("timed out after {}s", CONNECT_TIMEOUT.as_secs()),
                    )
                })?
                .map_err(|error| {
                    failure(service.id, &format!("connect to {host}:{port}"), error)
                })?;
        let mut command = redis::cmd("ACL");
        command.arg("SETUSER");
        for argument in plan.acl_arguments() {
            command.arg(argument);
        }
        tokio::time::timeout(COMMAND_TIMEOUT, command.query_async::<()>(&mut connection))
            .await
            .map_err(|_| {
                failure(
                    service.id,
                    &format!("create the ACL user '{}'", plan.user),
                    format!("timed out after {}s", COMMAND_TIMEOUT.as_secs()),
                )
            })?
            .map_err(|error| {
                failure(
                    service.id,
                    &format!("create the ACL user '{}'", plan.user),
                    error,
                )
            })?;

        info!(
            service_id = service.id,
            project_id = ?project_id,
            user = %plan.user,
            key_prefix = %plan.key_prefix,
            "Provisioned a key-prefix ACL user for a sandbox"
        );
        Ok(plan.environment(&container))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> RedisSandboxPlan {
        RedisSandboxPlan::new(3, "shop", "adminpass").expect("plan")
    }

    #[test]
    fn the_user_and_prefix_are_named_for_the_namespace() {
        let plan = plan();
        assert_eq!(plan.user, "sbx_shop");
        assert_eq!(plan.key_prefix, "sbx:shop:");
    }

    #[test]
    fn the_acl_resets_the_user_and_limits_keys_and_channels_to_the_prefix() {
        let arguments = plan().acl_arguments();
        assert_eq!(arguments[0], "sbx_shop");
        assert_eq!(arguments[1], "reset", "reset first so drift is repaired");
        assert!(arguments.contains(&"on".to_string()));
        assert!(arguments.contains(&"~sbx:shop:*".to_string()));
        assert!(arguments.contains(&"&sbx:shop:*".to_string()));
        assert!(arguments.contains(&"-@admin".to_string()));
        assert!(arguments.contains(&"-@dangerous".to_string()));
        // No pattern grants every key.
        assert!(!arguments.iter().any(|a| a == "~*" || a == "allkeys"));
    }

    #[test]
    fn the_password_is_stable_per_user_and_changes_with_the_admin_password() {
        let first = plan();
        assert_eq!(first, plan());
        let other_admin = RedisSandboxPlan::new(3, "shop", "different").expect("plan");
        let other_service = RedisSandboxPlan::new(4, "shop", "adminpass").expect("plan");
        let other_namespace = RedisSandboxPlan::new(3, "cafe", "adminpass").expect("plan");
        let password = |p: &RedisSandboxPlan| p.acl_arguments()[3].clone();
        assert_ne!(password(&first), password(&other_admin));
        assert_ne!(password(&first), password(&other_service));
        assert_ne!(password(&first), password(&other_namespace));
    }

    #[test]
    fn a_service_without_a_password_cannot_be_scoped() {
        let error = RedisSandboxPlan::new(3, "shop", "").expect_err("no password");
        assert!(error.contains("no password"), "{error}");
    }

    #[test]
    fn a_namespace_that_could_widen_the_pattern_is_refused() {
        for bad in ["", "Shop", "a*b", "a b", "a:b", "x]", &"a".repeat(49)] {
            assert!(
                RedisSandboxPlan::new(3, bad, "adminpass").is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_environment_keeps_the_deployment_names_and_hides_the_admin_password() {
        let environment = plan().environment("redis-shop");
        for name in [
            "REDIS_URL",
            "REDIS_HOST",
            "REDIS_PORT",
            "REDIS_DATABASE",
            "REDIS_PASSWORD",
        ] {
            assert!(environment.contains_key(name), "{name}");
        }
        assert_eq!(environment["REDIS_HOST"], "redis-shop");
        assert_eq!(environment["REDIS_PORT"], "6379");
        assert_eq!(environment["REDIS_KEY_PREFIX"], "sbx:shop:");
        assert!(environment["REDIS_URL"].starts_with("redis://sbx_shop:"));
        assert!(!environment
            .values()
            .any(|value| value.contains("adminpass")));
    }

    #[test]
    fn debug_output_hides_the_password() {
        let plan = plan();
        let shown = format!("{plan:?}");
        assert!(!shown.contains(&plan.acl_arguments()[3][1..]));
    }
}
