// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Custom hostnames for public status pages (#1412).
//!
//! A hostname attached to a page is served only once its ownership is proven,
//! and ownership is proven the way Temps already proves it for every other
//! certificate: an ACME order on a `domains` row (HTTP-01, or DNS-01 with a TXT
//! record). The certificate *is* the proof: until the order completes there
//! is no certificate to serve and the proxy answers 404 for the hostname.
//!
//! The `domains` row is created for the binding and owned by it alone; a
//! database trigger deletes it whenever the binding is removed, re-pointed or
//! detached, so serving and renewal stop on every path (see
//! `m20261012_000002_create_status_page_domains`).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use temps_config::ConfigService;
use temps_core::route_table::RouteTableRefresher;
use temps_core::UtcDateTime;
use temps_entities::{domains, status_page_domains, status_pages};
use tracing::{error, info, warn};
use utoipa::ToSchema;

use super::snapshot::StatusSnapshotCache;
use crate::services::StatusPageError;

/// Most custom hostnames per status page.
pub const MAX_DOMAINS_PER_STATUS_PAGE: u64 = 5;

/// Attach/verify operations allowed per project per hour. Each one can start
/// an ACME order, so this bounds the load on the CA and its rate limits.
pub const DOMAIN_OPERATIONS_PER_HOUR: usize = 30;

/// How ownership of a hostname is proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub enum VerificationMethod {
    /// ACME HTTP-01: the hostname must already point at this Temps instance.
    #[default]
    #[serde(rename = "http-01")]
    Http01,
    /// ACME DNS-01: a TXT record under `_acme-challenge.<hostname>`.
    #[serde(rename = "dns-01")]
    Dns01,
}

