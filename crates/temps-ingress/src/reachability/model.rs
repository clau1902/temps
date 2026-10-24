// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What the reachability API returns (ADR 064 D3, D4).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::registry::SetupField;

/// Who an address is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// Anyone on the internet.
    #[default]
    Public,
    /// Only devices on the operator's private network. Reserved (phase 2).
    Private,
    /// Anyone who signs in. Reserved (phase 4).
    SignedIn,
}

impl Audience {
    pub fn as_str(self) -> &'static str {
        match self {
            Audience::Public => temps_entities::audience::PUBLIC,
            Audience::Private => temps_entities::audience::PRIVATE,
            Audience::SignedIn => temps_entities::audience::SIGNED_IN,
        }
    }

    /// The stored value; an unknown one reads as the narrowest audience
    /// rather than as public.
    pub fn parse(value: &str) -> Self {
        match value {
            temps_entities::audience::PUBLIC => Audience::Public,
            temps_entities::audience::SIGNED_IN => Audience::SignedIn,
            _ => Audience::Private,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReachState {
    Reachable,
    NotReachable,
    /// Being set up; asks again shortly.
    Checking,
    /// Temps cannot tell from here.
    Unknown,
}

/// Where an address comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AddressSource {
    /// The environment's generated hostname under the preview domain.
    Generated,
    /// An environment domain.
    EnvironmentDomain,
    /// A project custom domain.
    CustomDomain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Open the *Make it reachable* flow.
    MakeReachable,
    /// Deploy the environment.
    Deploy,
    /// Add or fix a DNS record, as `detail` says.
    SetDnsRecord,
    /// Look at the tunnel or the ingress setup page.
    OpenIngress,
    /// Look at the worker.
    OpenWorker,
    /// Wait; Temps is still working on it.
    Wait,
    /// Run `command` on the machine it names.
    RunCommand,
}

/// What fixes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReachAction {
    pub kind: ActionKind,
    pub label: String,
    /// A console path, when there is a page for it.
    pub href: Option<String>,
    /// A command to run, when that is the fix.
    pub command: Option<String>,
}

impl ReachAction {
    pub fn new(kind: ActionKind, label: impl Into<String>) -> Self {
        Self {
            kind,
            label: label.into(),
            href: None,
            command: None,
        }
    }

    pub fn with_href(mut self, href: impl Into<String>) -> Self {
        self.href = Some(href.into());
        self
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }
}

/// One address of an environment and whether its audience can reach it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AddressReachability {
    #[schema(example = "myapp.example.com")]
    pub address: String,
    pub audience: Audience,
    pub source: AddressSource,
    /// The custom domain row, for a custom domain.
    pub custom_domain_id: Option<i32>,
    /// How traffic gets in: `tunnel:<name>`, `direct:<node>`, or
    /// `direct:control-plane`. `None` while nothing does.
    #[schema(example = "tunnel:homelab")]
    pub way_in: Option<String>,
    pub state: ReachState,
    #[schema(example = "No DNS record for myapp.example.com exists yet.")]
    pub reason: String,
    pub action: Option<ReachAction>,
}

/// Everything about one environment's addresses.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EnvironmentReachability {
    pub project_id: i32,
    pub environment_id: i32,
    pub environment_name: String,
    /// Whether anything is deployed to it.
    pub deployed: bool,
    /// The nodes its containers run on (`control plane` for the local one).
    pub running_on: Vec<String>,
    /// `reachable` when any address is.
    pub state: ReachState,
    /// One line for the header: why nothing reaches it, or where it is.
    pub summary: String,
    pub addresses: Vec<AddressReachability>,
    /// What *Make it reachable* would do, while no address is reachable.
    pub plan: Option<ReachabilityPlan>,
}

/// `POST …/reachability/plan`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PlanRequest {
    #[serde(default)]
    pub audience: Audience,
    /// The hostname to make reachable; Temps suggests one without it.
    pub address: Option<String>,
}

