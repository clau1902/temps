// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Credentials a sandbox can use against a linked MongoDB service (ADR-051).
//!
//! The sandbox gets a database of its own (`<namespace>_sandbox`) and a user,
//! defined in that database, with `readWrite` on it and no other role. The
//! password is derived from the administrator's so nothing new is stored. The
//! variables use the names a deployment gets (`MONGODB_*`).

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use mongodb::bson::{doc, Document};
use mongodb::error::{CommandError, ErrorKind};
use mongodb::options::ClientOptions;
use mongodb::Client;
use temps_entities::external_services;
use tracing::info;

use crate::externalsvc::mongodb::mongodb_container_name;
use crate::sandbox_access::sandbox_role_name;
use crate::sandbox_engines::{
    derive_password, is_plain_identifier, require_local, text_parameter, SandboxAccessEngine,
    SandboxAccessRequest,
};
use crate::services::{ExternalServiceError, ExternalServiceManager};

const MONGODB_INTERNAL_PORT: &str = "27017";
const DATABASE_MAX: usize = 63;
const PASSWORD_TAG: &str = "temps-scoped-mongodb-v1";
const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
/// `createUser` answers this when the user exists already.
const USER_ALREADY_EXISTS: i32 = 51003;

/// Gives each sandbox its own database and a user limited to it.
pub struct MongoDbSandboxAccess;

/// The database and user that give one sandbox its access.
#[derive(Clone, PartialEq, Eq)]
pub struct MongoDbSandboxPlan {
    pub database: String,
    pub user: String,
    password: String,
}

impl std::fmt::Debug for MongoDbSandboxPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MongoDbSandboxPlan")
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

fn database_name(namespace: &str) -> String {
    let mut name: String = format!("{namespace}_sandbox")
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect();
    if name.starts_with(|character: char| character.is_ascii_digit()) {
        name.insert_str(0, "db_");
    }
    name.truncate(DATABASE_MAX);
    name
}

impl MongoDbSandboxPlan {
    pub fn new(service_id: i32, namespace: &str, admin_password: &str) -> Result<Self, String> {
        let database = database_name(namespace);
        let user = sandbox_role_name(&database);
        for (kind, value) in [("database", &database), ("user", &user)] {
            if !is_plain_identifier(value, DATABASE_MAX) {
                return Err(format!(
                    "the {kind} name '{value}' is not a plain lowercase identifier"
                ));
            }
        }
        if admin_password.is_empty() {
            return Err("the service has no administrator password".to_string());
        }
        Ok(Self {
            password: derive_password(admin_password, PASSWORD_TAG, service_id, &user)?,
            database,
            user,
        })
    }

    /// `createUser` in the sandbox's own database: `readWrite` there and
    /// nothing else, so the user can neither see another database nor manage
    /// users.
    pub fn create_user(&self) -> Document {
        doc! {
            "createUser": &self.user,
            "pwd": &self.password,
            "roles": [{ "role": "readWrite", "db": &self.database }],
        }
    }

    /// The same, for a user that exists already: sets the password and roles
    /// back, repairing a user changed by hand.
    pub fn update_user(&self) -> Document {
        doc! {
            "updateUser": &self.user,
            "pwd": &self.password,
            "roles": [{ "role": "readWrite", "db": &self.database }],
        }
    }

    pub fn environment(&self, host: &str, direct_connection: bool) -> HashMap<String, String> {
        let mut url = format!(
            "mongodb://{}:{}@{}:{}/{}?authSource={}",
            urlencoding::encode(&self.user),
            urlencoding::encode(&self.password),
            host,
            MONGODB_INTERNAL_PORT,
            self.database,
            self.database
        );
        if direct_connection {
            url.push_str("&directConnection=true");
        }
        HashMap::from([
            ("MONGODB_URL".to_string(), url),
            ("MONGODB_HOST".to_string(), host.to_string()),
            (
                "MONGODB_PORT".to_string(),
                MONGODB_INTERNAL_PORT.to_string(),
            ),
            ("MONGODB_DATABASE".to_string(), self.database.clone()),
            ("MONGODB_USERNAME".to_string(), self.user.clone()),
            ("MONGODB_PASSWORD".to_string(), self.password.clone()),
        ])
    }
}

fn failure(service_id: i32, what: &str, reason: impl std::fmt::Display) -> ExternalServiceError {
    ExternalServiceError::InternalError {
        reason: format!("Failed to {what} on MongoDB service {service_id}: {reason}"),
    }
}