impl VerificationMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http01 => "http-01",
            Self::Dns01 => "dns-01",
        }
    }

    fn from_stored(value: &str) -> Self {
        if value == "dns-01" {
            Self::Dns01
        } else {
            Self::Http01
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AddStatusPageDomainRequest {
    /// Hostname to serve the page on, e.g. `status.example.com`. Unicode names
    /// are converted to punycode; wildcards and IP addresses are refused.
    pub hostname: String,
    /// Defaults to `http-01`.
    #[serde(default)]
    pub verification_method: VerificationMethod,
}

/// Lifecycle of a custom hostname.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusPageDomainStatus {
    /// Waiting for the DNS records below and a verification attempt.
    Pending,
    /// Ownership proven; the certificate is issued and renewed automatically.
    Active,
    /// The last verification attempt failed; see `last_error`.
    Failed,
    /// No certificate: the page was unpublished, which releases certificates.
    /// Verify again after publishing.
    Unverified,
}

/// A DNS record the operator must create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DnsRecordInstruction {
    /// `CNAME`, `A` or `TXT`.
    pub record_type: String,
    /// Fully qualified record name.
    pub name: String,
    /// Record value.
    pub value: String,
    /// Why the record is needed.
    pub purpose: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StatusPageDomainResponse {
    pub id: i32,
    pub hostname: String,
    pub verification_method: VerificationMethod,
    pub status: StatusPageDomainStatus,
    /// True when visitors can open the page on this hostname right now: the
    /// hostname is verified and the page is published.
    pub serving: bool,
    pub dns_records: Vec<DnsRecordInstruction>,
    /// Reason the last verification failed, if it did.
    pub last_error: Option<String>,
    #[schema(value_type = Option<String>, format = "date-time")]
    pub certificate_expires_at: Option<UtcDateTime>,
    #[schema(value_type = String, format = "date-time")]
    pub created_at: UtcDateTime,
}

/// Normalize and validate a hostname for a status page.
///
/// Lowercases, strips one trailing dot, converts Unicode labels to punycode
/// (IDNA), and refuses wildcards, IP literals, ports, paths, single-label
/// names and anything that is not a plain LDH hostname.
pub fn normalize_status_page_hostname(input: &str) -> Result<String, StatusPageError> {
    let invalid = |reason: &str| StatusPageError::InvalidHostname {
        hostname: input.trim().chars().take(260).collect(),
        reason: reason.to_string(),
    };
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(invalid("hostname is empty"));
    }
    if trimmed.contains('*') {
        return Err(invalid("wildcards are not allowed"));
    }
    if trimmed
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '/' | ':' | '@' | '?' | '#' | '[' | ']' | '\\'))
    {
        return Err(invalid(
            "enter a bare hostname without scheme, port or path",
        ));
    }
    let without_dot = trimmed.strip_suffix('.').unwrap_or(trimmed);
    let host = match url::Host::parse(without_dot) {
        Ok(url::Host::Domain(domain)) => domain,
        Ok(url::Host::Ipv4(_)) | Ok(url::Host::Ipv6(_)) => {
            return Err(invalid("IP addresses are not allowed; use a hostname"))
        }
        Err(e) => return Err(invalid(&format!("not a valid hostname ({e})"))),
    };
    let host = host.to_ascii_lowercase();
    if host.len() > 253 {
        return Err(invalid("hostname is longer than 253 characters"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(invalid(
            "use a fully qualified hostname such as status.example.com",
        ));
    }
    for label in &labels {
        if label.is_empty() || label.len() > 63 {
            return Err(invalid("each label must be 1-63 characters"));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(invalid(
                "labels may contain only letters, digits and hyphens",
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(invalid("labels may not start or end with a hyphen"));
        }
    }
    if labels
        .last()
        .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid("IP addresses are not allowed; use a hostname"));
    }
    Ok(host)
}

fn inside_zone(host: &str, zone: &str) -> bool {
    let zone = zone
        .trim()
        .trim_start_matches("*.")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    !zone.is_empty() && (host == zone || host.ends_with(&format!(".{zone}")))
}

/// Certificate operations on the `domains` row of a binding. Implemented over
/// the shared `DomainService` in production; a fake in tests.
#[async_trait]
pub trait StatusPageCertificates: Send + Sync {
    /// Create the `domains` row for `hostname` and start its ACME order.
    /// Returns the row id. Requesting the challenge is best-effort: a failure
    /// is recorded on the row and retried by `complete`'s caller.
    async fn begin(
        &self,
        hostname: &str,
        method: VerificationMethod,
        acme_email: &str,
    ) -> Result<i32, String>;

    /// Request a fresh ACME challenge for an existing row.
    async fn request_challenge(&self, hostname: &str, acme_email: &str) -> Result<(), String>;

    /// Complete the ACME order. On success the row holds a certificate.
    async fn complete(&self, hostname: &str, acme_email: &str) -> Result<(), String>;
}

/// [`StatusPageCertificates`] over the shared domain/ACME service.
pub struct DomainServiceCertificates {
    domain_service: Arc<temps_domains::DomainService>,
}

impl DomainServiceCertificates {
    pub fn new(domain_service: Arc<temps_domains::DomainService>) -> Self {
        Self { domain_service }
    }
}

#[async_trait]
impl StatusPageCertificates for DomainServiceCertificates {
    async fn begin(
        &self,
        hostname: &str,
        method: VerificationMethod,
        acme_email: &str,
    ) -> Result<i32, String> {
        let row = self
            .domain_service
            .create_domain(hostname, method.as_str())
            .await
            .map_err(|e| e.to_string())?;
        if let Err(e) = self
            .domain_service
            .request_challenge(hostname, acme_email)
            .await
        {
            warn!(
                hostname,
                error = %e,
                "requesting the ACME challenge for a status page hostname failed; verification will retry"
            );
        }
        Ok(row.id)
    }

    async fn request_challenge(&self, hostname: &str, acme_email: &str) -> Result<(), String> {
        self.domain_service
            .request_challenge(hostname, acme_email)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn complete(&self, hostname: &str, acme_email: &str) -> Result<(), String> {
        self.domain_service
            .complete_challenge(hostname, acme_email)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Release the certificates of every hostname of a page (unpublish): the
/// trigger deletes each `domains` row, which stops serving and renewal.
pub(crate) async fn release_page_certificates<C: ConnectionTrait>(
    db: &C,
    status_page_id: i32,
) -> Result<u64, sea_orm::DbErr> {
    let result = db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE status_page_domains SET domain_id = NULL WHERE status_page_id = $1 AND domain_id IS NOT NULL",
            [status_page_id.into()],
        ))
        .await?;
    Ok(result.rows_affected())
}

/// Release the certificates of every status-page hostname of a project, as
/// soon as the project is fenced for deletion.
pub(crate) async fn release_project_certificates(
    db: &DatabaseConnection,
    project_id: i32,
) -> Result<u64, sea_orm::DbErr> {
    let result = db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            r#"UPDATE status_page_domains SET domain_id = NULL
               WHERE domain_id IS NOT NULL
                 AND status_page_id IN (SELECT id FROM status_pages WHERE project_id = $1)"#,
            [project_id.into()],
        ))
        .await?;
    Ok(result.rows_affected())
}