/// One way to make the address reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanOptionKind {
    /// Publish it through a tunnel whose domain covers it.
    ExistingTunnel,
    /// Point Temps-managed DNS at a direct ingress.
    DirectManagedDns,
    /// Create a tunnel with a Cloudflare account Temps already has.
    NewTunnel,
    /// Add a Cloudflare account from a pasted token, then create a tunnel.
    NewTunnelWithToken,
    /// A direct ingress, with a DNS record the user adds.
    DirectManualDns,
}

impl PlanOptionKind {
    pub fn parse(value: &str) -> Option<Self> {
        [
            PlanOptionKind::ExistingTunnel,
            PlanOptionKind::NewTunnel,
            PlanOptionKind::NewTunnelWithToken,
            PlanOptionKind::DirectManagedDns,
            PlanOptionKind::DirectManualDns,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == value)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PlanOptionKind::ExistingTunnel => "existing_tunnel",
            PlanOptionKind::DirectManagedDns => "direct_managed_dns",
            PlanOptionKind::NewTunnel => "new_tunnel",
            PlanOptionKind::NewTunnelWithToken => "new_tunnel_with_token",
            PlanOptionKind::DirectManualDns => "direct_manual_dns",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    CheckToken,
    SaveDnsProvider,
    ManageDomain,
    CreateTunnel,
    WaitForTunnel,
    AttachDomain,
    RouteHostname,
    SetDnsRecord,
    ShowDnsRecord,
    IssueCertificate,
    WaitForPublication,
    CheckReachability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PlanStep {
    pub kind: StepKind,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PlanOption {
    pub kind: PlanOptionKind,
    pub title: String,
    pub description: String,
    /// How many things the user does: answer, confirm, create.
    pub actions: u8,
    /// The first option that fits, in the ADR's preference order.
    pub recommended: bool,
    /// How traffic will get in, as in [`AddressReachability::way_in`].
    pub way_in: String,
    pub steps: Vec<PlanStep>,
    /// What the user still has to answer (ADR 058 D8 descriptor form).
    /// `address` is answered with [`ReachabilityPlan::address`] by default.
    pub fields: Vec<SetupField>,
    /// Things worth knowing before choosing it.
    pub warnings: Vec<String>,
    /// Set when the option changes what another choice decided (ADR 058
    /// D9's hostname choice): what it changes, in words. Applying it needs
    /// `confirmed: true`.
    pub confirmation: Option<String>,
    /// Permissions applying it needs on top of `projects:write`.
    pub required_permissions: Vec<String>,
}

/// Why no option fits, and what would make one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PlanBlocked {
    pub reason: String,
    pub action: Option<ReachAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReachabilityPlan {
    /// What `apply` names to act on this plan. Plans expire after 15
    /// minutes and are applied once; `None` on the plan attached to
    /// `GET …/reachability`, which is not stored.
    pub plan_id: Option<String>,
    /// A hash of everything the plan depends on: the environment's
    /// addresses and audiences, DNS providers and zones, tunnels with their
    /// domains, `auto_publish` and hostname choices, and workers' ingress
    /// modes. `apply` recomputes it and refuses a plan the cluster has
    /// moved on from.
    pub fingerprint: Option<String>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub expires_at: Option<temps_core::DBDateTime>,
    pub audience: Audience,
    /// The address asked for, or the one Temps suggests.
    pub address: Option<String>,
    /// Why the address cannot be used, when it cannot.
    pub address_error: Option<String>,
    /// In the order Temps prefers them; the first is recommended.
    pub options: Vec<PlanOption>,
    /// Set when `options` is empty.
    pub blocked: Option<PlanBlocked>,
}

/// `POST …/reachability/apply`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ApplyRequest {
    /// The plan this acts on, from `POST …/reachability/plan`.
    pub plan_id: String,
    /// Which option of the plan to carry out.
    pub option: PlanOptionKind,
    /// Answers to the option's fields by key. `address`, when given, must be
    /// the plan's; `dns_provider_id` takes a provider id,
    /// `cloudflare_api_token` the token (held in memory for the run, never
    /// stored on it).
    #[serde(default)]
    #[schema(value_type = Object)]
    pub answers: std::collections::BTreeMap<String, serde_json::Value>,
    /// The user saw and accepted the option's `confirmation`.
    #[serde(default)]
    pub confirmed: bool,
}

/// `POST …/reachability/runs/{run_id}/resume`.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct ResumeRequest {
    /// `cloudflare_api_token`, when the run had not saved the account yet.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub answers: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    /// Done, and the address is reachable.
    Succeeded,
    /// Everything was created, but the address is not reachable yet.
    Waiting,
    /// A step failed; what the run created was undone.
    Failed,
    /// The server stopped during the run, and what it had started is
    /// accounted for: resume it or undo it.
    Interrupted,
    /// The server stopped during the run; Temps is looking up what its
    /// unfinished steps made before it can be resumed or undone.
    Recovering,
    Undoing,
    Undone,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Succeeded => "succeeded",
            RunState::Waiting => "waiting",
            RunState::Failed => "failed",
            RunState::Interrupted => "interrupted",
            RunState::Recovering => "recovering",
            RunState::Undoing => "undoing",
            RunState::Undone => "undone",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "running" => RunState::Running,
            "succeeded" => RunState::Succeeded,
            "waiting" => RunState::Waiting,
            "failed" => RunState::Failed,
            "undoing" => RunState::Undoing,
            "undone" => RunState::Undone,
            "interrupted" => RunState::Interrupted,
            // Unknown reads as needing recovery, never as finished.
            _ => RunState::Recovering,
        }
    }

    /// Whether the run is still doing something.
    pub fn is_active(self) -> bool {
        matches!(self, RunState::Running | RunState::Undoing | RunState::Recovering)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStepStatus {
    Pending,
    Running,
    Done,
    Failed,
    /// Not needed: what it makes was already there.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RunStep {
    pub kind: StepKind,
    pub label: String,
    pub status: RunStepStatus,
    pub detail: Option<String>,
}

/// A hostname's ADR 058 D9 choice before a run changed it, so undo puts it
/// back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PreviousChoice {
    /// No choice: automatic selection.
    Automatic,
    /// Kept off tunnels.
    Off,
    Tunnel { connector_id: i32 },
}

