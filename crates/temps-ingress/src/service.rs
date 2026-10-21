// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ingress connectors: create the tunnel, hand nodes what to run, and keep
//! one DNS record per served hostname pointing at it (ADR 058).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};
use serde::{Deserialize, Serialize};
use temps_core::{DBDateTime, EncryptionService};
use temps_dns::{
    DnsProviderService, DnsRecordContent, DnsRecordRequest, DnsRecordType, ManagedDnsRecordService,
    OwnershipScope,
};
use temps_entities::{
    ingress_connector_nodes, ingress_connectors, ingress_hostname_choices, ingress_publications,
    node_public_ingress, nodes,
};
use temps_routes::CachedPeerTable;
use tracing::{debug, info, warn};
use utoipa::ToSchema;

use crate::error::IngressError;
use crate::provider::{DnsIntent, TunnelContext, TunnelHandle, TunnelProvider};
use crate::registry::{FieldKind, ProviderDeps, ProviderInfo, ProviderRegistry, ProviderSetup};
use crate::wire::{NodeConnectorSpec, NodeConnectorStatusReport, NodeConnectorsResponse};

/// The DNS controller name stamped on the records this service owns.
const DNS_CONTROLLER: &str = "ingress-connector";
const DEFAULT_EDGE_PORT: i32 = 7480;
/// Keys in `settings` that hold the setup answers. Everything else there
/// belongs to the provider, which rewrites it on every `ensure`.
const SETUP_KEY: &str = "setup";
const SETUP_SECRETS_KEY: &str = "setup_secrets";
/// Every node handed the current tunnel secret since it was last rotated.
/// When one of them stops running the tunnel the secret is rotated, so it
/// cannot keep serving the tunnel's traffic.
const TOKEN_HOLDERS_KEY: &str = "token_holders";
/// How `last_error` starts when rotating the secret failed, so a later
/// success clears only that error.
const ROTATION_ERROR: &str = "Could not revoke the tunnel secret";

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateConnectorRequest {
    /// Lowercase letters, digits and hyphens; names the tunnel `temps-<name>`.
    pub name: String,
    /// The provider's `kind`, from `GET /ingress/providers`.
    pub provider: String,
    /// Answers to the provider's setup fields, by field key. DNS provider
    /// fields take the provider's id; every other field takes a string.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub values: BTreeMap<String, serde_json::Value>,
    /// Nodes that run a connector replica.
    pub node_ids: Vec<i32>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetConnectorNodesRequest {
    pub node_ids: Vec<i32>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateConnectorRequest {
    /// Publish every worker-served hostname under the domain (true), or only
    /// hostnames chosen for this tunnel (false).
    pub auto_publish: Option<bool>,
}

/// How one hostname reaches visitors, as the operator chose it (ADR 058 D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostnameMode {
    /// The most specific tunnel whose domain covers it and that publishes
    /// automatically.
    Automatic,
    /// Through the tunnel in `connector_id`.
    Tunnel,
    /// Through no tunnel: the operator points its DNS somewhere else.
    Off,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetHostnameChoiceRequest {
    pub mode: HostnameMode,
    /// Required with mode `tunnel`.
    pub connector_id: Option<i32>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HostnameCandidate {
    pub connector_id: i32,
    pub name: String,
    pub domain: String,
    pub auto_publish: bool,
}

/// Where a hostname stands with the tunnels, and why.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HostnameView {
    pub hostname: String,
    pub mode: HostnameMode,
    /// The tunnel chosen for it, with mode `tunnel`.
    pub chosen_connector_id: Option<i32>,
    /// The tunnel that publishes it, or would once `detail` is dealt with.
    pub connector_id: Option<i32>,
    pub connector_name: Option<String>,
    /// `published`, `pending`, `error` or `not_published`.
    pub state: String,
    /// What is happening; when it is not published, why and what to do.
    pub detail: String,
    /// Whether workers serve it. A tunnel only publishes those.
    pub served_by_workers: bool,
    /// The tunnels whose domain covers it.
    pub candidates: Vec<HostnameCandidate>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub published_at: Option<DBDateTime>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectorView {
    pub id: i32,
    pub name: String,
    pub provider: String,
    pub dns_provider_id: Option<i32>,
    pub domain: String,
    pub external_id: Option<String>,
    /// Where published hostnames point.
    pub dns_target: Option<String>,
    pub status: String,
    pub last_error: Option<String>,
    pub edge_port: i32,
    pub auto_publish: bool,
    pub nodes: Vec<ConnectorNodeView>,
    pub publications: Vec<PublicationView>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: DBDateTime,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectorNodeView {
    pub node_id: i32,
    pub node_name: String,
    pub driver: Option<String>,
    pub state: String,
    pub ready_connections: Option<i32>,
    pub last_error: Option<String>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub reported_at: Option<DBDateTime>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublicationView {
    pub hostname: String,
    pub state: String,
    pub last_error: Option<String>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub published_at: Option<DBDateTime>,
}

/// A tunnel's live connections, read from the provider on request.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectorLinksView {
    /// The provider's own word for the tunnel (Cloudflare: `healthy`,
    /// `degraded`, `down`, `inactive`).
    pub status: String,
    pub links: Vec<LinkView>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LinkView {
    /// The worker holding it, when its connector could be matched.
    pub node_id: Option<i32>,
    pub node_name: Option<String>,
    /// The provider's data centre, such as `mad01`.
    pub location: Option<String>,
    pub opened_at: Option<String>,
    pub reconnecting: bool,
    pub client_version: Option<String>,
}

pub struct IngressConnectorService {
    db: Arc<DatabaseConnection>,
    encryption: Arc<EncryptionService>,
    dns_providers: Arc<DnsProviderService>,
    records: Arc<ManagedDnsRecordService>,
    peer_table: Arc<CachedPeerTable>,
    http: reqwest::Client,
    registry: ProviderRegistry,
    /// Registered by the proxy plugin, after this one; see
    /// [`Self::set_request_policy`].
    request_policy: OnceLock<Arc<temps_core::RequestPolicyGateSlot>>,
    reconcile_lock: tokio::sync::Mutex<()>,
}

impl IngressConnectorService {
    pub fn new(
        db: Arc<DatabaseConnection>,
        encryption: Arc<EncryptionService>,
        dns_providers: Arc<DnsProviderService>,
        records: Arc<ManagedDnsRecordService>,
        peer_table: Arc<CachedPeerTable>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            db,
            encryption,
            dns_providers,
            records,
            peer_table,
            http,
            registry: ProviderRegistry::builtin(),
            request_policy: OnceLock::new(),
            reconcile_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Whether a custom request policy keeps traffic on the control plane
    /// decides what workers serve, so it decides what a tunnel may publish.
    /// Until it is known nothing is published.
    pub fn set_request_policy(&self, gate: Arc<temps_core::RequestPolicyGateSlot>) {
        let _ = self.request_policy.set(gate);
    }

    // ----- provider plumbing -------------------------------------------

    fn deps(&self) -> ProviderDeps<'_> {
        ProviderDeps {
            http: &self.http,
            dns_providers: &self.dns_providers,
        }
    }

    /// The DNS zone the connector's domain lives in: the domain itself, or
    /// the managed domain above it when the tunnel is on a subdomain.
    async fn zone_of(&self, connector: &ingress_connectors::Model) -> Result<String, IngressError> {
        match self
            .dns_providers
            .find_provider_for_domain(&connector.domain)
            .await?
        {
            Some((_, managed)) => Ok(managed.domain.trim_end_matches('.').to_ascii_lowercase()),
            None => Err(IngressError::Validation(format!(
                "{} is no longer a managed domain, or under one. Add it again under DNS providers.",
                connector.domain
            ))),
        }
    }

    /// The providers this control plane offers, for the console.
    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.registry.infos()
    }

    async fn provider_for(
        &self,
        connector: &ingress_connectors::Model,
    ) -> Result<Box<dyn TunnelProvider>, IngressError> {
        let factory = self.registry.get(&connector.provider)?;
        let secrets = self.setup_secrets(connector)?;
        factory.build(&self.deps(), connector, &secrets).await
    }

    /// A provider good enough to build node specs, which need no credential.
    fn spec_provider(&self, kind: &str) -> Option<Box<dyn TunnelProvider>> {
        self.registry
            .get(kind)
            .ok()
            .map(|factory| factory.spec_provider(&self.http))
    }

    /// The connector's `secret` setup answers, decrypted.
    fn setup_secrets(
        &self,
        connector: &ingress_connectors::Model,
    ) -> Result<BTreeMap<String, String>, IngressError> {
        let Some(sealed) = connector
            .settings
            .get(SETUP_SECRETS_KEY)
            .and_then(|v| v.as_str())
        else {
            return Ok(BTreeMap::new());
        };
        let json = self
            .encryption
            .decrypt_string(sealed)
            .map_err(|e| IngressError::Encryption(e.to_string()))?;
        serde_json::from_str(&json).map_err(|e| IngressError::Encryption(e.to_string()))
    }

    /// Check the answers against the provider's fields and resolve them.
    async fn resolve_setup(
        &self,
        info: &ProviderInfo,
        values: &BTreeMap<String, serde_json::Value>,
    ) -> Result<ProviderSetup, IngressError> {
        let mut setup = ProviderSetup {
            dns_provider: None,
            domain: None,
            settings: BTreeMap::new(),
            secrets: BTreeMap::new(),
        };
        for key in values.keys() {
            if !info.fields.iter().any(|f| &f.key == key) {
                return Err(IngressError::Validation(format!(
                    "{} has no setting '{key}'.",
                    info.name
                )));
            }
        }
        // DNS providers first: a managed domain is checked against one.
        let mut fields: Vec<_> = info.fields.iter().collect();
        fields.sort_by_key(|f| !matches!(f.kind, FieldKind::DnsProvider { .. }));
        for field in fields {
            let value = values.get(&field.key).filter(|v| match v {
                serde_json::Value::Null => false,
                serde_json::Value::String(s) => !s.trim().is_empty(),
                _ => true,
            });
            let Some(value) = value else {
                if field.required {
                    return Err(IngressError::Validation(format!(
                        "{} is required.",
                        field.label
                    )));
                }
                continue;
            };
            let text = || -> Result<String, IngressError> {
                match value {
                    serde_json::Value::String(s) => Ok(s.trim().to_string()),
                    serde_json::Value::Number(n) => Ok(n.to_string()),
                    _ => Err(IngressError::Validation(format!(
                        "{} must be text.",
                        field.label
                    ))),
                }
            };
            match &field.kind {
                FieldKind::Text => {
                    setup.settings.insert(field.key.clone(), text()?);
                }
                FieldKind::Secret => {
                    setup.secrets.insert(field.key.clone(), text()?);
                }
                FieldKind::DnsProvider { provider_type } => {
                    let id: i32 = text()?.parse().map_err(|_| {
                        IngressError::Validation(format!("{} must be a DNS provider.", field.label))
                    })?;
                    let provider = self.dns_providers.get(id).await?;
                    if let Some(wanted) = provider_type {
                        if !provider.provider_type.eq_ignore_ascii_case(wanted) {
                            return Err(IngressError::Validation(format!(
                                "{} needs a {wanted} DNS provider; '{}' is {}.",
                                field.label, provider.name, provider.provider_type
                            )));
                        }
                    }
                    setup.dns_provider = Some(provider);
                }
                FieldKind::ManagedDomain {
                    allow_subdomain, ..
                } => {
                    let domain = text()?.trim_end_matches('.').to_ascii_lowercase();
                    if !is_dns_name(&domain) {
                        return Err(IngressError::Validation(format!(
                            "{domain} is not a valid domain name."
                        )));
                    }
                    let managing = self.dns_providers.find_provider_for_domain(&domain).await?;
                    if let Some((_, zone)) = &managing {
                        let zone = zone.domain.trim_end_matches('.').to_ascii_lowercase();
                        if zone != domain && !allow_subdomain {
                            return Err(IngressError::Validation(format!(
                                "{} takes a whole managed domain; use {zone}.",
                                field.label
                            )));
                        }
                    }
                    match (&setup.dns_provider, managing) {
                        (Some(chosen), Some((managing, _))) if managing.id != chosen.id => {
                            return Err(IngressError::Validation(format!(
                                "{domain} is managed by DNS provider '{}', not '{}'. Pick that provider, or move the domain.",
                                managing.name, chosen.name
                            )))
                        }
                        (Some(chosen), None) => {
                            return Err(IngressError::Validation(format!(
                                "{domain} is not a managed domain of DNS provider '{}'. Add and verify it under Settings → DNS first.",
                                chosen.name
                            )))
                        }
                        (None, None) => {
                            return Err(IngressError::Validation(format!(
                                "{domain} is not a managed domain. Add and verify it under Settings → DNS first."
                            )))
                        }
                        _ => {}
                    }
                    setup.domain = Some(domain);
                }
            }
        }
        Ok(setup)
    }

    fn handle_of(
        &self,
        connector: &ingress_connectors::Model,
    ) -> Result<Option<TunnelHandle>, IngressError> {
        let (Some(external_id), Some(secret)) =
            (&connector.external_id, &connector.secret_encrypted)
        else {
            return Ok(None);
        };
        let secret = self
            .encryption
            .decrypt_string(secret)
            .map_err(|e| IngressError::Encryption(e.to_string()))?;
        Ok(Some(TunnelHandle {
            external_id: external_id.clone(),
            settings: connector.settings.clone(),
            secret,
        }))
    }

    async fn find(&self, id: i32) -> Result<ingress_connectors::Model, IngressError> {
        ingress_connectors::Entity::find_by_id(id)
            .one(self.db.as_ref())
            .await?
            .ok_or(IngressError::NotFound(id))
    }

    // ----- admin operations --------------------------------------------

    pub async fn list(&self) -> Result<Vec<ConnectorView>, IngressError> {
        let connectors = ingress_connectors::Entity::find()
            .order_by_asc(ingress_connectors::Column::Id)
            .all(self.db.as_ref())
            .await?;
        let mut views = Vec::with_capacity(connectors.len());
        for connector in connectors {
            views.push(self.view(connector).await?);
        }
        Ok(views)
    }

    pub async fn get(&self, id: i32) -> Result<ConnectorView, IngressError> {
        let connector = self.find(id).await?;
        self.view(connector).await
    }

    async fn view(
        &self,
        connector: ingress_connectors::Model,
    ) -> Result<ConnectorView, IngressError> {
        let node_rows = ingress_connector_nodes::Entity::find()
            .filter(ingress_connector_nodes::Column::ConnectorId.eq(connector.id))
            .order_by_asc(ingress_connector_nodes::Column::NodeId)
            .all(self.db.as_ref())
            .await?;
        let names: HashMap<i32, String> = nodes::Entity::find()
            .filter(nodes::Column::Id.is_in(node_rows.iter().map(|n| n.node_id)))
            .all(self.db.as_ref())
            .await?
            .into_iter()
            .map(|n| (n.id, n.name))
            .collect();
        let publications = ingress_publications::Entity::find()
            .filter(ingress_publications::Column::ConnectorId.eq(connector.id))
            .order_by_asc(ingress_publications::Column::Hostname)
            .all(self.db.as_ref())
            .await?;
        Ok(ConnectorView {
            id: connector.id,
            name: connector.name,
            provider: connector.provider,
            dns_provider_id: connector.dns_provider_id,
            domain: connector.domain,
            external_id: connector.external_id,
            dns_target: connector
                .settings
                .get("cname_target")
                .and_then(|v| v.as_str())
                .map(String::from),
            status: connector.status,
            last_error: connector.last_error,
            edge_port: connector.edge_port,
            auto_publish: connector.auto_publish,
            nodes: node_rows
                .into_iter()
                .map(|n| ConnectorNodeView {
                    node_name: names.get(&n.node_id).cloned().unwrap_or_default(),
                    node_id: n.node_id,
                    driver: n.driver,
                    state: n.state,
                    ready_connections: n.ready_connections,
                    last_error: n.last_error,
                    reported_at: n.reported_at,
                })
                .collect(),
            publications: publications
                .into_iter()
                .map(|p| PublicationView {
                    hostname: p.hostname,
                    state: p.state,
                    last_error: p.last_error,
                    published_at: p.published_at,
                })
                .collect(),
            created_at: connector.created_at,
        })
    }

    /// The tunnel's connections as the provider sees them, each matched to
    /// the worker whose connector holds it.
    pub async fn links(&self, id: i32) -> Result<ConnectorLinksView, IngressError> {
        let connector = self.find(id).await?;
        let handle = self.handle_of(&connector)?.ok_or_else(|| {
            IngressError::Validation(
                "The tunnel is not created yet, so it has no connections.".into(),
            )
        })?;
        let health = self.provider_for(&connector).await?.health(&handle).await?;
        let node_rows = ingress_connector_nodes::Entity::find()
            .filter(ingress_connector_nodes::Column::ConnectorId.eq(id))
            .all(self.db.as_ref())
            .await?;
        let names: HashMap<i32, String> = nodes::Entity::find()
            .filter(nodes::Column::Id.is_in(node_rows.iter().map(|n| n.node_id)))
            .all(self.db.as_ref())
            .await?
            .into_iter()
            .map(|n| (n.id, n.name))
            .collect();
        let by_ref: HashMap<String, i32> = node_rows
            .into_iter()
            .filter_map(|n| n.connector_ref.map(|r| (r, n.node_id)))
            .collect();
        let mut links: Vec<LinkView> = health
            .links
            .into_iter()
            .map(|link| {
                let node_id = link
                    .connector_ref
                    .as_ref()
                    .and_then(|r| by_ref.get(r).copied());
                LinkView {
                    node_id,
                    node_name: node_id.and_then(|id| names.get(&id).cloned()),
                    location: link.location,
                    opened_at: link.opened_at,
                    reconnecting: link.reconnecting,
                    client_version: link.client_version,
                }
            })
            .collect();
        links.sort_by(|a, b| (a.node_id, &a.location).cmp(&(b.node_id, &b.location)));
        Ok(ConnectorLinksView {
            status: health.status,
            links,
        })
    }

    pub async fn create(
        self: &Arc<Self>,
        request: CreateConnectorRequest,
    ) -> Result<ConnectorView, IngressError> {
        let name = request.name.trim().to_ascii_lowercase();
        if name.is_empty()
            || name.len() > 50
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(IngressError::Validation(
                "The name must be 1-50 lowercase letters, digits or hyphens.".into(),
            ));
        }
        let factory = self.registry.get(&request.provider)?;
        let info = factory.info();
        // Whether the workers allow it comes first: no point in answers a
        // worker will refuse.
        self.validate_nodes(&request.node_ids).await?;
        let setup = self.resolve_setup(&info, &request.values).await?;
        factory.validate(&self.deps(), &setup).await?;

        let mut settings = serde_json::Map::new();
        if !setup.settings.is_empty() {
            settings.insert(SETUP_KEY.into(), serde_json::json!(setup.settings));
        }
        if !setup.secrets.is_empty() {
            let json = serde_json::to_string(&setup.secrets)
                .map_err(|e| IngressError::Encryption(e.to_string()))?;
            let sealed = self
                .encryption
                .encrypt_string(&json)
                .map_err(|e| IngressError::Encryption(e.to_string()))?;
            settings.insert(SETUP_SECRETS_KEY.into(), serde_json::Value::String(sealed));
        }
        let now = Utc::now();
        let connector = ingress_connectors::ActiveModel {
            name: Set(name),
            provider: Set(info.kind.clone()),
            dns_provider_id: Set(setup.dns_provider.as_ref().map(|p| p.id)),
            domain: Set(setup.domain.clone().unwrap_or_default()),
            settings: Set(serde_json::Value::Object(settings)),
            edge_port: Set(DEFAULT_EDGE_PORT),
            auto_publish: Set(true),
            status: Set("pending".into()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(self.db.as_ref())
        .await
        .map_err(|error| match error {
            sea_orm::DbErr::Query(_) | sea_orm::DbErr::Exec(_)
                if error.to_string().contains("ingress_connectors_name_key") =>
            {
                IngressError::Validation("A connector with this name already exists.".into())
            }
            other => IngressError::Database(other),
        })?;
        self.replace_nodes(connector.id, &request.node_ids).await?;

        let id = connector.id;
        self.ensure(connector).await?;
        let service = Arc::clone(self);
        tokio::spawn(async move { service.reconcile_all().await });
        self.get(id).await
    }

    pub async fn set_nodes(
        self: &Arc<Self>,
        id: i32,
        node_ids: Vec<i32>,
    ) -> Result<ConnectorView, IngressError> {
        self.find(id).await?;
        self.validate_nodes(&node_ids).await?;
        self.replace_nodes(id, &node_ids).await?;
        let service = Arc::clone(self);
        tokio::spawn(async move { service.reconcile_all().await });
        self.get(id).await
    }

    pub async fn update(
        self: &Arc<Self>,
        id: i32,
        request: UpdateConnectorRequest,
    ) -> Result<ConnectorView, IngressError> {
        let connector = self.find(id).await?;
        if let Some(auto_publish) = request.auto_publish {
            let mut row: ingress_connectors::ActiveModel = connector.into();
            row.auto_publish = Set(auto_publish);
            row.updated_at = Set(Utc::now());
            row.update(self.db.as_ref()).await?;
            self.reconcile_all().await;
        }
        self.get(id).await
    }

    /// Every hostname a tunnel could publish, plus every one with a choice
    /// or a publication, with where it stands.
    pub async fn hostnames(&self) -> Result<Vec<HostnameView>, IngressError> {
        let plan = self.plan().await?;
        let publications = self.publications_by_host().await?;
        let mut hosts: BTreeSet<String> = self
            .peer_table
            .public_hostnames()
            .into_iter()
            .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
            .filter(|host| !plan.candidates(host).is_empty())
            .collect();
        hosts.extend(plan.choices.keys().cloned());
        hosts.extend(publications.keys().cloned());
        Ok(hosts
            .iter()
            .map(|host| plan.view(host, publications.get(host)))
            .collect())
    }

    pub async fn hostname(&self, hostname: &str) -> Result<HostnameView, IngressError> {
        let host = normalize_host(hostname)?;
        let plan = self.plan().await?;
        let publications = self.publications_by_host().await?;
        Ok(plan.view(&host, publications.get(&host)))
    }

    /// Record the operator's choice for one hostname and apply it now.
    pub async fn set_hostname_choice(
        &self,
        hostname: &str,
        request: SetHostnameChoiceRequest,
    ) -> Result<HostnameView, IngressError> {
        let host = normalize_host(hostname)?;
        let existing = ingress_hostname_choices::Entity::find()
            .filter(ingress_hostname_choices::Column::Hostname.eq(&host))
            .one(self.db.as_ref())
            .await?;
        let connector_id = match request.mode {
            HostnameMode::Automatic => {
                if let Some(row) = existing {
                    ingress_hostname_choices::Entity::delete_by_id(row.id)
                        .exec(self.db.as_ref())
                        .await?;
                }
                self.reconcile_all().await;
                return self.hostname(&host).await;
            }
            HostnameMode::Off => None,
            HostnameMode::Tunnel => {
                let id = request.connector_id.ok_or_else(|| {
                    IngressError::Validation("Pick the tunnel to publish it through.".into())
                })?;
                let connector = self.find(id).await?;
                let zone = self.zone_of(&connector).await?;
                record_label(&host, &connector.domain, &zone)?;
                Some(id)
            }
        };
        let now = Utc::now();
        match existing {
            Some(row) => {
                let mut row: ingress_hostname_choices::ActiveModel = row.into();
                row.connector_id = Set(connector_id);
                row.updated_at = Set(now);
                row.update(self.db.as_ref()).await?;
            }
            None => {
                ingress_hostname_choices::ActiveModel {
                    hostname: Set(host.clone()),
                    connector_id: Set(connector_id),
                    created_at: Set(now),
                    updated_at: Set(now),
                    ..Default::default()
                }
                .insert(self.db.as_ref())
                .await?;
            }
        }
        self.reconcile_all().await;
        self.hostname(&host).await
    }

    async fn publications_by_host(
        &self,
    ) -> Result<HashMap<String, ingress_publications::Model>, IngressError> {
        Ok(ingress_publications::Entity::find()
            .all(self.db.as_ref())
            .await?
            .into_iter()
            .map(|p| (p.hostname.clone(), p))
            .collect())
    }

    /// Re-check the tunnel at the provider and republish now.
    pub async fn sync(self: &Arc<Self>, id: i32) -> Result<ConnectorView, IngressError> {
        let connector = self.find(id).await?;
        self.ensure(connector).await?;
        self.reconcile_all().await;
        self.get(id).await
    }

    pub async fn delete(&self, id: i32) -> Result<(), IngressError> {
        let connector = self.find(id).await?;
        let publications = ingress_publications::Entity::find()
            .filter(ingress_publications::Column::ConnectorId.eq(id))
            .all(self.db.as_ref())
            .await?;
        for publication in publications {
            if publication.state == "published" {
                self.remove_record(&connector, &publication.hostname)
                    .await?;
            }
        }
        if let Some(handle) = self.handle_of(&connector)? {
            self.provider_for(&connector).await?.delete(&handle).await?;
        }
        ingress_connectors::Entity::delete_by_id(id)
            .exec(self.db.as_ref())
            .await?;
        info!(connector_id = id, "ingress connector deleted");
        Ok(())
    }

    async fn validate_nodes(&self, node_ids: &[i32]) -> Result<(), IngressError> {
        let found = nodes::Entity::find()
            .filter(nodes::Column::Id.is_in(node_ids.iter().copied()))
            .all(self.db.as_ref())
            .await?;
        for id in node_ids {
            match found.iter().find(|n| n.id == *id) {
                None => {
                    return Err(IngressError::Validation(format!("Node {id} does not exist.")))
                }
                Some(node) if node.role != "worker" => {
                    return Err(IngressError::Validation(format!(
                        "Node '{}' is a {}, not a worker; only workers run connectors.",
                        node.name, node.role
                    )))
                }
                Some(node) if node.status != "active" => {
                    return Err(IngressError::Validation(format!(
                        "Worker '{}' is {}, so it cannot run a connector. Bring it back (its agent must be running and connected) or pick another worker.",
                        node.name, node.status
                    )))
                }
                Some(_) => {}
            }
        }
        // A tunnel exposes a worker as much as an open port does, so only a
        // worker that delegated public ingress takes one (ADR 058 D7). Agents
        // that report no mode predate the rule. The agent enforces this too.
        let modes = node_public_ingress::Entity::find()
            .filter(node_public_ingress::Column::NodeId.is_in(node_ids.iter().copied()))
            .all(self.db.as_ref())
            .await?;
        for row in modes {
            let mode = row.mode.as_deref().unwrap_or("delegated");
            if mode != "delegated" {
                let name = found
                    .iter()
                    .find(|n| n.id == row.node_id)
                    .map_or_else(|| row.node_id.to_string(), |n| n.name.clone());
                return Err(IngressError::NotAllowed(format!(
                    "Worker '{name}' does not let Temps expose it (its public ingress is '{mode}'). On that worker, run `temps agent ingress delegated` as root, then try again."
                )));
            }
        }
        Ok(())
    }

    async fn replace_nodes(&self, connector_id: i32, node_ids: &[i32]) -> Result<(), IngressError> {
        let wanted: HashSet<i32> = node_ids.iter().copied().collect();
        // Recorded before a new node can fetch the secret, so it is rotated
        // once that node leaves even if it leaves right away.
        self.add_token_holders(connector_id, &wanted).await?;
        let existing = ingress_connector_nodes::Entity::find()
            .filter(ingress_connector_nodes::Column::ConnectorId.eq(connector_id))
            .all(self.db.as_ref())
            .await?;
        for row in &existing {
            if !wanted.contains(&row.node_id) {
                ingress_connector_nodes::Entity::delete_by_id(row.id)
                    .exec(self.db.as_ref())
                    .await?;
            }
        }
        for node_id in wanted {
            if existing.iter().any(|row| row.node_id == node_id) {
                continue;
            }
            ingress_connector_nodes::ActiveModel {
                connector_id: Set(connector_id),
                node_id: Set(node_id),
                state: Set("pending".into()),
                created_at: Set(Utc::now()),
                ..Default::default()
            }
            .insert(self.db.as_ref())
            .await?;
        }
        Ok(())
    }

    /// Create or adopt the provider-side tunnel and store the outcome on the
    /// row, so a failure is visible on the connector rather than lost.
    async fn ensure(&self, connector: ingress_connectors::Model) -> Result<(), IngressError> {
        let outcome = async {
            let provider = self.provider_for(&connector).await?;
            let existing = self.handle_of(&connector)?;
            let instance_id = self.records.instance_id().await?;
            provider
                .ensure(&TunnelContext {
                    connector_id: connector.id,
                    name: &connector.name,
                    domain: &connector.domain,
                    edge_port: connector.edge_port as u16,
                    instance_id: &instance_id,
                    existing,
                })
                .await
        }
        .await;
        // Settings the service owns may have changed while the provider was
        // called (token holders); keep the stored ones.
        let current = self
            .find(connector.id)
            .await
            .unwrap_or_else(|_| connector.clone());
        let mut row: ingress_connectors::ActiveModel = connector.clone().into();
        row.updated_at = Set(Utc::now());
        match outcome {
            Ok(handle) => {
                let secret = self
                    .encryption
                    .encrypt_string(&handle.secret)
                    .map_err(|e| IngressError::Encryption(e.to_string()))?;
                row.external_id = Set(Some(handle.external_id));
                row.settings = Set(keep_setup(&current.settings, handle.settings));
                row.secret_encrypted = Set(Some(secret));
                row.status = Set("ready".into());
                row.last_error = Set(None);
                row.update(self.db.as_ref()).await?;
                Ok(())
            }
            Err(error) => {
                row.status = Set("error".into());
                row.last_error = Set(Some(error.to_string()));
                row.update(self.db.as_ref()).await?;
                Err(error)
            }
        }
    }

    // ----- secret rotation ---------------------------------------------

    async fn add_token_holders(
        &self,
        connector_id: i32,
        node_ids: &HashSet<i32>,
    ) -> Result<(), IngressError> {
        let connector = self.find(connector_id).await?;
        let Some(holders) = token_holders(&connector.settings) else {
            // Never recorded: the first reconcile starts from the nodes
            // assigned then, which includes these.
            return Ok(());
        };
        let merged: BTreeSet<i32> = holders.iter().chain(node_ids).copied().collect();
        if merged != holders {
            self.save_token_holders(connector, &merged, None).await?;
        }
        Ok(())
    }

    async fn save_token_holders(
        &self,
        connector: ingress_connectors::Model,
        holders: &BTreeSet<i32>,
        secret: Option<String>,
    ) -> Result<(), IngressError> {
        let mut settings = connector.settings.clone();
        if let serde_json::Value::Object(map) = &mut settings {
            map.insert(TOKEN_HOLDERS_KEY.into(), serde_json::json!(holders));
        }
        let rotated = secret.is_some();
        let clear_error = rotated
            && connector
                .last_error
                .as_deref()
                .is_some_and(|e| e.starts_with(ROTATION_ERROR));
        let mut row: ingress_connectors::ActiveModel = connector.into();
        row.settings = Set(settings);
        if let Some(secret) = secret {
            row.secret_encrypted = Set(Some(secret));
            row.updated_at = Set(Utc::now());
        }
        if clear_error {
            row.status = Set("ready".into());
            row.last_error = Set(None);
        }
        row.update(self.db.as_ref()).await?;
        Ok(())
    }

    /// Rotate the secret of every tunnel a node that held it no longer runs
    /// (removed from the connector, or deleted from the cluster). The new
    /// secret changes the spec revision, so the nodes that stay restart with
    /// it; the provider also cuts the departed nodes' open connections.
    async fn revoke_departed(&self) -> Result<(), IngressError> {
        let connectors = ingress_connectors::Entity::find()
            .order_by_asc(ingress_connectors::Column::Id)
            .all(self.db.as_ref())
            .await?;
        if connectors.is_empty() {
            return Ok(());
        }
        let assignments = ingress_connector_nodes::Entity::find()
            .all(self.db.as_ref())
            .await?;
        for connector in connectors {
            let rows: Vec<&ingress_connector_nodes::Model> = assignments
                .iter()
                .filter(|row| row.connector_id == connector.id)
                .collect();
            let current: BTreeSet<i32> = rows.iter().map(|row| row.node_id).collect();
            let Some(holders) = token_holders(&connector.settings) else {
                self.save_token_holders(connector, &current, None).await?;
                continue;
            };
            let departed: Vec<i32> = holders.difference(&current).copied().collect();
            if departed.is_empty() {
                if !current.is_subset(&holders) {
                    let merged = holders.union(&current).copied().collect();
                    self.save_token_holders(connector, &merged, None).await?;
                }
                continue;
            }
            let Some(handle) = self.handle_of(&connector)? else {
                // No secret was ever issued, so there is nothing to revoke.
                self.save_token_holders(connector, &current, None).await?;
                continue;
            };
            // Cut only connections no staying node holds, and only when every
            // running node has said which connection is its own.
            let keep: Option<HashSet<String>> = rows
                .iter()
                .filter(|row| row.state == "ready")
                .map(|row| row.connector_ref.clone())
                .collect();
            let outcome = async {
                self.provider_for(&connector)
                    .await?
                    .rotate_secret(&handle, keep.as_ref())
                    .await
            }
            .await;
            match outcome {
                Ok(Some(secret)) => {
                    let sealed = self
                        .encryption
                        .encrypt_string(&secret)
                        .map_err(|e| IngressError::Encryption(e.to_string()))?;
                    info!(
                        connector_id = connector.id,
                        ?departed,
                        "rotated tunnel secret after nodes left"
                    );
                    self.save_token_holders(connector, &current, Some(sealed))
                        .await?;
                }
                Ok(None) => {
                    let names = self.node_names(&departed).await?;
                    warn!(
                        connector_id = connector.id,
                        ?departed,
                        "tunnel provider cannot rotate its secret"
                    );
                    self.record_rotation_error(
                        connector,
                        format!(
                            "{ROTATION_ERROR}: this provider cannot rotate it, so {names} can still run this tunnel. Delete the connector and create it again to cut them off."
                        ),
                    )
                    .await?;
                }
                Err(error) => {
                    let names = self.node_names(&departed).await?;
                    warn!(connector_id = connector.id, ?departed, %error, "could not rotate tunnel secret");
                    self.record_rotation_error(
                        connector,
                        format!(
                            "{ROTATION_ERROR} after {names} stopped running this tunnel, so they can still run it: {error} Temps retries every minute; sync the connector to retry now."
                        ),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn record_rotation_error(
        &self,
        connector: ingress_connectors::Model,
        message: String,
    ) -> Result<(), IngressError> {
        if connector.last_error.as_deref() == Some(message.as_str()) {
            return Ok(());
        }
        let mut row: ingress_connectors::ActiveModel = connector.into();
        row.status = Set("error".into());
        row.last_error = Set(Some(message));
        row.updated_at = Set(Utc::now());
        row.update(self.db.as_ref()).await?;
        Ok(())
    }

    /// `worker 'a', node 7`: the departed nodes, by name while they exist.
    async fn node_names(&self, ids: &[i32]) -> Result<String, IngressError> {
        let found = nodes::Entity::find()
            .filter(nodes::Column::Id.is_in(ids.iter().copied()))
            .all(self.db.as_ref())
            .await?;
        Ok(ids
            .iter()
            .map(|id| {
                found
                    .iter()
                    .find(|n| n.id == *id)
                    .map_or_else(|| format!("node {id}"), |n| format!("worker '{}'", n.name))
            })
            .collect::<Vec<_>>()
            .join(", "))
    }

    // ----- publications ------------------------------------------------

    /// What the tunnels should publish right now, from the connectors, the
    /// operator's choices and what workers serve.
    async fn plan(&self) -> Result<Plan, IngressError> {
        let models = ingress_connectors::Entity::find()
            .order_by_asc(ingress_connectors::Column::Id)
            .all(self.db.as_ref())
            .await?;
        let with_nodes: HashSet<i32> = ingress_connector_nodes::Entity::find()
            .all(self.db.as_ref())
            .await?
            .into_iter()
            .map(|row| row.connector_id)
            .collect();
        let mut connectors = Vec::with_capacity(models.len());
        for model in models {
            let zone = match self.zone_of(&model).await {
                Ok(zone) => Some(zone),
                Err(error) => {
                    debug!(connector_id = model.id, %error, "no zone for ingress connector");
                    None
                }
            };
            connectors.push(PlannedConnector {
                has_nodes: with_nodes.contains(&model.id),
                zone,
                model,
            });
        }
        let choices = ingress_hostname_choices::Entity::find()
            .all(self.db.as_ref())
            .await?
            .into_iter()
            .map(|choice| (choice.hostname, choice.connector_id))
            .collect();
        let gate = self.request_policy.get();
        let routes = temps_routes::route_sync::worker_public_routes(
            self.db.as_ref(),
            &self.peer_table,
            gate.is_none_or(|gate| !gate.supports_worker_ingress()),
        )
        .await
        .map_err(|(_, error)| IngressError::Database(error))?;
        let mut blocking = routes.blocking_policies;
        if gate.is_none() {
            blocking = vec!["the control plane is still starting".into()];
        }
        Ok(Plan {
            connectors,
            choices,
            served: routes
                .routes
                .into_iter()
                .map(|route| route.host.trim_end_matches('.').to_ascii_lowercase())
                .collect(),
            blocking,
        })
    }

    /// Bring every tunnel's DNS records in line with the plan.
    pub async fn reconcile_all(&self) {
        let _guard = self.reconcile_lock.lock().await;
        if let Err(error) = self.reconcile().await {
            warn!(%error, "ingress connector reconcile failed");
        }
    }

    async fn reconcile(&self) -> Result<(), IngressError> {
        self.revoke_departed().await?;
        let plan = self.plan().await?;
        let rows = ingress_publications::Entity::find()
            .all(self.db.as_ref())
            .await?;
        if plan.connectors.is_empty() && rows.is_empty() {
            return Ok(());
        }
        let desired: HashMap<&str, i32> = plan
            .served
            .iter()
            .filter_map(|host| match plan.decide(host) {
                Decision::Publish(id) => Some((host.as_str(), id)),
                Decision::Skip { .. } => None,
            })
            .collect();

        // Withdraw first: a hostname moving between tunnels frees its row,
        // and its record is replaced in place by the new tunnel's.
        let mut kept: HashMap<String, ingress_publications::Model> = HashMap::new();
        for row in rows {
            match desired.get(row.hostname.as_str()) {
                Some(id) if *id == row.connector_id => {
                    kept.insert(row.hostname.clone(), row);
                }
                Some(id) => {
                    info!(host = %row.hostname, from = row.connector_id, to = id, "moving hostname to another tunnel");
                    ingress_publications::Entity::delete_by_id(row.id)
                        .exec(self.db.as_ref())
                        .await?;
                }
                None => {
                    let Some(connector) = plan.connector(row.connector_id) else {
                        continue;
                    };
                    // A tunnel in trouble keeps its records: a provider outage
                    // must not unpublish everything it carries.
                    if connector.model.status != "ready" || connector.zone.is_none() {
                        continue;
                    }
                    if row.state == "published" {
                        if let Ok(provider) = self.provider_for(&connector.model).await {
                            if let Some(handle) = self.handle_of(&connector.model)? {
                                if let Err(error) = provider.unpublish(&handle, &row.hostname).await
                                {
                                    warn!(host = %row.hostname, %error, "tunnel provider refused to unpublish");
                                }
                            }
                        }
                        if let Err(error) =
                            self.remove_record(&connector.model, &row.hostname).await
                        {
                            warn!(host = %row.hostname, %error, "could not remove tunnel record");
                            continue;
                        }
                    }
                    ingress_publications::Entity::delete_by_id(row.id)
                        .exec(self.db.as_ref())
                        .await?;
                    info!(connector_id = row.connector_id, host = %row.hostname, "unpublished hostname");
                }
            }
        }

        let mut providers: HashMap<i32, Result<Box<dyn TunnelProvider>, String>> = HashMap::new();
        for (host, id) in desired {
            let existing = kept.get(host);
            if existing.is_some_and(|row| row.state == "published") {
                continue;
            }
            let Some(connector) = plan.connector(id) else {
                continue;
            };
            let Some(handle) = self.handle_of(&connector.model)? else {
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(slot) = providers.entry(id) {
                slot.insert(
                    self.provider_for(&connector.model)
                        .await
                        .map_err(|error| error.to_string()),
                );
            }
            let outcome = match &providers[&id] {
                Ok(provider) => {
                    self.publish(&connector.model, provider.as_ref(), &handle, host)
                        .await
                }
                Err(error) => Err(IngressError::Provider(error.clone())),
            };
            self.save_publication(id, host, existing, outcome).await?;
        }
        Ok(())
    }

    async fn publish(
        &self,
        connector: &ingress_connectors::Model,
        provider: &dyn TunnelProvider,
        handle: &TunnelHandle,
        host: &str,
    ) -> Result<(), IngressError> {
        let zone = self.zone_of(connector).await?;
        let label = record_label(host, &connector.domain, &zone)?;
        match provider.publish(handle, host).await? {
            DnsIntent::Cname { target, proxied } => {
                self.records
                    .set_managed_record(
                        host,
                        DnsRecordRequest {
                            name: label,
                            content: DnsRecordContent::CNAME { target },
                            ttl: Some(1),
                            proxied,
                        },
                        Some(proxied),
                        OwnershipScope::for_controller(DNS_CONTROLLER),
                    )
                    .await
                    .map_err(|error| {
                        IngressError::Provider(format!(
                            "Could not point {host} at the tunnel: {error}. If another Temps automation \
                             (generated hostnames or domain delivery) already manages this record, \
                             turn that off for {} first.",
                            connector.domain
                        ))
                    })?;
            }
            DnsIntent::None => {}
        }
        info!(connector_id = connector.id, %host, "published hostname through tunnel");
        Ok(())
    }

    async fn remove_record(
        &self,
        connector: &ingress_connectors::Model,
        host: &str,
    ) -> Result<(), IngressError> {
        let Ok(zone) = self.zone_of(connector).await else {
            return Ok(());
        };
        let Ok(label) = record_label(host, &connector.domain, &zone) else {
            return Ok(());
        };
        match self
            .records
            .remove_managed_record(
                host,
                &label,
                DnsRecordType::CNAME,
                OwnershipScope::for_controller(DNS_CONTROLLER),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(temps_dns::DnsError::RecordNotFound(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn save_publication(
        &self,
        connector_id: i32,
        host: &str,
        existing: Option<&ingress_publications::Model>,
        outcome: Result<(), IngressError>,
    ) -> Result<(), IngressError> {
        let now = Utc::now();
        let (state, error, published_at) = match outcome {
            Ok(()) => ("published", None, Some(now)),
            Err(error) => {
                debug!(%host, %error, "publication failed");
                ("error", Some(error.to_string()), None)
            }
        };
        match existing {
            Some(row) => {
                let mut row: ingress_publications::ActiveModel = row.clone().into();
                row.state = Set(state.into());
                row.last_error = Set(error);
                row.published_at = Set(published_at);
                row.updated_at = Set(now);
                row.update(self.db.as_ref()).await?;
            }
            None => {
                ingress_publications::ActiveModel {
                    connector_id: Set(connector_id),
                    hostname: Set(host.into()),
                    state: Set(state.into()),
                    last_error: Set(error),
                    published_at: Set(published_at),
                    created_at: Set(now),
                    updated_at: Set(now),
                    ..Default::default()
                }
                .insert(self.db.as_ref())
                .await?;
            }
        }
        Ok(())
    }

    // ----- node side ---------------------------------------------------

    /// Whether `node_id` runs any connector; its route snapshot then carries
    /// the public routes its tunnel origin listener serves.
    pub async fn node_has_connectors(&self, node_id: i32) -> Result<bool, IngressError> {
        Ok(ingress_connector_nodes::Entity::find()
            .filter(ingress_connector_nodes::Column::NodeId.eq(node_id))
            .one(self.db.as_ref())
            .await?
            .is_some())
    }

    pub async fn node_specs(&self, node_id: i32) -> Result<NodeConnectorsResponse, IngressError> {
        let assigned: Vec<i32> = ingress_connector_nodes::Entity::find()
            .filter(ingress_connector_nodes::Column::NodeId.eq(node_id))
            .all(self.db.as_ref())
            .await?
            .into_iter()
            .map(|row| row.connector_id)
            .collect();
        let connectors = ingress_connectors::Entity::find()
            .filter(ingress_connectors::Column::Id.is_in(assigned))
            .all(self.db.as_ref())
            .await?;
        let mut specs: Vec<NodeConnectorSpec> = Vec::new();
        for connector in connectors {
            let Some(handle) = self.handle_of(&connector)? else {
                continue;
            };
            let Some(provider) = self.spec_provider(&connector.provider) else {
                continue;
            };
            specs.push(provider.connector_spec(connector.id, &handle, connector.edge_port as u16));
        }
        Ok(NodeConnectorsResponse { connectors: specs })
    }

    pub async fn record_status(
        &self,
        node_id: i32,
        report: NodeConnectorStatusReport,
    ) -> Result<(), IngressError> {
        let now = Utc::now();
        for status in report.reports {
            let Some(row) = ingress_connector_nodes::Entity::find()
                .filter(ingress_connector_nodes::Column::NodeId.eq(node_id))
                .filter(ingress_connector_nodes::Column::ConnectorId.eq(status.connector_id))
                .one(self.db.as_ref())
                .await?
            else {
                continue;
            };
            let mut row: ingress_connector_nodes::ActiveModel = row.into();
            row.driver = Set(status.driver);
            row.state = Set(status.state);
            row.ready_connections = Set(status.ready_connections);
            row.connector_ref = Set(status.connector_ref);
            row.last_error = Set(status.last_error);
            row.reported_at = Set(Some(now));
            row.update(self.db.as_ref()).await?;
        }
        Ok(())
    }

    /// Reconcile on every route-table change and once a minute.
    pub fn start_reconciler(self: &Arc<Self>) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let notifier = service.peer_table.generation_notifier();
            loop {
                tokio::select! {
                    _ = notifier.notified() => {
                        // Let a burst of route changes settle.
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
                service.reconcile_all().await;
            }
        });
    }
}

/// The record name of `host` inside `domain`. Cloudflare's free certificate
/// covers one level under the zone, and tunnel records are always proxied,
/// so deeper names are refused with what to use instead.
fn record_label(host: &str, domain: &str, zone: &str) -> Result<String, IngressError> {
    if !covers(domain, zone, host) {
        return Err(IngressError::Validation(format!(
            "{host} is not under {domain}"
        )));
    }
    let label = host
        .strip_suffix(&format!(".{zone}"))
        .ok_or_else(|| IngressError::Validation(format!("{host} is not under {zone}")))?;
    // On the zone itself, keep hostnames where Cloudflare's free certificate
    // reaches. A tunnel on a subdomain was chosen knowing that it does not
    // (the console says so), so its hostnames may go deeper.
    if domain == zone && label.contains('.') {
        return Err(IngressError::Validation(format!(
            "{host} is more than one level below {zone}. Cloudflare's free certificate only \
             covers *.{zone}, so use a hostname like {}.{zone}.",
            label.replace('.', "-")
        )));
    }
    Ok(label.to_string())
}

/// Whether a tunnel on `domain` (in `zone`) publishes `host`: anything under
/// the domain, and a subdomain itself. Never the zone apex.
fn covers(domain: &str, zone: &str, host: &str) -> bool {
    host.ends_with(&format!(".{domain}")) || (host == domain && domain != zone)
}

struct PlannedConnector {
    model: ingress_connectors::Model,
    /// `None` when its domain is no longer managed; it then covers nothing.
    zone: Option<String>,
    has_nodes: bool,
}

struct Plan {
    connectors: Vec<PlannedConnector>,
    choices: HashMap<String, Option<i32>>,
    /// Hostnames workers serve, which are the only ones a tunnel can carry.
    served: HashSet<String>,
    /// Cluster-wide policies that keep every hostname on the control plane.
    blocking: Vec<String>,
}

enum Decision {
    Publish(i32),
    Skip { connector: Option<i32>, why: String },
}

impl Plan {
    fn connector(&self, id: i32) -> Option<&PlannedConnector> {
        self.connectors.iter().find(|c| c.model.id == id)
    }

    fn candidates(&self, host: &str) -> Vec<&PlannedConnector> {
        self.connectors
            .iter()
            .filter(|c| {
                c.zone
                    .as_deref()
                    .is_some_and(|zone| covers(&c.model.domain, zone, host))
            })
            .collect()
    }

    /// Which tunnel publishes `host`. A choice wins; otherwise the most
    /// specific automatic tunnel covering it, the oldest on a tie, so two
    /// tunnels never fight over a record.
    fn decide(&self, host: &str) -> Decision {
        let candidates = self.candidates(host);
        let chosen = match self.choices.get(host) {
            Some(None) => {
                return Decision::Skip {
                    connector: None,
                    why: "Kept off tunnels by choice. Point its DNS where it should go.".into(),
                }
            }
            Some(Some(id)) => match self.connector(*id) {
                Some(connector) => connector,
                None => {
                    return Decision::Skip {
                        connector: None,
                        why: "The tunnel chosen for it no longer exists.".into(),
                    }
                }
            },
            None => {
                let automatic =
                    candidates
                        .iter()
                        .filter(|c| c.model.auto_publish)
                        .max_by(|a, b| {
                            a.model
                                .domain
                                .len()
                                .cmp(&b.model.domain.len())
                                .then(b.model.id.cmp(&a.model.id))
                        });
                match (automatic, candidates.first()) {
                    (Some(connector), _) => *connector,
                    (None, Some(connector)) => {
                        return Decision::Skip {
                            connector: None,
                            why: format!(
                                "Tunnel '{}' covers it but only publishes hostnames chosen for it. Choose that tunnel for this hostname to publish it.",
                                connector.model.name
                            ),
                        }
                    }
                    (None, None) => {
                        return Decision::Skip {
                            connector: None,
                            why: "No tunnel's domain covers it.".into(),
                        }
                    }
                }
            }
        };
        let id = chosen.model.id;
        let name = &chosen.model.name;
        let skip = |why: String| Decision::Skip {
            connector: Some(id),
            why,
        };
        let Some(zone) = chosen.zone.as_deref() else {
            return skip(format!(
                "Tunnel '{name}' is on {}, which is no longer a managed domain. Add it again under DNS providers.",
                chosen.model.domain
            ));
        };
        if let Err(error) = record_label(host, &chosen.model.domain, zone) {
            return skip(format!("Tunnel '{name}' cannot publish it: {error}"));
        }
        if chosen.model.status != "ready" {
            return skip(match &chosen.model.last_error {
                Some(error) => format!("Tunnel '{name}' is not ready: {error}"),
                None => format!("Tunnel '{name}' is not ready yet."),
            });
        }
        if !chosen.has_nodes {
            return skip(format!(
                "Tunnel '{name}' runs on no worker. Run it on one from the worker's page."
            ));
        }
        if !self.served.contains(host) {
            return skip(if self.blocking.is_empty() {
                "Workers do not serve it, so a tunnel cannot carry it: it runs on the control plane, or needs a redirect, wake-up, attack mode, a security policy or path routing, which only the control plane applies.".into()
            } else {
                format!(
                    "Workers serve no hostnames while {}, so a tunnel cannot carry it.",
                    self.blocking.join(", ")
                )
            });
        }
        Decision::Publish(id)
    }

    fn view(&self, host: &str, publication: Option<&ingress_publications::Model>) -> HostnameView {
        let (mode, chosen_connector_id) = match self.choices.get(host) {
            None => (HostnameMode::Automatic, None),
            Some(None) => (HostnameMode::Off, None),
            Some(Some(id)) => (HostnameMode::Tunnel, Some(*id)),
        };
        let name_of = |id: i32| self.connector(id).map(|c| c.model.name.clone());
        let (connector_id, state, detail) = match self.decide(host) {
            Decision::Publish(id) => {
                let name = name_of(id).unwrap_or_default();
                match publication.filter(|p| p.connector_id == id) {
                    Some(p) if p.state == "published" => (
                        Some(id),
                        "published",
                        format!("Published through tunnel '{name}'. Cloudflare serves HTTPS."),
                    ),
                    Some(p) if p.state == "error" => (
                        Some(id),
                        "error",
                        p.last_error
                            .clone()
                            .unwrap_or_else(|| "Publishing failed.".into()),
                    ),
                    _ => (
                        Some(id),
                        "pending",
                        format!("Being published through tunnel '{name}'."),
                    ),
                }
            }
            Decision::Skip { connector, why } => match publication {
                Some(p) if p.state == "published" => (
                    Some(p.connector_id),
                    "published",
                    format!(
                        "{why} Its record through tunnel '{}' stays until that is resolved.",
                        name_of(p.connector_id).unwrap_or_default()
                    ),
                ),
                _ => (connector, "not_published", why),
            },
        };
        HostnameView {
            hostname: host.to_string(),
            mode,
            chosen_connector_id,
            connector_name: connector_id.and_then(name_of),
            connector_id,
            state: state.into(),
            detail,
            served_by_workers: self.served.contains(host),
            candidates: self
                .candidates(host)
                .into_iter()
                .map(|c| HostnameCandidate {
                    connector_id: c.model.id,
                    name: c.model.name.clone(),
                    domain: c.model.domain.clone(),
                    auto_publish: c.model.auto_publish,
                })
                .collect(),
            published_at: publication.and_then(|p| p.published_at),
        }
    }
}

fn is_dns_name(name: &str) -> bool {
    name.len() <= 253
        && name.split('.').count() >= 2
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn normalize_host(hostname: &str) -> Result<String, IngressError> {
    let host = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
    if !is_dns_name(&host) {
        return Err(IngressError::Validation(format!(
            "{hostname} is not a valid hostname."
        )));
    }
    Ok(host)
}

/// The nodes recorded as holding the current secret; `None` before the
/// first reconcile records them.
fn token_holders(settings: &serde_json::Value) -> Option<BTreeSet<i32>> {
    serde_json::from_value(settings.get(TOKEN_HOLDERS_KEY)?.clone()).ok()
}

/// The provider's new settings plus the keys the service owns: the setup
/// answers and the token holders.
fn keep_setup(old: &serde_json::Value, new: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut merged) = new else {
        return new;
    };
    for key in [SETUP_KEY, SETUP_SECRETS_KEY, TOKEN_HOLDERS_KEY] {
        if let Some(value) = old.get(key) {
            merged.insert(key.into(), value.clone());
        }
    }
    serde_json::Value::Object(merged)
}
