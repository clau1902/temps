// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Credentials a sandbox can use against a linked MariaDB service (ADR-051).
//!
//! The sandbox gets a database of its own (`<namespace>_sandbox`) and a user
//! that can use that database and nothing else, with a password derived from
//! the administrator's so nothing new is stored. The variables use the names a
//! deployment gets (`MYSQL_*`, `MARIADB_*`, `DATABASE_URL`).

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use sqlx::ConnectOptions;
use temps_entities::external_services;
use tracing::info;

use crate::externalsvc::mariadb::{mariadb_container_name, MariaDbService};
use crate::externalsvc::scoped_resource_name;
use crate::sandbox_access::sandbox_role_name;
use crate::sandbox_engines::{
    derive_password, is_plain_identifier, require_local, text_parameter, SandboxAccessEngine,
    SandboxAccessRequest,
};
use crate::services::{ExternalServiceError, ExternalServiceManager};

const MARIADB_INTERNAL_PORT: &str = "3306";
const DATABASE_MAX: usize = 63;
const PASSWORD_TAG: &str = "temps-scoped-mariadb-v1";
/// Connections the user may hold at once, so a runaway loop in a sandbox
/// cannot exhaust the server's connection slots.
const USER_CONNECTION_LIMIT: u32 = 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Gives each sandbox its own database and a user limited to it.
pub struct MariaDbSandboxAccess;

/// The database, user and statements that give one sandbox its access.
#[derive(Clone, PartialEq, Eq)]
pub struct MariaDbSandboxPlan {
    pub database: String,
    pub user: String,
    password: String,
}

impl std::fmt::Debug for MariaDbSandboxPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MariaDbSandboxPlan")
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl MariaDbSandboxPlan {
    pub fn new(service_id: i32, namespace: &str, root_password: &str) -> Result<Self, String> {
        let database =
            MariaDbService::normalize_database_name(&scoped_resource_name(namespace, "sandbox"));
        let user = sandbox_role_name(&database);
        for (kind, value) in [("database", &database), ("user", &user)] {
            if !is_plain_identifier(value, DATABASE_MAX) {
                return Err(format!(
                    "the {kind} name '{value}' is not a plain lowercase identifier"
                ));
            }
        }
        if root_password.is_empty() {
            return Err("the service has no root password to provision a user with".to_string());
        }
        Ok(Self {
            password: derive_password(root_password, PASSWORD_TAG, service_id, &user)?,
            database,
            user,
        })
    }

    /// Idempotent statements, run as the administrator on every connect so a
    /// user or grant changed by hand is put back. Identifiers are plain
    /// `[a-z0-9_]` and the password is hex, so nothing needs quoting beyond
    /// the delimiters here. The database name's underscores are escaped in
    /// the grant, where `_` would otherwise match any character and reach
    /// other databases.
    pub fn statements(&self) -> Vec<String> {
        let Self {
            database,
            user,
            password,
        } = self;
        let escaped = database.replace('_', "\\_");
        vec![
            format!("CREATE DATABASE IF NOT EXISTS `{database}`"),
            format!("CREATE USER IF NOT EXISTS '{user}'@'%' IDENTIFIED BY '{password}'"),
            format!(
                "ALTER USER '{user}'@'%' IDENTIFIED BY '{password}' \
                 WITH MAX_USER_CONNECTIONS {USER_CONNECTION_LIMIT}"
            ),
            format!("REVOKE ALL PRIVILEGES, GRANT OPTION FROM '{user}'@'%'"),
            format!("GRANT ALL PRIVILEGES ON `{escaped}`.* TO '{user}'@'%'"),
        ]
    }

    pub fn environment(&self, host: &str) -> Result<HashMap<String, String>, String> {
        MariaDbService::build_env_vars(
            host,
            MARIADB_INTERNAL_PORT,
            &self.database,
            &self.user,
            &self.password,
        )
        .map_err(|error| error.to_string())
    }
}

fn failure(service_id: i32, what: &str, reason: impl std::fmt::Display) -> ExternalServiceError {
    ExternalServiceError::InternalError {
        reason: format!("Failed to {what} on MariaDB service {service_id}: {reason}"),
    }
}