pub struct StatusPageDomainService {
    db: Arc<DatabaseConnection>,
    config_service: Arc<ConfigService>,
    cache: Arc<StatusSnapshotCache>,
    certificates: Arc<dyn StatusPageCertificates>,
    /// Knows every hostname an application serves. `None` fails closed: no
    /// hostname can be attached without the check.
    route_table: Option<Arc<dyn RouteTableRefresher>>,
    /// Recent attach/verify operations per project, for [`Self::throttle`].
    /// Bounded by the number of projects with recent operations.
    operations: std::sync::Mutex<
        std::collections::HashMap<i32, std::collections::VecDeque<std::time::Instant>>,
    >,
}

impl StatusPageDomainService {
    pub fn new(
        db: Arc<DatabaseConnection>,
        config_service: Arc<ConfigService>,
        cache: Arc<StatusSnapshotCache>,
        certificates: Arc<dyn StatusPageCertificates>,
        route_table: Option<Arc<dyn RouteTableRefresher>>,
    ) -> Self {
        Self {
            db,
            config_service,
            cache,
            certificates,
            route_table,
            operations: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub async fn list(
        &self,
        status_page_id: i32,
    ) -> Result<Vec<StatusPageDomainResponse>, StatusPageError> {
        let page = self.page(status_page_id).await?;
        let bindings = status_page_domains::Entity::find()
            .filter(status_page_domains::Column::StatusPageId.eq(status_page_id))
            .order_by_asc(status_page_domains::Column::Id)
            .all(self.db.as_ref())
            .await?;
        let domain_ids: Vec<i32> = bindings.iter().filter_map(|b| b.domain_id).collect();
        let rows = if domain_ids.is_empty() {
            Vec::new()
        } else {
            domains::Entity::find()
                .filter(domains::Column::Id.is_in(domain_ids))
                .all(self.db.as_ref())
                .await?
        };
        let target = self.dns_target().await;
        Ok(bindings
            .into_iter()
            .map(|binding| {
                let row = binding
                    .domain_id
                    .and_then(|id| rows.iter().find(|r| r.id == id));
                to_response(&binding, row, page.is_published, &target)
            })
            .collect())
    }

    pub async fn attach(
        &self,
        status_page_id: i32,
        request: AddStatusPageDomainRequest,
        acme_email: &str,
    ) -> Result<StatusPageDomainResponse, StatusPageError> {
        let hostname = normalize_status_page_hostname(&request.hostname)?;
        if acme_email.trim().is_empty() {
            return Err(StatusPageError::AcmeEmailRequired { hostname });
        }
        let page = self.page(status_page_id).await?;
        self.throttle(page.project_id)?;

        // Check and claim under the hostname lock, in one transaction, so a
        // concurrent claim of the same name (here or by a project,
        // environment or custom route) cannot also pass its checks.
        let txn = self.db.begin().await?;
        let result = async {
            status_page_domains::lock_hostname_claim(&txn, &hostname).await?;
            let attached = status_page_domains::Entity::find()
                .filter(status_page_domains::Column::StatusPageId.eq(status_page_id))
                .count(&txn)
                .await?;
            if attached >= MAX_DOMAINS_PER_STATUS_PAGE {
                return Err(StatusPageError::StatusPageDomainLimitReached {
                    status_page_id,
                    limit: MAX_DOMAINS_PER_STATUS_PAGE,
                });
            }
            self.ensure_available(&txn, &hostname).await?;
            // A tombstone of this hostname (removed earlier, or its page
            // deleted) is replaced by the new binding.
            status_page_domains::Entity::delete_many()
                .filter(status_page_domains::Column::Hostname.eq(hostname.as_str()))
                .filter(status_page_domains::Column::StatusPageId.is_null())
                .exec(&txn)
                .await?;
            let binding = status_page_domains::ActiveModel {
                status_page_id: Set(Some(status_page_id)),
                hostname: Set(hostname.clone()),
                verification_method: Set(request.verification_method.as_str().to_string()),
                domain_id: Set(None),
                ..Default::default()
            }
            .insert(&txn)
            .await
            .map_err(|e| {
                if is_unique_violation(&e) {
                    StatusPageError::HostnameInUse {
                        hostname: hostname.clone(),
                    }
                } else {
                    StatusPageError::Database(e)
                }
            })?;
            Ok(binding)
        }
        .await;
        let binding = match result {
            Ok(binding) => {
                txn.commit().await?;
                binding
            }
            Err(e) => {
                if let Err(rollback) = txn.rollback().await {
                    warn!(error = %rollback, "failed to roll back status page hostname claim");
                }
                return Err(e);
            }
        };

        // No certificate is requested while the page is unpublished: the
        // order starts when the hostname is verified on a published page.
        let binding = if page.is_published {
            match self
                .start_order(&binding, request.verification_method, acme_email)
                .await
            {
                Ok(binding) => binding,
                Err(e) => {
                    if let Err(delete) = status_page_domains::Entity::delete_by_id(binding.id)
                        .exec(self.db.as_ref())
                        .await
                    {
                        error!(binding_id = binding.id, error = %delete, "failed to roll back status page hostname binding");
                    }
                    return Err(e);
                }
            }
        } else {
            binding
        };
        info!(
            status_page_id,
            project_id = page.project_id,
            hostname = %hostname,
            "attached custom hostname to status page"
        );
        self.one(&binding, page.is_published).await
    }

    /// Create the `domains` row and ACME order for a binding, and link it —
    /// only if the binding is still attached to this page, still has no
    /// certificate, and the page is still published. Otherwise the new row
    /// is deleted at once, so no certificate can outlive its binding.
    async fn start_order(
        &self,
        binding: &status_page_domains::Model,
        method: VerificationMethod,
        acme_email: &str,
    ) -> Result<status_page_domains::Model, StatusPageError> {
        let hostname = binding.hostname.clone();
        let status_page_id = binding.status_page_id.unwrap_or_default();
        let domain_id = self
            .certificates
            .begin(&hostname, method, acme_email)
            .await
            .map_err(|reason| StatusPageError::CertificateOrder {
                hostname: hostname.clone(),
                reason,
            })?;
        let linked = self
            .db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                r#"UPDATE status_page_domains SET domain_id = $1, updated_at = now()
                   WHERE id = $2 AND status_page_id = $3 AND domain_id IS NULL
                     AND EXISTS (SELECT 1 FROM status_pages WHERE id = $3 AND is_published)"#,
                [domain_id.into(), binding.id.into(), status_page_id.into()],
            ))
            .await?
            .rows_affected();
        if linked != 1 {
            self.delete_certificate_row(domain_id, &hostname).await;
            return Err(StatusPageError::HostnameBindingChanged { hostname });
        }
        status_page_domains::Entity::find_by_id(binding.id)
            .one(self.db.as_ref())
            .await?
            .ok_or(StatusPageError::StatusPageDomainNotFound {
                status_page_id,
                domain_id: binding.id,
            })
    }

