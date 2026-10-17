// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Credentials a sandbox can use against a linked PostgreSQL service
//! (ADR-051).
//!
//! A deployment connects as the cluster's administrator to a database named
//! for its environment. A sandbox runs code nobody reviewed, so it gets its
//! own database (`<project>_sandbox`) and a role that can use that database and
//! nothing else. Everything here is pure: names, the derived password, the SQL
//! to run and the environment variables to hand out. Running the SQL is the
//! manager's job, because standalone and cluster services are reached
//! differently.

use std::collections::HashMap;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::externalsvc::postgres::PostgresService;
use crate::externalsvc::scoped_resource_name;
use crate::sandbox_engines::{SandboxAccessEngine, SandboxAccessRequest};
use crate::services::{ExternalServiceError, ExternalServiceManager};
use temps_entities::external_services;
use tracing::info;

/// The environment slot a sandbox occupies in a project's database naming.
const SANDBOX_SCOPE: &str = "sandbox";
/// PostgreSQL truncates identifiers at 63 bytes.
const IDENTIFIER_MAX: usize = 63;
const ROLE_PREFIX: &str = "sbx_";
/// Connections the role may hold at once, so a runaway loop in a sandbox
/// cannot exhaust the cluster's connection slots.
const ROLE_CONNECTION_LIMIT: u32 = 20;
/// Tag of the HMAC input. Bump it to rotate every derived password at once.
const PASSWORD_DERIVATION_TAG: &str = "temps-scoped-role-v1";

type HmacSha256 = Hmac<Sha256>;

/// Why a plan could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPlanError {
    /// A name that would be spliced into SQL as an identifier is not made of
    /// `[a-z0-9_]`.
    UnsafeIdentifier { kind: &'static str, value: String },
    /// The administrator password could not key the HMAC.
    PasswordDerivation { reason: String },
}

impl std::fmt::Display for SandboxPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafeIdentifier { kind, value } => write!(
                formatter,
                "the {kind} name '{value}' is not a plain lowercase identifier ([a-z0-9_], at \
                 most {IDENTIFIER_MAX} characters, not starting with a digit)"
            ),
            Self::PasswordDerivation { reason } => {
                write!(formatter, "could not derive the sandbox password: {reason}")
            }
        }
    }
}

impl std::error::Error for SandboxPlanError {}

/// The database a project's sandboxes share on a service.
///
/// Named for the project, so a new chat on the same project continues with the
/// same data, and distinct from every `<project>_<environment>` database.
pub fn sandbox_database_name(project_slug: &str) -> String {
    PostgresService::normalize_database_name(&scoped_resource_name(project_slug, SANDBOX_SCOPE))
}

/// The role that owns `database`. Roles are cluster-wide like databases, so the
/// name is derived from the database name; when that would not fit in an
/// identifier it keeps a prefix and a digest of the whole name instead, so two
/// long names cannot collapse into one role.
pub fn sandbox_role_name(database: &str) -> String {
    let direct = format!("{ROLE_PREFIX}{database}");
    if direct.len() <= IDENTIFIER_MAX {
        return direct;
    }
    let digest = hex::encode(Sha256::digest(database.as_bytes()));
    let keep = IDENTIFIER_MAX - ROLE_PREFIX.len() - 1 - 12;
    format!(
        "{ROLE_PREFIX}{}_{}",
        database.chars().take(keep).collect::<String>(),
        &digest[..12]
    )
}

/// A stable password for the role: it survives restarts without being stored,
/// and changes when the administrator password does.
pub fn derive_role_password(
    admin_password: &str,
    service_id: i32,
    database: &str,
) -> Result<String, SandboxPlanError> {
    let mut mac = HmacSha256::new_from_slice(admin_password.as_bytes()).map_err(|error| {
        SandboxPlanError::PasswordDerivation {
            reason: error.to_string(),
        }
    })?;
    mac.update(format!("{PASSWORD_DERIVATION_TAG}:{service_id}:{database}").as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn is_plain_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= IDENTIFIER_MAX
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !value.as_bytes()[0].is_ascii_digit()
}

/// SQL string literal for a value that is not an identifier (the administrator's
/// user name, which operators choose freely).
fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Everything needed to give a sandbox its database, as SQL.
///
/// Every statement is idempotent, so provisioning can run on each connect and
/// repairs a role or database that drifted.
#[derive(Clone, PartialEq, Eq)]
pub struct SandboxDatabasePlan {
    pub database: String,
    pub role: String,
    password: String,
    admin_user: String,
}

impl std::fmt::Debug for SandboxDatabasePlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SandboxDatabasePlan")
            .field("database", &self.database)
            .field("role", &self.role)
            .field("password", &"***")
            .finish()
    }
}