#[async_trait]
impl SandboxAccessEngine for MongoDbSandboxAccess {
    fn service_types(&self) -> &'static [&'static str] {
        &["mongodb"]
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
            .unwrap_or_else(|| mongodb_container_name(&service.name))])
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
        let admin_user = text_parameter(parameters, "username").unwrap_or_else(|| "root".into());
        let admin_password = text_parameter(parameters, "password").unwrap_or_default();
        let plan =
            MongoDbSandboxPlan::new(service.id, namespace, &admin_password).map_err(|reason| {
                ExternalServiceError::SandboxAccessUnsupported {
                    service_id: service.id,
                    service_type: service.service_type.clone(),
                    reason,
                }
            })?;
        let container = text_parameter(parameters, "container_name")
            .unwrap_or_else(|| mongodb_container_name(&service.name));
        let host = text_parameter(parameters, "host").unwrap_or_else(|| "localhost".to_string());
        let port =
            text_parameter(parameters, "port").unwrap_or_else(|| MONGODB_INTERNAL_PORT.to_string());
        let replica_set = text_parameter(parameters, "replica_set").is_some();

        // `directConnection` keeps the driver on exactly this host:port: a
        // replica-set member registers itself under an address that only
        // resolves inside its own container.
        let connection_string = format!(
            "mongodb://{}:{}@{}:{}/?authSource=admin&directConnection=true",
            urlencoding::encode(&admin_user),
            urlencoding::encode(&admin_password),
            host,
            port
        );
        let mut options = ClientOptions::parse(&connection_string)
            .await
            .map_err(|error| failure(service.id, "parse the connection string", error))?;
        options.server_selection_timeout = Some(OPERATION_TIMEOUT);
        options.connect_timeout = Some(OPERATION_TIMEOUT);
        let client = Client::with_options(options)
            .map_err(|error| failure(service.id, "open a client", error))?;
        let database = client.database(&plan.database);

        let created =
            tokio::time::timeout(OPERATION_TIMEOUT, database.run_command(plan.create_user()))
                .await
                .map_err(|_| {
                    failure(
                        service.id,
                        &format!("create the user '{}'", plan.user),
                        format!("timed out after {}s", OPERATION_TIMEOUT.as_secs()),
                    )
                })?;
        match created {
            Ok(_) => {}
            Err(error) if is_user_exists(&error) => {
                tokio::time::timeout(OPERATION_TIMEOUT, database.run_command(plan.update_user()))
                    .await
                    .map_err(|_| {
                        failure(
                            service.id,
                            &format!("update the user '{}'", plan.user),
                            format!("timed out after {}s", OPERATION_TIMEOUT.as_secs()),
                        )
                    })?
                    .map_err(|error| {
                        failure(
                            service.id,
                            &format!("update the user '{}'", plan.user),
                            error,
                        )
                    })?;
            }
            Err(error) => {
                return Err(failure(
                    service.id,
                    &format!("create the user '{}'", plan.user),
                    error,
                ))
            }
        }

        info!(
            service_id = service.id,
            project_id = ?project_id,
            database = %plan.database,
            user = %plan.user,
            "Provisioned a database user for a sandbox"
        );
        Ok(plan.environment(&container, replica_set))
    }
}

fn is_user_exists(error: &mongodb::error::Error) -> bool {
    matches!(
        error.kind.as_ref(),
        ErrorKind::Command(CommandError { code, .. }) if *code == USER_ALREADY_EXISTS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> MongoDbSandboxPlan {
        MongoDbSandboxPlan::new(5, "shop", "adminpassword").expect("plan")
    }

    #[test]
    fn the_database_and_user_are_named_for_the_namespace() {
        let plan = plan();
        assert_eq!(plan.database, "shop_sandbox");
        assert_eq!(plan.user, "sbx_shop_sandbox");
    }

    #[test]
    fn a_namespace_with_odd_characters_still_gives_a_plain_database_name() {
        assert_eq!(database_name("My-Shop.v2"), "my_shop_v2_sandbox");
        assert_eq!(database_name("2fast"), "db_2fast_sandbox");
        let long = database_name(&"a".repeat(100));
        assert!(long.len() <= DATABASE_MAX);
    }

    #[test]
    fn the_user_may_read_and_write_its_own_database_only() {
        let command = plan().create_user();
        assert_eq!(command.get_str("createUser").ok(), Some("sbx_shop_sandbox"));
        let roles = command.get_array("roles").expect("roles");
        assert_eq!(roles.len(), 1, "exactly one role");
        let role = roles[0].as_document().expect("role document");
        assert_eq!(role.get_str("role").ok(), Some("readWrite"));
        assert_eq!(role.get_str("db").ok(), Some("shop_sandbox"));
    }

    #[test]
    fn updating_a_user_resets_its_password_and_roles() {
        let update = plan().update_user();
        let create = plan().create_user();
        assert_eq!(
            update.get_str("updateUser").ok(),
            create.get_str("createUser").ok()
        );
        assert_eq!(update.get_str("pwd").ok(), create.get_str("pwd").ok());
        assert_eq!(
            update.get_array("roles").ok(),
            create.get_array("roles").ok()
        );
    }

    #[test]
    fn the_password_is_stable_and_follows_the_administrator_password() {
        assert_eq!(plan(), plan());
        let other = MongoDbSandboxPlan::new(5, "shop", "another").expect("plan");
        assert_ne!(plan().password, other.password);
    }

    #[test]
    fn a_service_without_an_administrator_password_is_refused() {
        assert!(MongoDbSandboxPlan::new(5, "shop", "").is_err());
    }

    #[test]
    fn the_environment_keeps_the_deployment_names_and_authenticates_against_its_database() {
        let environment = plan().environment("mongo-shop", false);
        for name in [
            "MONGODB_URL",
            "MONGODB_HOST",
            "MONGODB_PORT",
            "MONGODB_DATABASE",
            "MONGODB_USERNAME",
            "MONGODB_PASSWORD",
        ] {
            assert!(environment.contains_key(name), "{name}");
        }
        let url = &environment["MONGODB_URL"];
        assert!(
            url.contains("/shop_sandbox?authSource=shop_sandbox"),
            "{url}"
        );
        assert!(!url.contains("directConnection"));
        assert!(plan().environment("mongo-shop", true)["MONGODB_URL"]
            .ends_with("&directConnection=true"));
        assert!(!environment
            .values()
            .any(|value| value.contains("adminpassword")));
    }

    #[test]
    fn debug_output_hides_the_password() {
        let plan = plan();
        assert!(!format!("{plan:?}").contains(&plan.password));
    }
}