    async fn delete_certificate_row(&self, domain_id: i32, hostname: &str) {
        if let Err(e) = domains::Entity::delete_by_id(domain_id)
            .exec(self.db.as_ref())
            .await
        {
            error!(
                domain_id,
                hostname,
                error = %e,
                "failed to delete an unlinked status page certificate row; it would be renewed until removed"
            );
        }
    }

    /// Try to prove ownership now: completes the ACME order (requesting a new
    /// one first if the certificate was released). Only for published pages.
    pub async fn verify(
        &self,
        status_page_id: i32,
        domain_binding_id: i32,
        acme_email: &str,
    ) -> Result<StatusPageDomainResponse, StatusPageError> {
        let page = self.page(status_page_id).await?;
        let binding = self.binding(status_page_id, domain_binding_id).await?;
        let hostname = binding.hostname.clone();
        if acme_email.trim().is_empty() {
            return Err(StatusPageError::AcmeEmailRequired { hostname });
        }
        if !page.is_published {
            return Err(StatusPageError::DomainRequiresPublishedPage {
                status_page_id,
                hostname,
            });
        }
        self.throttle(page.project_id)?;
        let method = VerificationMethod::from_stored(&binding.verification_method);

        let binding = match binding.domain_id {
            Some(_) => binding,
            None => {
                let binding = self.start_order(&binding, method, acme_email).await?;
                if method == VerificationMethod::Dns01 {
                    // A new TXT value must be published before completing.
                    return self.one(&binding, page.is_published).await;
                }
                binding
            }
        };
        let Some(domain_id) = binding.domain_id else {
            return Err(StatusPageError::HostnameBindingChanged { hostname });
        };

        let row = domains::Entity::find_by_id(domain_id)
            .one(self.db.as_ref())
            .await?;
        let already_active = row.as_ref().is_some_and(|r| is_verified(r, &hostname));
        if !already_active {
            let needs_order = row
                .as_ref()
                .is_none_or(|r| r.status == "pending" || r.status == "failed");
            if needs_order {
                if let Err(reason) = self
                    .certificates
                    .request_challenge(&hostname, acme_email)
                    .await
                {
                    return Err(StatusPageError::DomainVerificationFailed { hostname, reason });
                }
                if method == VerificationMethod::Dns01 {
                    return self.one(&binding, page.is_published).await;
                }
            }
            if let Err(reason) = self.certificates.complete(&hostname, acme_email).await {
                return Err(StatusPageError::DomainVerificationFailed { hostname, reason });
            }
            // The page could have been unpublished or the hostname removed
            // while the order ran: then the certificate must not survive.
            let still_bound = status_page_domains::Entity::find_by_id(binding.id)
                .filter(status_page_domains::Column::StatusPageId.eq(status_page_id))
                .filter(status_page_domains::Column::DomainId.eq(domain_id))
                .one(self.db.as_ref())
                .await?
                .is_some()
                && self.page(status_page_id).await?.is_published;
            if !still_bound {
                self.delete_certificate_row(domain_id, &hostname).await;
                return Err(StatusPageError::HostnameBindingChanged { hostname });
            }
            info!(status_page_id, hostname = %hostname, "verified custom hostname of status page");
        }
        self.refresh_page(status_page_id).await;
        self.one(&binding, page.is_published).await
    }