impl SandboxDatabasePlan {
    pub fn new(
        service_id: i32,
        project_slug: &str,
        admin_user: &str,
        admin_password: &str,
    ) -> Result<Self, SandboxPlanError> {
        let database = sandbox_database_name(project_slug);
        if !is_plain_identifier(&database) {
            return Err(SandboxPlanError::UnsafeIdentifier {
                kind: "database",
                value: database,
            });
        }
        let role = sandbox_role_name(&database);
        if !is_plain_identifier(&role) {
            return Err(SandboxPlanError::UnsafeIdentifier {
                kind: "role",
                value: role,
            });
        }
        let password = derive_role_password(admin_password, service_id, &database)?;
        Ok(Self {
            database,
            role,
            password,
            admin_user: admin_user.to_string(),
        })
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    /// Create the role, or put an existing one back to the intended
    /// attributes and password.
    pub fn role_statement(&self) -> String {
        let attributes = format!(
            "LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS \
             CONNECTION LIMIT {ROLE_CONNECTION_LIMIT} PASSWORD '{}'",
            self.password
        );
        format!(
            "DO $sbx$ BEGIN \
             IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = {literal}) THEN \
             CREATE ROLE \"{role}\" {attributes}; \
             ELSE ALTER ROLE \"{role}\" {attributes}; \
             END IF; END $sbx$",
            literal = quote_literal(&self.role),
            role = self.role,
        )
    }

    /// Bound with the database name as `$1`.
    pub const DATABASE_EXISTS_QUERY: &'static str =
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)";

    /// `CREATE DATABASE` cannot run inside a function or transaction, so it is
    /// its own statement, chosen by the caller after the existence check.
    pub fn create_database_statement(&self) -> String {
        format!(
            "CREATE DATABASE \"{}\" OWNER \"{}\"",
            self.database, self.role
        )
    }

    /// For a database that already exists (an earlier run, or one created by
    /// hand): make the role its owner, so migrations work.
    pub fn own_database_statement(&self) -> String {
        format!(
            "ALTER DATABASE \"{}\" OWNER TO \"{}\"",
            self.database, self.role
        )
    }

    /// Keep the role out of every other database.
    ///
    /// New databases let anyone connect (`PUBLIC` holds `CONNECT`), so the role
    /// could open all of them. This takes that right away from `PUBLIC` on the
    /// sandbox database and on the databases the administrator owns (the
    /// per-environment ones Temps creates). It leaves `postgres`, the
    /// templates, and databases owned by other roles as they were, so tools
    /// that rely on them keep working.
    pub fn lockdown_statement(&self) -> String {
        format!(
            "DO $sbx$ DECLARE d record; BEGIN \
             FOR d IN SELECT datname FROM pg_database \
             WHERE datallowconn AND NOT datistemplate AND datname <> 'postgres' \
             AND datname <> {database_literal} \
             AND datdba = (SELECT oid FROM pg_roles WHERE rolname = {admin_literal}) \
             LOOP EXECUTE format('REVOKE CONNECT ON DATABASE %I FROM PUBLIC', d.datname); \
             END LOOP; \
             REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC; \
             END $sbx$",
            database_literal = quote_literal(&self.database),
            admin_literal = quote_literal(&self.admin_user),
            database = self.database,
        )
    }

    /// The statements that follow `CREATE`/`ALTER DATABASE`, in order.
    pub fn finishing_statements(&self) -> Vec<String> {
        vec![self.lockdown_statement()]
    }

    /// The variables the app reads, under the same names a deployment gets.
    /// `hosts` are the database containers on the sandbox's data network, with
    /// the port the server listens on inside them.
    pub fn environment(&self, hosts: &[(String, u16)]) -> HashMap<String, String> {
        postgres_environment(&self.role, &self.password, &self.database, hosts)
    }
}

/// A statement of the plan failed.
#[derive(Debug)]
pub struct SandboxApplyError {
    pub step: &'static str,
    pub database: String,
    pub source: tokio_postgres::Error,
}