impl PreviousChoice {
    pub fn from_choice(choice: Option<Option<i32>>) -> Self {
        match choice {
            None => PreviousChoice::Automatic,
            Some(None) => PreviousChoice::Off,
            Some(Some(connector_id)) => PreviousChoice::Tunnel { connector_id },
        }
    }
}

/// Something a run made, so undo removes exactly that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunResource {
    DnsProvider { id: i32, name: String },
    ManagedDomain { provider_id: i32, domain: String },
    /// An existing managed domain whose automatic DNS the run turned on.
    AutoManageEnabled { provider_id: i32, domain: String },
    Connector { id: i32, name: String },
    CustomDomain { id: i32, domain: String },
    HostnameChoice {
        hostname: String,
        connector_id: i32,
        previous: PreviousChoice,
    },
    DnsRecord {
        hostname: String,
        label: String,
        /// `A` or `AAAA`.
        record_type: String,
    },
    Certificate { domain: String },
}

impl RunResource {
    pub fn describe(&self) -> String {
        match self {
            RunResource::DnsProvider { name, .. } => format!("Cloudflare account {name}"),
            RunResource::ManagedDomain { domain, .. } => format!("Managed domain {domain}"),
            RunResource::AutoManageEnabled { domain, .. } => {
                format!("Automatic DNS for {domain}")
            }
            RunResource::Connector { name, .. } => format!("Tunnel {name}"),
            RunResource::CustomDomain { domain, .. } => format!("Domain {domain} on the project"),
            RunResource::HostnameChoice { hostname, previous, .. } => match previous {
                PreviousChoice::Automatic => format!("Publishing {hostname} through the tunnel"),
                _ => format!("Routing {hostname} through the tunnel (its earlier choice is restored on undo)"),
            },
            RunResource::DnsRecord { hostname, .. } => format!("DNS record for {hostname}"),
            RunResource::Certificate { domain } => format!("Certificate for {domain}"),
        }
    }
}