    /// At most [`DOMAIN_OPERATIONS_PER_HOUR`] attach/verify operations per
    /// project per hour: each one can start an ACME order.
    fn throttle(&self, project_id: i32) -> Result<(), StatusPageError> {
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_secs(3600);
        let mut operations = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        operations.retain(|_, times| {
            times.retain(|t| now.duration_since(*t) < window);
            !times.is_empty()
        });
        let times = operations.entry(project_id).or_default();
        if times.len() >= DOMAIN_OPERATIONS_PER_HOUR {
            let retry_after_secs = times
                .front()
                .map(|oldest| window.saturating_sub(now.duration_since(*oldest)).as_secs())
                .unwrap_or(60);
            return Err(StatusPageError::DomainOperationsThrottled {
                project_id,
                retry_after_secs,
            });
        }
        times.push_back(now);
        Ok(())
    }

    /// Detach a hostname. It becomes a tombstone that keeps answering 404,
    /// and its certificate is deleted (serving and renewal stop).
    pub async fn remove(
        &self,
        status_page_id: i32,
        domain_binding_id: i32,
    ) -> Result<String, StatusPageError> {
        let binding = self.binding(status_page_id, domain_binding_id).await?;
        let hostname = binding.hostname.clone();
        let mut active: status_page_domains::ActiveModel = binding.into();
        active.status_page_id = Set(None);
        active.update(self.db.as_ref()).await?;
        // The console stops serving the page on this hostname now, before the
        // proxy's route table has reloaded.
        self.refresh_page(status_page_id).await;
        info!(status_page_id, hostname = %hostname, "removed custom hostname from status page");
        Ok(hostname)
    }

    async fn refresh_page(&self, status_page_id: i32) {
        if let Err(e) = self.cache.refresh_page(status_page_id).await {
            error!(
                status_page_id,
                error = %e,
                "failed to rebuild status page snapshot after a hostname change; taking the page offline until the next refresh"
            );
            self.cache.remove_page(status_page_id).await;
        }
    }

    async fn page(&self, status_page_id: i32) -> Result<status_pages::Model, StatusPageError> {
        status_pages::Entity::find_by_id(status_page_id)
            .one(self.db.as_ref())
            .await?
            .ok_or(StatusPageError::StatusPageNotFound { status_page_id })
    }

    async fn binding(
        &self,
        status_page_id: i32,
        domain_id: i32,
    ) -> Result<status_page_domains::Model, StatusPageError> {
        status_page_domains::Entity::find_by_id(domain_id)
            .filter(status_page_domains::Column::StatusPageId.eq(status_page_id))
            .one(self.db.as_ref())
            .await?
            .ok_or(StatusPageError::StatusPageDomainNotFound {
                status_page_id,
                domain_id,
            })
    }

    async fn one(
        &self,
        binding: &status_page_domains::Model,
        published: bool,
    ) -> Result<StatusPageDomainResponse, StatusPageError> {
        let row = match binding.domain_id {
            Some(id) => {
                domains::Entity::find_by_id(id)
                    .one(self.db.as_ref())
                    .await?
            }
            None => None,
        };
        let target = self.dns_target().await;
        Ok(to_response(binding, row.as_ref(), published, &target))
    }