impl std::fmt::Display for SandboxApplyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "could not {} for the sandbox database '{}': {}",
            self.step, self.database, self.source
        )
    }
}

impl std::error::Error for SandboxApplyError {}

/// Run the plan as the administrator, on a connection to any database of the
/// server (the primary, for a cluster).
pub async fn apply(
    client: &tokio_postgres::Client,
    plan: &SandboxDatabasePlan,
) -> Result<(), SandboxApplyError> {
    let failed = |step: &'static str| {
        let database = plan.database.clone();
        move |source: tokio_postgres::Error| SandboxApplyError {
            step,
            database,
            source,
        }
    };
    client
        .batch_execute(&plan.role_statement())
        .await
        .map_err(failed("create the role"))?;
    let exists: bool = client
        .query_one(
            SandboxDatabasePlan::DATABASE_EXISTS_QUERY,
            &[&plan.database],
        )
        .await
        .map_err(failed("check whether the database exists"))?
        .get(0);
    let ownership = if exists {
        plan.own_database_statement()
    } else {
        plan.create_database_statement()
    };
    client
        .batch_execute(&ownership)
        .await
        .map_err(failed("create the database"))?;
    for statement in plan.finishing_statements() {
        client
            .batch_execute(&statement)
            .await
            .map_err(failed("restrict who can connect"))?;
    }
    Ok(())
}

/// Same variable names as the deployment path, whatever the credentials are.
pub fn postgres_environment(
    user: &str,
    password: &str,
    database: &str,
    hosts: &[(String, u16)],
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("POSTGRES_USER".to_string(), user.to_string());
    env.insert("POSTGRES_PASSWORD".to_string(), password.to_string());
    env.insert("POSTGRES_DB".to_string(), database.to_string());
    env.insert("POSTGRES_NAME".to_string(), database.to_string());
    env.insert("POSTGRES_DATABASE".to_string(), database.to_string());
    if hosts.is_empty() {
        return env;
    }
    // One endpoint everywhere: the URL names the writable member, like
    // `POSTGRES_HOST`/`POSTGRES_PORT`. A multi-host URL is understood by libpq
    // alone; the common Node and Python drivers reject it. The variables are
    // resolved again for every command, so a failover shows up in the next one.
    let authority = format!(
        "{}:{}",
        crate::externalsvc::postgres_cluster::uri_host(&hosts[0].0),
        hosts[0].1
    );
    env.insert(
        "POSTGRES_URL".to_string(),
        format!(
            "postgresql://{}:{}@{authority}/{database}",
            urlencoding::encode(user),
            urlencoding::encode(password),
        ),
    );
    // One endpoint, as a single `host` and `port` pair: a tool that reads
    // these variables dials exactly this and nothing else. A cluster's
    // members listen on different ports, so joining the hosts under one port
    // would send some of them to a port nothing listens on. The caller puts
    // the writable member first.
    env.insert("POSTGRES_HOST".to_string(), hosts[0].0.clone());
    env.insert("POSTGRES_PORT".to_string(), hosts[0].1.to_string());
    env
}

/// PostgreSQL's sandbox access: a database and a role of the sandbox's own on
/// a standalone service or on a cluster's primary.
///
/// Provisions on every call (the statements are idempotent), so a role whose
/// administrator password changed, or a database dropped by hand, is repaired
/// rather than handed out broken. The hosts are the database containers by
/// name: the sandbox resolves them on its data network.
pub struct PostgresSandboxAccess;