/// What a run set out to create, with what finds it again (ADR 064 D4):
/// written to the journal before the call that creates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Intent {
    /// Found by its name, which the run picks before creating it.
    DnsProvider { name: String },
    ManagedDomain { provider_id: i32, domain: String },
    AutoManageEnabled { provider_id: i32, domain: String },
    /// Found by its name; Cloudflare's tunnel is `temps-<name>-<install>`.
    Connector { name: String },
    CustomDomain { domain: String, environment_id: i32 },
    HostnameChoice {
        hostname: String,
        connector_id: i32,
        previous: PreviousChoice,
    },
    /// Found by its ADR 031 ownership marker: controller `reachability`,
    /// this project and environment.
    DnsRecord {
        hostname: String,
        label: String,
        record_type: String,
    },
    Certificate { domain: String },
}

impl Intent {
    /// The idempotency key: the same for every attempt of the step.
    pub fn key(&self, run_id: i32) -> String {
        let what = match self {
            Intent::DnsProvider { name } => format!("dns_provider/{name}"),
            Intent::ManagedDomain { provider_id, domain } => {
                format!("managed_domain/{provider_id}/{domain}")
            }
            Intent::AutoManageEnabled { provider_id, domain } => {
                format!("auto_manage/{provider_id}/{domain}")
            }
            Intent::Connector { name } => format!("connector/{name}"),
            Intent::CustomDomain { domain, environment_id } => {
                format!("custom_domain/{environment_id}/{domain}")
            }
            Intent::HostnameChoice { hostname, .. } => format!("hostname_choice/{hostname}"),
            Intent::DnsRecord { hostname, record_type, .. } => {
                format!("dns_record/{hostname}/{record_type}")
            }
            Intent::Certificate { domain } => format!("certificate/{domain}"),
        };
        format!("run-{run_id}/{what}")
    }

    pub fn describe(&self) -> String {
        match self {
            Intent::DnsProvider { name } => format!("Cloudflare account {name}"),
            Intent::ManagedDomain { domain, .. } => format!("Managed domain {domain}"),
            Intent::AutoManageEnabled { domain, .. } => format!("Automatic DNS for {domain}"),
            Intent::Connector { name } => format!("Tunnel {name}"),
            Intent::CustomDomain { domain, .. } => format!("Domain {domain} on the project"),
            Intent::HostnameChoice { hostname, .. } => {
                format!("Publishing {hostname} through the tunnel")
            }
            Intent::DnsRecord { hostname, .. } => format!("DNS record for {hostname}"),
            Intent::Certificate { domain } => format!("Certificate for {domain}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JournalState {
    /// Recorded before the call; whether it happened is not known yet.
    Pending,
    /// Created; `resource` says what.
    Done,
    /// A lookup after the call found it was never created.
    Absent,
}

/// One entry of a run's journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct JournalEntry {
    pub key: String,
    pub intent: Intent,
    pub state: JournalState,
    pub resource: Option<RunResource>,
    #[schema(value_type = String, format = DateTime)]
    pub at: temps_core::DBDateTime,
}

/// What undoing one resource did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UndoResult {
    pub resource: RunResource,
    pub label: String,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RunView {
    pub id: i32,
    pub project_id: Option<i32>,
    pub environment_id: Option<i32>,
    pub audience: Audience,
    pub address: Option<String>,
    pub option: String,
    pub plan_id: Option<String>,
    pub state: RunState,
    pub steps: Vec<RunStep>,
    /// What the run created, oldest first: the journal's `done` entries.
    pub created: Vec<RunResource>,
    /// Every step's resource, written before the call that creates it.
    pub journal: Vec<JournalEntry>,
    pub undo: Vec<UndoResult>,
    /// What went wrong or what is still missing, and what to do.
    pub error: Option<String>,
    /// The address, once it is reachable.
    pub live_url: Option<String>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: temps_core::DBDateTime,
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: temps_core::DBDateTime,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub finished_at: Option<temps_core::DBDateTime>,
}