    /// Refuse a hostname that anything else uses: the platform's own
    /// hostnames and zones, any application or project/environment domain or
    /// custom route (including wildcards covering it), another status page,
    /// or an existing certificate record (exact or wildcard). The reason is
    /// logged, not returned: a project admin must not learn what another
    /// tenant uses a name for.
    async fn ensure_available<C: ConnectionTrait>(
        &self,
        conn: &C,
        hostname: &str,
    ) -> Result<(), StatusPageError> {
        let in_use = |owner: &str| {
            info!(
                hostname,
                owner, "refused status page hostname that is already in use"
            );
            StatusPageError::HostnameInUse {
                hostname: hostname.to_string(),
            }
        };
        let unverifiable = |reason: String| StatusPageError::HostnameUnverifiable {
            hostname: hostname.to_string(),
            reason,
        };

        let settings = self
            .config_service
            .get_settings()
            .await
            .map_err(|e| unverifiable(e.to_string()))?;
        if settings.is_reserved_hostname(hostname) {
            return Err(in_use(
                "this Temps instance (console, public ingress or preview domain)",
            ));
        }
        for zone in [
            Some(settings.preview_domain.as_str()),
            settings.on_demand_tls.zone.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if inside_zone(hostname, zone) {
                return Err(in_use("a platform-managed zone"));
            }
        }

        if status_page_domains::attached_hostname_conflict(conn, hostname).await? {
            return Err(in_use("a status page"));
        }

        if let Some(owner) = self
            .config_service
            .hostname_claimed_by(hostname)
            .await
            .map_err(|e| unverifiable(e.to_string()))?
        {
            return Err(in_use(owner));
        }

        let mut certificate_names = vec![hostname.to_string()];
        if let Some((_, parent)) = hostname.split_once('.') {
            certificate_names.push(format!("*.{parent}"));
        }
        if domains::Entity::find()
            .filter(domains::Column::Domain.is_in(certificate_names))
            .one(conn)
            .await?
            .is_some()
        {
            return Err(in_use("a certificate managed under Domains"));
        }

        let Some(route_table) = self.route_table.as_ref() else {
            error!(
                hostname,
                "no route table is available to check status page hostname; refusing"
            );
            return Err(unverifiable(
                "the route table is not available in this process".to_string(),
            ));
        };
        match route_table.application_serving_host(hostname).await {
            Ok(None) => Ok(()),
            Ok(Some(owner)) => Err(in_use(&owner)),
            Err(e) => Err(unverifiable(e.to_string())),
        }
    }

    /// Where the hostname should point: the public ingress hostname when one
    /// is set, else the console hostname.
    async fn dns_target(&self) -> DnsTarget {
        let settings = match self.config_service.get_settings().await {
            Ok(settings) => settings,
            Err(e) => {
                warn!(error = %e, "could not read settings to describe status page DNS records");
                return DnsTarget::Unknown;
            }
        };
        match settings
            .public_ingress_hostname()
            .or_else(|| settings.console_hostname())
        {
            Some(host) if host.parse::<std::net::IpAddr>().is_ok() => DnsTarget::Address(host),
            Some(host) => DnsTarget::Hostname(host),
            None => DnsTarget::Unknown,
        }
    }
}

enum DnsTarget {
    Hostname(String),
    Address(String),
    Unknown,
}

fn is_verified(row: &domains::Model, hostname: &str) -> bool {
    domains::CERT_SERVING_STATUSES.contains(&row.status.as_str())
        && row.certificate.is_some()
        && row.domain.eq_ignore_ascii_case(hostname)
}