#[async_trait::async_trait]
impl SandboxAccessEngine for PostgresSandboxAccess {
    fn service_types(&self) -> &'static [&'static str] {
        &["postgres"]
    }

    async fn containers(
        &self,
        manager: &ExternalServiceManager,
        service: &external_services::Model,
    ) -> Result<Vec<String>, ExternalServiceError> {
        if service.node_id.is_some() {
            return Ok(Vec::new());
        }
        // Every local member, whatever its status now: one that is down when
        // the sandbox connects is the primary after the next failover.
        if service.topology == "cluster" {
            let members = manager.get_service_members(service.id).await?;
            return Ok(members
                .into_iter()
                .filter(|member| {
                    member.node_id.is_none() && !crate::services::is_role_monitor(&member.role)
                })
                .map(|member| member.container_name)
                .collect());
        }
        Ok(vec![service.container_name.clone().unwrap_or_else(|| {
            crate::externalsvc::postgres::postgres_container_name(&service.name)
        })])
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
        let service_id = service.id;
        let param = |key: &str, default: &str| {
            parameters
                .get(key)
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .unwrap_or(default)
                .to_string()
        };
        let admin_user = param("username", "postgres");
        let admin_password = param("password", "");
        let plan = SandboxDatabasePlan::new(service_id, namespace, &admin_user, &admin_password)
            .map_err(|error| ExternalServiceError::SandboxAccessUnsupported {
                service_id,
                service_type: service.service_type.clone(),
                reason: error.to_string(),
            })?;

        let hosts = if service.topology == "cluster" {
            let members = manager.get_service_members(service_id).await?;
            let mut hosts: Vec<(String, u16)> = members
                .iter()
                .filter(|member| {
                    !crate::services::is_role_monitor(&member.role)
                        && member.status == "running"
                        && member.node_id.is_none()
                })
                // A cluster member listens on its own stored port inside the
                // container as well as on the host (the container publishes
                // `port` to `port`), unlike a standalone service's 5432.
                .map(|member| {
                    let port = member
                        .port
                        .and_then(|port| u16::try_from(port).ok())
                        .unwrap_or(5432);
                    (member.container_name.clone(), port)
                })
                .collect();
            if hosts.is_empty() {
                return Err(ExternalServiceError::SandboxAccessUnsupported {
                    service_id,
                    service_type: service.service_type.clone(),
                    reason: "no running data member of this cluster is on the control plane's \
                             Docker network, which is the only place a sandbox can reach"
                        .to_string(),
                });
            }
            let (client, primary) = manager.connect_cluster_primary_client(service_id).await?;
            crate::sandbox_access::apply(&client, &plan)
                .await
                .map_err(|error| ExternalServiceError::InternalError {
                    reason: format!(
                        "Failed to provision the sandbox database on cluster {service_id}: {error}"
                    ),
                })?;
            // The writable member first: `POSTGRES_HOST`/`POSTGRES_PORT` name
            // one endpoint, and it should be the one that accepts writes now.
            hosts.sort_by_key(|(container, _)| *container != primary);
            hosts
        } else {
            if let crate::services::ServiceExecutionRoute::Remote(node_id) =
                crate::services::service_execution_route(service.node_id)
            {
                return Err(ExternalServiceError::SandboxAccessUnsupported {
                    service_id,
                    service_type: service.service_type.clone(),
                    reason: format!(
                        "the service runs on node {node_id}, and a sandbox can only reach \
                         services on the control plane"
                    ),
                });
            }
            let host = param("host", "localhost");
            let port = parameters
                .get("port")
                .and_then(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| value.as_u64().map(|number| number.to_string()))
                })
                .unwrap_or_else(|| "5432".to_string());
            let port_number: u16 =
                port.parse()
                    .map_err(|_| ExternalServiceError::ParameterValidationFailed {
                        service_id,
                        reason: format!("The service's port '{port}' is not a port number"),
                    })?;
            let (client, connection) = tokio_postgres::Config::new()
                .host(&host)
                .port(port_number)
                .user(&admin_user)
                .password(&admin_password)
                .dbname("postgres")
                .connect_timeout(std::time::Duration::from_secs(10))
                .connect(tokio_postgres::NoTls)
                .await
                .map_err(|error| ExternalServiceError::InternalError {
                    reason: format!(
                        "Failed to connect to PostgreSQL service {} at {}:{} to provision the \
                         sandbox database '{}': {}",
                        service_id, host, port, plan.database, error
                    ),
                })?;
            let driver = tokio::spawn(connection);
            let applied = crate::sandbox_access::apply(&client, &plan).await;
            drop(client);
            driver.abort();
            applied.map_err(|error| ExternalServiceError::InternalError {
                reason: format!(
                    "Failed to provision the sandbox database on service {service_id}: {error}"
                ),
            })?;
            let container = service
                .container_name
                .clone()
                .unwrap_or_else(|| format!("postgres-{}", service.name));
            vec![(container, 5432)]
        };

        info!(
            service_id,
            project_id = ?project_id,
            database = %plan.database,
            role = %plan.role,
            "Provisioned a sandbox database and scoped role"
        );
        Ok(plan.environment(&hosts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> SandboxDatabasePlan {
        SandboxDatabasePlan::new(3, "shop-api", "postgres", "admin-secret").expect("plan builds")
    }

    #[test]
    fn the_sandbox_database_is_named_for_the_project_and_the_sandbox_scope() {
        assert_eq!(sandbox_database_name("shop-api"), "shop_api_sandbox");
        assert_eq!(sandbox_database_name("7days"), "db_7days_sandbox");
    }

    #[test]
    fn the_role_name_follows_the_database_and_fits_an_identifier() {
        assert_eq!(
            sandbox_role_name("shop_api_sandbox"),
            "sbx_shop_api_sandbox"
        );
        let long = "a".repeat(63);
        let role = sandbox_role_name(&long);
        assert!(role.len() <= IDENTIFIER_MAX, "{role}");
        assert!(is_plain_identifier(&role));
    }

    #[test]
    fn two_long_databases_with_the_same_start_get_different_roles() {
        let first = format!("{}x", "a".repeat(62));
        let second = format!("{}y", "a".repeat(62));
        assert_ne!(sandbox_role_name(&first), sandbox_role_name(&second));
    }

    #[test]
    fn the_password_is_stable_and_depends_on_every_input() {
        let base = derive_role_password("admin-secret", 3, "shop_api_sandbox").expect("derives");
        assert_eq!(
            base,
            derive_role_password("admin-secret", 3, "shop_api_sandbox").expect("derives")
        );
        assert_eq!(base.len(), 64);
        assert!(base.bytes().all(|byte| byte.is_ascii_hexdigit()));
        for other in [
            derive_role_password("other-secret", 3, "shop_api_sandbox"),
            derive_role_password("admin-secret", 4, "shop_api_sandbox"),
            derive_role_password("admin-secret", 3, "other_sandbox"),
        ] {
            assert_ne!(base, other.expect("derives"));
        }
    }

    #[test]
    fn the_role_cannot_do_anything_beyond_using_its_database() {
        let statement = plan().role_statement();
        for attribute in [
            "NOSUPERUSER",
            "NOCREATEDB",
            "NOCREATEROLE",
            "NOREPLICATION",
            "NOBYPASSRLS",
            "CONNECTION LIMIT 20",
        ] {
            assert!(statement.contains(attribute), "{attribute}: {statement}");
        }
        // Created when missing, corrected when present.
        assert!(statement.contains("CREATE ROLE \"sbx_shop_api_sandbox\""));
        assert!(statement.contains("ALTER ROLE \"sbx_shop_api_sandbox\""));
    }

    #[test]
    fn the_database_belongs_to_the_role() {
        let plan = plan();
        assert_eq!(
            plan.create_database_statement(),
            "CREATE DATABASE \"shop_api_sandbox\" OWNER \"sbx_shop_api_sandbox\""
        );
        assert_eq!(
            plan.own_database_statement(),
            "ALTER DATABASE \"shop_api_sandbox\" OWNER TO \"sbx_shop_api_sandbox\""
        );
    }

    #[test]
    fn the_lockdown_closes_the_administrators_databases_but_not_the_maintenance_ones() {
        let statement = plan().lockdown_statement();
        assert!(statement.contains("REVOKE CONNECT ON DATABASE %I FROM PUBLIC"));
        assert!(statement.contains("datname <> 'postgres'"));
        assert!(statement.contains("NOT datistemplate"));
        assert!(statement.contains("rolname = 'postgres'"));
        // The sandbox database is excluded from the loop and revoked on its own.
        assert!(statement.contains("datname <> 'shop_api_sandbox'"));
        assert!(statement.contains("REVOKE ALL ON DATABASE \"shop_api_sandbox\" FROM PUBLIC"));
    }

    #[test]
    fn an_administrator_name_with_a_quote_cannot_break_out_of_the_literal() {
        let plan = SandboxDatabasePlan::new(1, "shop", "o'brien", "pw").expect("plan builds");
        assert!(plan.lockdown_statement().contains("rolname = 'o''brien'"));
    }

    #[test]
    fn a_project_slug_cannot_smuggle_sql_into_an_identifier() {
        // Slugs are normalised to [a-z0-9_] before they get here; the check is
        // the backstop if that ever changes.
        for slug in ["shop\"; DROP DATABASE x; --", "a b", "ünï"] {
            if let Ok(plan) = SandboxDatabasePlan::new(1, slug, "postgres", "pw") {
                assert!(is_plain_identifier(&plan.database), "{}", plan.database);
                assert!(is_plain_identifier(&plan.role), "{}", plan.role);
            }
        }
        assert!(!is_plain_identifier("bad\"name"));
        assert!(!is_plain_identifier("1starts_with_digit"));
        assert!(!is_plain_identifier(""));
        assert!(!is_plain_identifier(&"a".repeat(64)));
    }

    #[test]
    fn the_environment_uses_the_deployment_variable_names() {
        let env = plan().environment(&[("postgres-main".to_string(), 5432)]);
        for name in [
            "POSTGRES_URL",
            "POSTGRES_HOST",
            "POSTGRES_PORT",
            "POSTGRES_DB",
            "POSTGRES_USER",
            "POSTGRES_PASSWORD",
        ] {
            assert!(env.contains_key(name), "missing {name}");
        }
        assert_eq!(env["POSTGRES_DB"], "shop_api_sandbox");
        assert_eq!(env["POSTGRES_USER"], "sbx_shop_api_sandbox");
        assert_eq!(env["POSTGRES_HOST"], "postgres-main");
        assert_eq!(env["POSTGRES_PORT"], "5432");
        assert!(env["POSTGRES_URL"].starts_with("postgresql://sbx_shop_api_sandbox:"));
        assert!(env["POSTGRES_URL"].ends_with("@postgres-main:5432/shop_api_sandbox"));
    }

    #[test]
    fn the_environment_never_carries_the_administrator_credentials() {
        let env = plan().environment(&[("postgres-main".to_string(), 5432)]);
        let joined = env.values().cloned().collect::<Vec<_>>().join("\n");
        assert!(!joined.contains("admin-secret"));
        assert_ne!(env["POSTGRES_USER"], "postgres");
    }

    #[test]
    fn a_cluster_url_names_only_the_writable_member() {
        let env = plan().environment(&[
            ("postgres-main-2".to_string(), 6012),
            ("postgres-main-1".to_string(), 6011),
        ]);
        let url = &env["POSTGRES_URL"];
        // One host: the common Node and Python drivers reject a host list.
        assert!(
            url.ends_with("@postgres-main-2:6012/shop_api_sandbox"),
            "{url}"
        );
        assert!(!url.contains(','), "{url}");
        assert!(!url.contains("target_session_attrs"), "{url}");
    }

    #[test]
    fn host_and_port_name_one_endpoint_even_for_a_cluster() {
        let env = plan().environment(&[
            ("postgres-main-2".to_string(), 6012),
            ("postgres-main-1".to_string(), 6011),
        ]);
        // The pair must be dialable as given: the first member's host with
        // its own port, never a list of hosts under one port.
        assert_eq!(env["POSTGRES_HOST"], "postgres-main-2");
        assert_eq!(env["POSTGRES_PORT"], "6012");
        assert!(!env["POSTGRES_HOST"].contains(','));
    }

    #[test]
    fn without_a_reachable_host_only_the_credentials_are_returned() {
        let env = plan().environment(&[]);
        assert!(!env.contains_key("POSTGRES_URL"));
        assert!(env.contains_key("POSTGRES_PASSWORD"));
    }

    #[test]
    fn the_debug_output_hides_the_password() {
        let plan = plan();
        let shown = format!("{plan:?}");
        assert!(!shown.contains(plan.password()), "{shown}");
        assert!(shown.contains("***"));
    }

    // ---- against a real server -------------------------------------------

    use testcontainers::{
        core::{ContainerPort, WaitFor},
        runners::AsyncRunner,
        GenericImage, ImageExt,
    };

    async fn connect(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
        let (client, connection) = tokio_postgres::Config::new()
            .host(host)
            .port(port)
            .user(user)
            .password(password)
            .dbname(database)
            .connect(tokio_postgres::NoTls)
            .await?;
        tokio::spawn(connection);
        Ok(client)
    }

    #[tokio::test]
    async fn the_role_owns_its_database_and_can_reach_nothing_else() {
        let docker_up = match bollard::Docker::connect_with_defaults() {
            Ok(docker) => docker.ping().await.is_ok(),
            Err(_) => false,
        };
        if !docker_up {
            println!("Docker not available, skipping");
            return;
        }
        let container = GenericImage::new("postgres", "18-alpine")
            .with_exposed_port(ContainerPort::Tcp(5432))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_USER", "postgres")
            .with_env_var("POSTGRES_PASSWORD", "admin-secret")
            .start()
            .await
            .expect("the PostgreSQL test container starts");
        let host = container.get_host().await.expect("host").to_string();
        let port = container.get_host_port_ipv4(5432).await.expect("port");

        let admin = connect(&host, port, "postgres", "admin-secret", "postgres")
            .await
            .expect("admin connects");
        // What a deployment's database looks like: created by the
        // administrator, with data in it.
        admin
            .batch_execute("CREATE DATABASE shop_production")
            .await
            .expect("production database");
        let production = connect(&host, port, "postgres", "admin-secret", "shop_production")
            .await
            .expect("admin reaches production");
        production
            .batch_execute("CREATE TABLE orders (id int); INSERT INTO orders VALUES (1)")
            .await
            .expect("production data");

        let plan = SandboxDatabasePlan::new(1, "shop", "postgres", "admin-secret").expect("plan");
        apply(&admin, &plan).await.expect("first apply");
        // Provisioning runs on every connect: it must be safe to repeat.
        apply(&admin, &plan).await.expect("second apply");

        // The role can build in its own database, migrations included.
        let sandbox = connect(&host, port, &plan.role, plan.password(), &plan.database)
            .await
            .expect("the role connects to its database");
        sandbox
            .batch_execute(
                "CREATE TABLE notes (id serial PRIMARY KEY, body text); \
                 INSERT INTO notes (body) VALUES ('hello'); \
                 CREATE SCHEMA app; CREATE TABLE app.t (id int);",
            )
            .await
            .expect("the role owns its database");
        let rows = sandbox
            .query("SELECT body FROM notes", &[])
            .await
            .expect("reads back");
        assert_eq!(rows.len(), 1);

        // It cannot reach production, wherever it tries.
        let refused = connect(&host, port, &plan.role, plan.password(), "shop_production").await;
        assert!(refused.is_err(), "the role must not connect to production");
        let message = refused
            .err()
            .and_then(|error| error.as_db_error().map(|db| db.message().to_string()))
            .unwrap_or_default();
        assert!(
            message.contains("permission denied for database"),
            "unexpected refusal: {message}"
        );

        // It cannot widen its own rights.
        for statement in [
            "CREATE DATABASE escape",
            "CREATE ROLE intruder LOGIN",
            "ALTER ROLE sbx_shop_sandbox SUPERUSER",
            "SET ROLE postgres",
        ] {
            assert!(
                sandbox.batch_execute(statement).await.is_err(),
                "must be refused: {statement}"
            );
        }

        // The wrong password is refused.
        assert!(connect(&host, port, &plan.role, "guess", &plan.database)
            .await
            .is_err());

        // Production is untouched for the administrator.
        let count: i64 = production
            .query_one("SELECT count(*) FROM orders", &[])
            .await
            .expect("administrator still reads production")
            .get(0);
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn a_database_created_by_hand_is_taken_over_by_the_role() {
        let docker_up = match bollard::Docker::connect_with_defaults() {
            Ok(docker) => docker.ping().await.is_ok(),
            Err(_) => false,
        };
        if !docker_up {
            println!("Docker not available, skipping");
            return;
        }
        let container = GenericImage::new("postgres", "18-alpine")
            .with_exposed_port(ContainerPort::Tcp(5432))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_USER", "postgres")
            .with_env_var("POSTGRES_PASSWORD", "admin-secret")
            .start()
            .await
            .expect("the PostgreSQL test container starts");
        let host = container.get_host().await.expect("host").to_string();
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        let admin = connect(&host, port, "postgres", "admin-secret", "postgres")
            .await
            .expect("admin connects");
        admin
            .batch_execute("CREATE DATABASE shop_sandbox")
            .await
            .expect("pre-existing database");

        let plan = SandboxDatabasePlan::new(1, "shop", "postgres", "admin-secret").expect("plan");
        apply(&admin, &plan).await.expect("apply");

        let sandbox = connect(&host, port, &plan.role, plan.password(), &plan.database)
            .await
            .expect("the role connects");
        sandbox
            .batch_execute("CREATE TABLE t (id int)")
            .await
            .expect("the role now owns the database");
    }
}