#[async_trait]
impl SandboxAccessEngine for MariaDbSandboxAccess {
    fn service_types(&self) -> &'static [&'static str] {
        &["mariadb"]
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
            .unwrap_or_else(|| mariadb_container_name(&service.name))])
    }

    async fn issue(
        &self,
        _manager: &ExternalServiceManager,
        request: SandboxAccessRequest<'_>,
    ) -> Result<HashMap<String, String>, ExternalServiceError> {
        let SandboxAccessRequest {
            service,
            namespace,
            project_id,
            parameters,
        } = request;
        require_local(service)?;
        let root_password = text_parameter(parameters, "root_password").unwrap_or_default();
        let plan =
            MariaDbSandboxPlan::new(service.id, namespace, &root_password).map_err(|reason| {
                ExternalServiceError::SandboxAccessUnsupported {
                    service_id: service.id,
                    service_type: service.service_type.clone(),
                    reason,
                }
            })?;
        let container = text_parameter(parameters, "container_name")
            .unwrap_or_else(|| mariadb_container_name(&service.name));
        let host = text_parameter(parameters, "host").unwrap_or_else(|| "localhost".to_string());
        let port: u16 = text_parameter(parameters, "port")
            .and_then(|port| port.parse().ok())
            .unwrap_or(3306);

        let options = MySqlConnectOptions::new()
            .host(&host)
            .port(port)
            .username("root")
            .password(&root_password)
            .disable_statement_logging();
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect_with(options)
            .await
            .map_err(|error| failure(service.id, &format!("connect to {host}:{port}"), error))?;
        for statement in plan.statements() {
            sqlx::raw_sql(&statement)
                .execute(&pool)
                .await
                .map_err(|error| {
                    failure(
                        service.id,
                        &format!("provision the sandbox database '{}'", plan.database),
                        error,
                    )
                })?;
        }
        pool.close().await;

        info!(
            service_id = service.id,
            project_id = ?project_id,
            database = %plan.database,
            user = %plan.user,
            "Provisioned a database and user for a sandbox"
        );
        plan.environment(&container)
            .map_err(|reason| failure(service.id, "build the sandbox variables", reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> MariaDbSandboxPlan {
        MariaDbSandboxPlan::new(4, "shop", "rootpassword").expect("plan")
    }

    #[test]
    fn the_database_and_user_are_named_for_the_namespace() {
        let plan = plan();
        assert_eq!(plan.database, "shop_sandbox");
        assert_eq!(plan.user, "sbx_shop_sandbox");
    }

    #[test]
    fn the_grant_is_limited_to_one_database_with_underscores_escaped() {
        let statements = plan().statements();
        let grant = statements
            .iter()
            .find(|statement| statement.starts_with("GRANT"))
            .expect("a grant");
        assert_eq!(
            grant,
            "GRANT ALL PRIVILEGES ON `shop\\_sandbox`.* TO 'sbx_shop_sandbox'@'%'"
        );
        assert!(
            !statements.iter().any(|s| s.contains("*.*")),
            "no global grant"
        );
    }

    #[test]
    fn privileges_are_revoked_before_they_are_granted_so_drift_is_repaired() {
        let statements = plan().statements();
        let position = |prefix: &str| {
            statements
                .iter()
                .position(|statement| statement.starts_with(prefix))
                .expect(prefix)
        };
        assert!(position("REVOKE") < position("GRANT"));
        assert!(position("CREATE USER") < position("ALTER USER"));
        assert!(statements[2].contains("MAX_USER_CONNECTIONS 20"));
    }

    #[test]
    fn the_password_is_stable_and_follows_the_root_password() {
        assert_eq!(plan(), plan());
        let other = MariaDbSandboxPlan::new(4, "shop", "another-root").expect("plan");
        assert_ne!(plan().password, other.password);
        let elsewhere = MariaDbSandboxPlan::new(5, "shop", "rootpassword").expect("plan");
        assert_ne!(plan().password, elsewhere.password);
    }

    #[test]
    fn a_service_without_a_root_password_cannot_be_provisioned() {
        let error = MariaDbSandboxPlan::new(4, "shop", "").expect_err("no password");
        assert!(error.contains("root password"), "{error}");
    }

    #[test]
    fn a_namespace_that_could_break_out_of_an_identifier_is_normalised_or_refused() {
        // Anything but [a-z0-9_] is replaced by the shared normaliser, so the
        // statement can never contain a quote or backtick from a name.
        let plan = MariaDbSandboxPlan::new(4, "sh`op'; drop", "rootpassword").expect("plan");
        assert!(
            is_plain_identifier(&plan.database, DATABASE_MAX),
            "{plan:?}"
        );
        assert!(is_plain_identifier(&plan.user, DATABASE_MAX), "{plan:?}");
        for statement in plan.statements() {
            assert!(!statement.contains(';'), "{statement}");
            assert_eq!(statement.matches('`').count() % 2, 0, "{statement}");
        }
    }

    #[test]
    fn the_environment_keeps_the_deployment_names() {
        let environment = plan().environment("maria-shop").expect("environment");
        for name in [
            "DATABASE_URL",
            "MYSQL_URL",
            "MYSQL_HOST",
            "MYSQL_PORT",
            "MYSQL_DATABASE",
            "MYSQL_USER",
            "MYSQL_PASSWORD",
            "MARIADB_URL",
        ] {
            assert!(environment.contains_key(name), "{name}");
        }
        assert_eq!(environment["MYSQL_HOST"], "maria-shop");
        assert_eq!(environment["MYSQL_PORT"], "3306");
        assert_eq!(environment["MYSQL_USER"], "sbx_shop_sandbox");
        assert!(!environment
            .values()
            .any(|value| value.contains("rootpassword")));
    }

    #[test]
    fn debug_output_hides_the_password() {
        let plan = plan();
        assert!(!format!("{plan:?}").contains(&plan.password));
    }
}