fn to_response(
    binding: &status_page_domains::Model,
    row: Option<&domains::Model>,
    published: bool,
    target: &DnsTarget,
) -> StatusPageDomainResponse {
    let method = VerificationMethod::from_stored(&binding.verification_method);
    let status = match row {
        None => StatusPageDomainStatus::Unverified,
        Some(row) if is_verified(row, &binding.hostname) => StatusPageDomainStatus::Active,
        Some(row) if row.status == "failed" => StatusPageDomainStatus::Failed,
        Some(_) => StatusPageDomainStatus::Pending,
    };
    let mut dns_records = Vec::new();
    match target {
        DnsTarget::Hostname(host) if host != &binding.hostname => {
            dns_records.push(DnsRecordInstruction {
                record_type: "CNAME".to_string(),
                name: binding.hostname.clone(),
                value: host.clone(),
                purpose: "Points the hostname at this Temps instance. An A record to the same address works too.".to_string(),
            })
        }
        DnsTarget::Address(address) => dns_records.push(DnsRecordInstruction {
            record_type: if address.contains(':') { "AAAA" } else { "A" }.to_string(),
            name: binding.hostname.clone(),
            value: address.clone(),
            purpose: "Points the hostname at this Temps instance.".to_string(),
        }),
        _ => dns_records.push(DnsRecordInstruction {
            record_type: "A".to_string(),
            name: binding.hostname.clone(),
            value: "this server's public IP address".to_string(),
            purpose: "Points the hostname at this Temps instance.".to_string(),
        }),
    }
    if method == VerificationMethod::Dns01 {
        if let Some(value) = row.and_then(|r| r.dns_challenge_value.clone()) {
            dns_records.push(DnsRecordInstruction {
                record_type: "TXT".to_string(),
                name: format!("_acme-challenge.{}", binding.hostname),
                value,
                purpose: "Proves you control the hostname (ACME DNS-01). Can be removed after verification; renewal asks for a new value.".to_string(),
            });
        }
    }
    StatusPageDomainResponse {
        id: binding.id,
        hostname: binding.hostname.clone(),
        verification_method: method,
        status,
        serving: status == StatusPageDomainStatus::Active && published,
        dns_records,
        last_error: row
            .filter(|_| status != StatusPageDomainStatus::Active)
            .and_then(|r| r.last_error.clone()),
        certificate_expires_at: row
            .filter(|_| status == StatusPageDomainStatus::Active)
            .and_then(|r| r.expiration_time),
        created_at: binding.created_at,
    }
}

fn is_unique_violation(error: &sea_orm::DbErr) -> bool {
    let message = error.to_string();
    message.contains("duplicate key") || message.contains("23505")
}

/// Verified hostnames per page, for the public snapshot.
pub(crate) async fn load_verified_hosts<C: ConnectionTrait>(
    db: &C,
    page_ids: &[i32],
) -> Result<std::collections::HashMap<i32, HashSet<String>>, sea_orm::DbErr> {
    use sea_orm::FromQueryResult;
    #[derive(FromQueryResult)]
    struct Row {
        status_page_id: i32,
        hostname: String,
    }
    let mut result: std::collections::HashMap<i32, HashSet<String>> = Default::default();
    for chunk in page_ids.chunks(1000) {
        let placeholders = (1..=chunk.len())
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            r#"
            SELECT spd.status_page_id, spd.hostname
            FROM status_page_domains spd
            JOIN domains d ON d.id = spd.domain_id
            WHERE spd.status_page_id IN ({placeholders})
              AND d.status IN ('active', 'active_renewal_failed')
              AND d.certificate IS NOT NULL
              AND lower(d.domain) = spd.hostname
            "#
        );
        let values: Vec<sea_orm::Value> = chunk.iter().map(|id| (*id).into()).collect();
        for row in Row::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .all(db)
        .await?
        {
            result
                .entry(row.status_page_id)
                .or_default()
                .insert(row.hostname);
        }
    }
    Ok(result)
}

/// Fakes for tests: an ACME issuer that writes `domains` rows directly, and a
/// route table that reports a fixed set of application hostnames.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Issues certificates by writing the `domains` row the real ACME flow
    /// would. `complete` fails for hostnames in `fail_hosts`, the way an
    /// HTTP-01 check fails while DNS does not point at this instance yet.
    pub struct FakeCertificates {
        pub db: Arc<DatabaseConnection>,
        pub fail_hosts: Mutex<HashSet<String>>,
        pub completed: Mutex<Vec<String>>,
    }

    impl FakeCertificates {
        pub fn new(db: Arc<DatabaseConnection>) -> Self {
            Self {
                db,
                fail_hosts: Mutex::new(HashSet::new()),
                completed: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl StatusPageCertificates for FakeCertificates {
        async fn begin(
            &self,
            hostname: &str,
            method: VerificationMethod,
            _acme_email: &str,
        ) -> Result<i32, String> {
            let row = domains::ActiveModel {
                domain: Set(hostname.to_string()),
                status: Set("challenge_requested".to_string()),
                verification_method: Set(method.as_str().to_string()),
                is_wildcard: Set(false),
                dns_challenge_value: Set(
                    (method == VerificationMethod::Dns01).then(|| "txt-value".to_string())
                ),
                ..Default::default()
            }
            .insert(self.db.as_ref())
            .await
            .map_err(|e| e.to_string())?;
            Ok(row.id)
        }

        async fn request_challenge(&self, _hostname: &str, _email: &str) -> Result<(), String> {
            Ok(())
        }

        async fn complete(&self, hostname: &str, _email: &str) -> Result<(), String> {
            if self
                .fail_hosts
                .lock()
                .map_err(|e| e.to_string())?
                .contains(hostname)
            {
                return Err(format!(
                    "HTTP-01 challenge for {hostname} failed: connection refused"
                ));
            }
            let row = domains::Entity::find()
                .filter(domains::Column::Domain.eq(hostname))
                .one(self.db.as_ref())
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no domains row for {hostname}"))?;
            let mut active: domains::ActiveModel = row.into();
            active.status = Set("active".to_string());
            active.certificate = Set(Some("-----BEGIN CERTIFICATE-----\ntest\n".to_string()));
            active.private_key = Set(Some("encrypted-test-key".to_string()));
            active.expiration_time = Set(Some(chrono::Utc::now() + chrono::Duration::days(90)));
            active
                .update(self.db.as_ref())
                .await
                .map_err(|e| e.to_string())?;
            self.completed
                .lock()
                .map_err(|e| e.to_string())?
                .push(hostname.to_string());
            Ok(())
        }
    }

    /// Route table that reports `serving` hostnames as used by an application.
    pub struct StubRouteTable {
        pub serving: HashSet<String>,
    }

    #[async_trait]
    impl RouteTableRefresher for StubRouteTable {
        async fn refresh_routes(&self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
            Ok(0)
        }

        async fn application_serving_host(
            &self,
            host: &str,
        ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self
                .serving
                .contains(host)
                .then(|| "a live application route".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_are_normalized() {
        assert_eq!(
            normalize_status_page_hostname("  Status.Example.COM. ").ok(),
            Some("status.example.com".to_string())
        );
        // IDNA: Unicode labels become punycode.
        assert_eq!(
            normalize_status_page_hostname("estado.bücher.example").ok(),
            Some("estado.xn--bcher-kva.example".to_string())
        );
        assert_eq!(
            normalize_status_page_hostname("xn--bcher-kva.example").ok(),
            Some("xn--bcher-kva.example".to_string())
        );
    }

    #[test]
    fn unsafe_or_ambiguous_hostnames_are_refused() {
        for input in [
            "",
            "   ",
            "*.example.com",
            "status.*.example.com",
            "127.0.0.1",
            "10.0.0.1",
            "1.2.3",
            "[::1]",
            "::1",
            "localhost",
            "example",
            "status.example.com:443",
            "https://status.example.com",
            "status.example.com/path",
            "user@status.example.com",
            "-bad.example.com",
            "bad-.example.com",
            "under_score.example.com",
            "a..example.com",
            "status example.com",
            "status.example.123",
        ] {
            assert!(
                normalize_status_page_hostname(input).is_err(),
                "{input:?} must be refused"
            );
        }
        let long_label = format!("{}.example.com", "a".repeat(64));
        assert!(normalize_status_page_hostname(&long_label).is_err());
    }

    #[tokio::test]
    async fn domain_operations_are_throttled_per_project() {
        let db = Arc::new(sea_orm::DatabaseConnection::Disconnected);
        let config = temps_config::ServerConfig::new(
            "127.0.0.1:3000".to_string(),
            "postgres://test:test@localhost/test".to_string(),
            None,
            None,
        )
        .expect("server config");
        let service = StatusPageDomainService::new(
            db.clone(),
            Arc::new(ConfigService::new(Arc::new(config), db.clone())),
            Arc::new(StatusSnapshotCache::new(db.clone())),
            Arc::new(test_support::FakeCertificates::new(db)),
            None,
        );
        for _ in 0..DOMAIN_OPERATIONS_PER_HOUR {
            service.throttle(7).expect("within budget");
        }
        assert!(matches!(
            service.throttle(7),
            Err(StatusPageError::DomainOperationsThrottled { project_id: 7, .. })
        ));
        service.throttle(8).expect("other projects are independent");
    }

    #[test]
    fn zone_membership() {
        assert!(inside_zone("app.preview.example", "preview.example"));
        assert!(inside_zone("preview.example", "*.preview.example"));
        assert!(!inside_zone("notpreview.example", "preview.example"));
        assert!(!inside_zone("status.example.com", ""));
    }
}
