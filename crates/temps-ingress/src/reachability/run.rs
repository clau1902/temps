// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A *Make it reachable* run: the plan's steps in order, every resource it
//! creates journaled, and undo of exactly those, newest first (ADR 064 D4).
//!
//! The steps used to run in the browser (`web/src/components/nodes/
//! ingress-setup.ts`); here they survive a closed tab and serve the CLI and
//! AI tools too. The run talks to the rest of Temps only through [`RunOps`],
//! and reports through [`RunSink`] after every change, so the bookkeeping is
//! tested without a database or Cloudflare.
//!
//! Crash safety: before a call that creates something, the run writes a
//! `pending` [`JournalEntry`] whose key ([`Intent::key`]) is enough to find
//! the resource again; after it, `done` with the resource. If the pending
//! write fails, the call is not made. A retry or a resume reuses the key
//! and looks the resource up before creating it again, and so does the
//! service's reconciler for a run Temps stopped in the middle of.
//!
//! - A step that fails undoes what the run created and ends it `failed`,
//!   as the browser flow did.
//! - A step that waits too long (DNS, publication, reachability) ends it
//!   `waiting` with everything kept: nothing is wrong, it is not live yet.
//! - Undo never touches what the run found already there; it is not in the
//!   journal. A hostname choice the run changed is put back as it was.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::model::{
    AddressReachability, Intent, JournalEntry, JournalState, PlanOptionKind, PlanStep,
    PreviousChoice, ReachState, RunResource, RunState, RunStep, RunStepStatus, StepKind,
    UndoResult,
};
use super::plan::NamedId;
use super::token_check::TokenCheckResult;

/// A managed zone's state under one DNS provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneState {
    pub auto_manage: bool,
    pub verified: bool,
}

/// A tunnel as the run sees it while waiting for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelStatus {
    pub status: String,
    pub last_error: Option<String>,
    /// `(node id, state, ready connections, last error)` per replica.
    pub replicas: Vec<(i32, String, Option<i32>, Option<String>)>,
}

/// Everything a run does to the rest of Temps.
#[async_trait]
pub trait RunOps: Send + Sync {
    async fn check_token(&self, token: &str, zone: &str) -> TokenCheckResult;
    /// Finds what `intent` describes, if it exists: how a pending journal
    /// entry is resolved.
    async fn find(&self, intent: &Intent) -> Result<Option<RunResource>, String>;
    /// Saves a Cloudflare DNS provider under `name`; returns its id.
    async fn save_dns_provider(&self, name: &str, zone: &str, token: &str) -> Result<i32, String>;
    async fn delete_dns_provider(&self, id: i32) -> Result<(), String>;
    async fn managed_zone(&self, provider_id: i32, zone: &str)
        -> Result<Option<ZoneState>, String>;
    async fn add_managed_zone(&self, provider_id: i32, zone: &str) -> Result<(), String>;
    async fn set_auto_manage(&self, provider_id: i32, zone: &str, on: bool) -> Result<(), String>;
    async fn verify_zone(&self, provider_id: i32, zone: &str) -> Result<bool, String>;
    async fn remove_managed_zone(&self, provider_id: i32, zone: &str) -> Result<(), String>;
    /// Creates a tunnel that publishes only the hostnames chosen for it.
    /// On failure, the id of a tunnel that was saved anyway.
    async fn create_tunnel(
        &self,
        name: &str,
        provider_id: i32,
        domain: &str,
        node_ids: &[i32],
    ) -> Result<(i32, TunnelStatus), (Option<i32>, String)>;
    async fn tunnel_status(&self, id: i32) -> Result<TunnelStatus, String>;
    async fn delete_tunnel(&self, id: i32) -> Result<(), String>;
    /// The custom domain with this hostname: `(id, environment id)`.
    async fn find_custom_domain(&self, host: &str) -> Result<Option<(i32, i32)>, String>;
    async fn attach_domain(
        &self,
        project_id: i32,
        environment_id: i32,
        host: &str,
    ) -> Result<i32, String>;
    async fn detach_domain(&self, id: i32) -> Result<(), String>;
    /// See `IngressConnectorService::hostname_choice`.
    async fn hostname_choice(&self, host: &str) -> Result<Option<Option<i32>>, String>;
    async fn route_hostname(&self, host: &str, connector_id: i32) -> Result<(), String>;
    /// Puts a hostname's choice back as it was before the run.
    async fn restore_hostname_choice(
        &self,
        host: &str,
        previous: PreviousChoice,
    ) -> Result<(), String>;
    /// ADR 058 D9's state and detail.
    async fn hostname_state(&self, host: &str) -> Result<(String, String), String>;
    /// Creates the record; returns its label, for undo.
    async fn set_dns_record(&self, host: &str, ip: IpAddr) -> Result<String, String>;
    async fn remove_dns_record(
        &self,
        host: &str,
        label: &str,
        record_type: &str,
    ) -> Result<(), String>;
    async fn certificate_exists(&self, host: &str) -> Result<bool, String>;
    /// Creates the certificate's record; issuing it is [`Self::finish_certificate`].
    async fn start_certificate(&self, host: &str) -> Result<(), String>;
    async fn finish_certificate(&self, host: &str, email: &str) -> Result<(), String>;
    async fn delete_certificate(&self, host: &str) -> Result<(), String>;
    async fn resolve(&self, host: &str) -> Vec<IpAddr>;
    async fn reachability(&self, host: &str) -> AddressReachability;
    async fn sleep(&self, duration: Duration);
}

/// Where a run's progress goes after every change.
#[async_trait]
pub trait RunSink: Send + Sync {
    /// `Err` when it could not be stored. Before a creating call that means
    /// the call is not made.
    async fn save(&self, record: &RunRecord) -> Result<(), String>;
}

/// How long the run waits for what happens elsewhere.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    /// For a new tunnel's worker to connect (the browser flow's 3 minutes).
    pub connect: Duration,
    /// For the reconciler to publish a hostname; it runs every 60 s.
    pub publish: Duration,
    /// For a DNS record the user adds.
    pub dns: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(2),
            connect: Duration::from_secs(180),
            publish: Duration::from_secs(90),
            dns: Duration::from_secs(600),
        }
    }
}

/// A new tunnel the run creates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTunnel {
    /// `None` until the run saves the pasted token as a provider.
    pub provider_id: Option<i32>,
    /// The name the pasted token's provider is saved under, picked unused
    /// when the run starts so a lookup finds it again.
    #[serde(default)]
    pub provider_name: String,
    pub zone: String,
    pub domain: String,
    /// Unused when the run starts; Cloudflare's tunnel is
    /// `temps-<name>-<install>`.
    pub name: String,
    pub worker: NamedId,
}

/// What a run is asked to do, resolved from the plan when it starts and
/// stored with the run so a resume does the same.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInput {
    pub run_id: i32,
    pub project_id: i32,
    pub environment_id: i32,
    pub environment_name: String,
    pub option: PlanOptionKind,
    pub address: String,
    pub steps: Vec<PlanStep>,
    /// An existing tunnel to publish through.
    pub tunnel: Option<NamedId>,
    pub new_tunnel: Option<NewTunnel>,
    /// The user confirmed changing the hostname's tunnel choice.
    #[serde(default)]
    pub route_confirmed: bool,
    /// In memory for the run only; never stored.
    #[serde(skip)]
    pub token: Option<String>,
    /// A direct ingress and the address DNS points at.
    pub direct: Option<(String, IpAddr)>,
    /// For Let's Encrypt; the signed-in user's.
    pub acme_email: Option<String>,
}

/// The run as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub state: RunState,
    pub steps: Vec<RunStep>,
    pub journal: Vec<JournalEntry>,
    pub undo: Vec<UndoResult>,
    pub error: Option<String>,
    pub live_url: Option<String>,
}

impl RunRecord {
    pub fn new(steps: &[PlanStep]) -> Self {
        Self {
            state: RunState::Running,
            steps: steps
                .iter()
                .map(|s| RunStep {
                    kind: s.kind,
                    label: s.label.clone(),
                    status: RunStepStatus::Pending,
                    detail: None,
                })
                .collect(),
            journal: Vec::new(),
            undo: Vec::new(),
            error: None,
            live_url: None,
        }
    }

    /// What the run created, oldest first.
    pub fn created(&self) -> Vec<RunResource> {
        created(&self.journal)
    }

    /// Whether a journal entry is still `pending`: a call whose outcome is
    /// not known. Undo and resume wait for the reconciler to resolve it.
    pub fn has_pending(&self) -> bool {
        self.journal
            .iter()
            .any(|e| e.state == JournalState::Pending)
    }
}

/// The `done` entries' resources, oldest first.
pub fn created(journal: &[JournalEntry]) -> Vec<RunResource> {
    journal
        .iter()
        .filter(|e| e.state == JournalState::Done)
        .filter_map(|e| e.resource.clone())
        .collect()
}

/// Resolve every `pending` entry by looking its resource up. `Err` keeps
/// an entry pending: Temps could not tell, so it does not guess.
pub async fn reconcile_journal(
    journal: &mut [JournalEntry],
    ops: &dyn RunOps,
) -> Result<usize, String> {
    let mut resolved = 0;
    for entry in journal.iter_mut() {
        if entry.state != JournalState::Pending {
            continue;
        }
        match ops.find(&entry.intent).await? {
            Some(resource) => {
                entry.state = JournalState::Done;
                entry.resource = Some(resource);
            }
            None => entry.state = JournalState::Absent,
        }
        entry.at = chrono::Utc::now();
        resolved += 1;
    }
    Ok(resolved)
}

enum Outcome {
    Done(Option<String>),
    Skipped(String),
    Failed(String),
    /// Nothing is wrong, but it is not there yet.
    Stalled(String),
}

/// Whether a journaled step has to make its call.
enum Begin {
    /// An earlier attempt created it already.
    Have(RunResource),
    /// The pending entry is stored; make the call.
    Go,
}

struct Context {
    provider_id: Option<i32>,
    connector_id: Option<i32>,
}

struct Runner<'a> {
    input: &'a RunInput,
    ops: &'a dyn RunOps,
    sink: &'a dyn RunSink,
    timing: Timing,
    record: RunRecord,
    ctx: Context,
}

fn record_type(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(_) => "A",
        IpAddr::V6(_) => "AAAA",
    }
}

impl Runner<'_> {
    /// Progress that is not a journal entry: a failed save is logged, and
    /// the next one carries it.
    async fn save(&self) {
        if let Err(error) = self.sink.save(&self.record).await {
            tracing::warn!(run = self.input.run_id, "could not save reachability run progress: {error}");
        }
    }

    async fn set(&mut self, index: usize, status: RunStepStatus, detail: Option<String>) {
        if let Some(step) = self.record.steps.get_mut(index) {
            step.status = status;
            step.detail = detail;
        }
        self.save().await;
    }

    fn known(&self, intent: &Intent) -> bool {
        let key = intent.key(self.input.run_id);
        self.record
            .journal
            .iter()
            .any(|e| e.key == key && e.state != JournalState::Absent)
    }

    /// Before a creating call: reuse what an earlier attempt made, else
    /// store a pending entry. Nothing is called when that cannot be stored.
    async fn begin(&mut self, intent: &Intent) -> Result<Begin, String> {
        let key = intent.key(self.input.run_id);
        let now = chrono::Utc::now();
        match self.record.journal.iter().position(|e| e.key == key) {
            Some(at) => {
                let entry = &self.record.journal[at];
                match (entry.state, entry.resource.clone()) {
                    (JournalState::Done, Some(resource)) => return Ok(Begin::Have(resource)),
                    (JournalState::Pending, _) => {
                        if let Some(resource) = self.ops.find(intent).await.map_err(|error| {
                            format!(
                                "An earlier attempt at this step may have made {} already, and \
                                 Temps could not check: {error} Try again in a minute.",
                                intent.describe()
                            )
                        })? {
                            self.finish(&key, Some(resource.clone())).await;
                            return Ok(Begin::Have(resource));
                        }
                    }
                    _ => {}
                }
                let entry = &mut self.record.journal[at];
                entry.state = JournalState::Pending;
                entry.resource = None;
                entry.at = now;
            }
            None => self.record.journal.push(JournalEntry {
                key: key.clone(),
                intent: intent.clone(),
                state: JournalState::Pending,
                resource: None,
                at: now,
            }),
        }
        self.sink.save(&self.record).await.map_err(|error| {
            format!(
                "Temps could not record that it is about to create {}, so it did not: {error}",
                intent.describe()
            )
        })?;
        Ok(Begin::Go)
    }

    /// After the call: `Some` what it made, `None` nothing.
    async fn finish(&mut self, key: &str, resource: Option<RunResource>) {
        if let Some(entry) = self.record.journal.iter_mut().find(|e| e.key == key) {
            entry.state = if resource.is_some() {
                JournalState::Done
            } else {
                JournalState::Absent
            };
            entry.resource = resource;
            entry.at = chrono::Utc::now();
        }
        self.save().await;
    }

    /// After a failed call: it may have made something anyway, so look.
    async fn finish_failed(&mut self, intent: &Intent) {
        let key = intent.key(self.input.run_id);
        match self.ops.find(intent).await {
            Ok(found) => self.finish(&key, found).await,
            // Left pending; the reconciler resolves it before undo.
            Err(error) => tracing::warn!(
                run = self.input.run_id,
                "could not tell whether {} was created: {error}",
                intent.describe()
            ),
        }
    }

    async fn step(&mut self, index: usize, kind: StepKind) -> Outcome {
        let input = self.input;
        let address = input.address.clone();
        match kind {
            StepKind::CheckToken => {
                let (Some(token), Some(new)) = (&input.token, &input.new_tunnel) else {
                    return Outcome::Failed(
                        "Paste the Cloudflare API token again: Temps never stores it until \
                         the run saves it."
                            .into(),
                    );
                };
                let result = self.ops.check_token(token, &new.zone).await;
                match result.refusal() {
                    Some(reason) => Outcome::Failed(reason),
                    None => Outcome::Done(Some(
                        "Every permission is there, for this zone only.".into(),
                    )),
                }
            }
            StepKind::SaveDnsProvider => {
                let Some(new) = &input.new_tunnel else {
                    return Outcome::Skipped("No new tunnel.".into());
                };
                let intent = Intent::DnsProvider {
                    name: new.provider_name.clone(),
                };
                let resource = match self.begin(&intent).await {
                    Err(error) => return Outcome::Failed(error),
                    Ok(Begin::Have(resource)) => resource,
                    Ok(Begin::Go) => {
                        let Some(token) = &input.token else {
                            self.finish(&intent.key(input.run_id), None).await;
                            return Outcome::Failed(
                                "Paste the Cloudflare API token again: Temps never stores it \
                                 until the run saves it."
                                    .into(),
                            );
                        };
                        match self
                            .ops
                            .save_dns_provider(&new.provider_name, &new.zone, token)
                            .await
                        {
                            Ok(id) => {
                                let resource = RunResource::DnsProvider {
                                    id,
                                    name: new.provider_name.clone(),
                                };
                                self.finish(&intent.key(input.run_id), Some(resource.clone()))
                                    .await;
                                resource
                            }
                            Err(error) => {
                                self.finish_failed(&intent).await;
                                return Outcome::Failed(error);
                            }
                        }
                    }
                };
                if let RunResource::DnsProvider { id, name } = resource {
                    self.ctx.provider_id = Some(id);
                    Outcome::Done(Some(format!("Added as {name}.")))
                } else {
                    Outcome::Failed("The saved Cloudflare account could not be found.".into())
                }
            }
            StepKind::ManageDomain => {
                let Some(new) = &input.new_tunnel else {
                    return Outcome::Skipped("No new tunnel.".into());
                };
                let Some(provider_id) = self.ctx.provider_id else {
                    return Outcome::Failed(
                        "No Cloudflare account to manage the domain with.".into(),
                    );
                };
                let zone = new.zone.clone();
                let add = Intent::ManagedDomain {
                    provider_id,
                    domain: zone.clone(),
                };
                let enable = Intent::AutoManageEnabled {
                    provider_id,
                    domain: zone.clone(),
                };
                let state = if self.known(&add) || self.known(&enable) {
                    None
                } else {
                    match self.ops.managed_zone(provider_id, &zone).await {
                        Ok(state) => state,
                        Err(error) => return Outcome::Failed(error),
                    }
                };
                let mut verified = state.is_some_and(|s| s.verified);
                let found = state.is_some();
                match state {
                    None if !self.known(&enable) => {
                        self.set(
                            index,
                            RunStepStatus::Running,
                            Some(format!("Adding {zone} to Temps…")),
                        )
                        .await;
                        match self.begin(&add).await {
                            Err(error) => return Outcome::Failed(error),
                            Ok(Begin::Have(_)) => {
                                verified = self
                                    .ops
                                    .managed_zone(provider_id, &zone)
                                    .await
                                    .ok()
                                    .flatten()
                                    .is_some_and(|s| s.verified);
                            }
                            Ok(Begin::Go) => {
                                if let Err(error) =
                                    self.ops.add_managed_zone(provider_id, &zone).await
                                {
                                    self.finish_failed(&add).await;
                                    return Outcome::Failed(error);
                                }
                                self.finish(
                                    &add.key(input.run_id),
                                    Some(RunResource::ManagedDomain {
                                        provider_id,
                                        domain: zone.clone(),
                                    }),
                                )
                                .await;
                            }
                        }
                    }
                    Some(ZoneState {
                        auto_manage: true, ..
                    }) => {}
                    _ => match self.begin(&enable).await {
                        Err(error) => return Outcome::Failed(error),
                        Ok(Begin::Have(_)) => {}
                        Ok(Begin::Go) => {
                            if let Err(error) =
                                self.ops.set_auto_manage(provider_id, &zone, true).await
                            {
                                self.finish_failed(&enable).await;
                                return Outcome::Failed(error);
                            }
                            self.finish(
                                &enable.key(input.run_id),
                                Some(RunResource::AutoManageEnabled {
                                    provider_id,
                                    domain: zone.clone(),
                                }),
                            )
                            .await;
                        }
                    },
                }
                if !verified {
                    self.set(
                        index,
                        RunStepStatus::Running,
                        Some("Checking the token can manage it…".into()),
                    )
                    .await;
                    match self.ops.verify_zone(provider_id, &zone).await {
                        Ok(true) => {}
                        Ok(false) => {
                            return Outcome::Failed(format!(
                                "Cloudflare did not confirm the token can manage {zone}. Add \
                                 the zone to the token's Zone Resources (or, if your domain's \
                                 zone in Cloudflare is not {zone}, use a hostname right under \
                                 it), then try again."
                            ))
                        }
                        Err(error) => return Outcome::Failed(error),
                    }
                }
                Outcome::Done(Some(if found {
                    "Already in Temps.".into()
                } else {
                    "Added and verified.".into()
                }))
            }
            StepKind::CreateTunnel => {
                let Some(new) = input.new_tunnel.clone() else {
                    return Outcome::Skipped("No new tunnel.".into());
                };
                let Some(provider_id) = self.ctx.provider_id else {
                    return Outcome::Failed(
                        "No Cloudflare account to create the tunnel with.".into(),
                    );
                };
                let intent = Intent::Connector {
                    name: new.name.clone(),
                };
                match self.begin(&intent).await {
                    Err(error) => Outcome::Failed(error),
                    Ok(Begin::Have(resource)) => {
                        if let RunResource::Connector { id, .. } = resource {
                            self.ctx.connector_id = Some(id);
                        }
                        Outcome::Done(Some(format!(
                            "Tunnel {} for {} (created earlier).",
                            new.name, new.domain
                        )))
                    }
                    Ok(Begin::Go) => match self
                        .ops
                        .create_tunnel(&new.name, provider_id, &new.domain, &[new.worker.id])
                        .await
                    {
                        Ok((id, status)) => {
                            self.ctx.connector_id = Some(id);
                            self.finish(
                                &intent.key(input.run_id),
                                Some(RunResource::Connector {
                                    id,
                                    name: new.name.clone(),
                                }),
                            )
                            .await;
                            if status.status == "error" {
                                return Outcome::Failed(
                                    status
                                        .last_error
                                        .unwrap_or_else(|| "Cloudflare refused the tunnel.".into()),
                                );
                            }
                            Outcome::Done(Some(format!(
                                "Tunnel {} for {}.",
                                new.name, new.domain
                            )))
                        }
                        Err((saved, error)) => {
                            match saved {
                                Some(id) => {
                                    self.finish(
                                        &intent.key(input.run_id),
                                        Some(RunResource::Connector {
                                            id,
                                            name: new.name.clone(),
                                        }),
                                    )
                                    .await
                                }
                                None => self.finish_failed(&intent).await,
                            }
                            Outcome::Failed(error)
                        }
                    },
                }
            }
            StepKind::WaitForTunnel => {
                let (Some(id), Some(new)) = (self.ctx.connector_id, &input.new_tunnel) else {
                    return Outcome::Skipped("No new tunnel.".into());
                };
                let worker = new.worker.clone();
                let mut waited = Duration::ZERO;
                loop {
                    match self.ops.tunnel_status(id).await {
                        Ok(status) if status.status == "error" => {
                            return Outcome::Failed(
                                status
                                    .last_error
                                    .unwrap_or_else(|| "Cloudflare refused the tunnel.".into()),
                            )
                        }
                        Ok(status) => {
                            if let Some((_, state, links, error)) =
                                status.replicas.iter().find(|r| r.0 == worker.id)
                            {
                                match state.as_str() {
                                    "ready" => {
                                        return Outcome::Done(Some(match links {
                                            Some(n) => format!(
                                                "{} has {n} connection{} to Cloudflare.",
                                                worker.name,
                                                if *n == 1 { "" } else { "s" }
                                            ),
                                            None => format!("{} is connected.", worker.name),
                                        }))
                                    }
                                    "refused" => {
                                        return Outcome::Failed(format!(
                                            "{} could not run the tunnel{}",
                                            worker.name,
                                            error
                                                .as_ref()
                                                .map(|e| format!(": {e}"))
                                                .unwrap_or_else(|| ".".into())
                                        ))
                                    }
                                    _ => {
                                        self.set(
                                            index,
                                            RunStepStatus::Running,
                                            Some(match error {
                                                Some(e) => format!("{state}: {e}"),
                                                None => format!("{}: {state}", worker.name),
                                            }),
                                        )
                                        .await
                                    }
                                }
                            }
                        }
                        // A missed poll is not a failure; the deadline still applies.
                        Err(error) => {
                            self.set(
                                index,
                                RunStepStatus::Running,
                                Some(format!("Status unavailable: {error}")),
                            )
                            .await
                        }
                    }
                    if waited >= self.timing.connect {
                        // Slow is not broken: the rest can be set up meanwhile.
                        return Outcome::Done(Some(format!(
                            "{} has not connected yet; it keeps trying.",
                            worker.name
                        )));
                    }
                    self.ops.sleep(self.timing.poll).await;
                    waited += self.timing.poll.max(Duration::from_millis(1));
                }
            }
            StepKind::AttachDomain => {
                let intent = Intent::CustomDomain {
                    domain: address.clone(),
                    environment_id: input.environment_id,
                };
                if !self.known(&intent) {
                    match self.ops.find_custom_domain(&address).await {
                        Ok(Some((_, environment_id))) if environment_id == input.environment_id => {
                            return Outcome::Skipped(format!(
                                "Already on {}.",
                                input.environment_name
                            ))
                        }
                        Ok(Some(_)) => {
                            return Outcome::Failed(format!(
                                "{address} is attached to another environment. Remove it there \
                                 first, or use another hostname."
                            ))
                        }
                        Ok(None) => {}
                        Err(error) => return Outcome::Failed(error),
                    }
                }
                match self.begin(&intent).await {
                    Err(error) => Outcome::Failed(error),
                    Ok(Begin::Have(_)) => Outcome::Done(None),
                    Ok(Begin::Go) => match self
                        .ops
                        .attach_domain(input.project_id, input.environment_id, &address)
                        .await
                    {
                        Ok(id) => {
                            self.finish(
                                &intent.key(input.run_id),
                                Some(RunResource::CustomDomain {
                                    id,
                                    domain: address.clone(),
                                }),
                            )
                            .await;
                            Outcome::Done(None)
                        }
                        Err(error) => {
                            self.finish_failed(&intent).await;
                            Outcome::Failed(error)
                        }
                    },
                }
            }
            StepKind::RouteHostname => {
                let Some(connector_id) = self
                    .ctx
                    .connector_id
                    .or(input.tunnel.as_ref().map(|t| t.id))
                else {
                    return Outcome::Skipped("No tunnel to publish through.".into());
                };
                let known = self
                    .record
                    .journal
                    .iter()
                    .find(|e| {
                        e.state != JournalState::Absent
                            && matches!(&e.intent, Intent::HostnameChoice { hostname, .. } if *hostname == address)
                    })
                    .map(|e| e.intent.clone());
                let intent = match known {
                    Some(intent) => intent,
                    None => {
                        let current = match self.ops.hostname_choice(&address).await {
                            Ok(current) => current,
                            Err(error) => return Outcome::Failed(error),
                        };
                        if current == Some(Some(connector_id)) {
                            return Outcome::Skipped("Already chosen for this tunnel.".into());
                        }
                        // Another user's choice is changed only when the plan
                        // said so and it was confirmed.
                        if current.is_some() && !input.route_confirmed {
                            return Outcome::Failed(format!(
                                "{address} has a tunnel choice that this setup would change, and \
                                 that was not confirmed. Plan again and confirm the change, or \
                                 change it under Expose your apps."
                            ));
                        }
                        Intent::HostnameChoice {
                            hostname: address.clone(),
                            connector_id,
                            previous: PreviousChoice::from_choice(current),
                        }
                    }
                };
                let Intent::HostnameChoice { previous, .. } = intent.clone() else {
                    return Outcome::Failed("The hostname choice could not be recorded.".into());
                };
                match self.begin(&intent).await {
                    Err(error) => Outcome::Failed(error),
                    Ok(Begin::Have(_)) => Outcome::Done(None),
                    Ok(Begin::Go) => match self.ops.route_hostname(&address, connector_id).await {
                        Ok(()) => {
                            self.finish(
                                &intent.key(input.run_id),
                                Some(RunResource::HostnameChoice {
                                    hostname: address.clone(),
                                    connector_id,
                                    previous,
                                }),
                            )
                            .await;
                            Outcome::Done(None)
                        }
                        Err(error) => {
                            self.finish_failed(&intent).await;
                            Outcome::Failed(error)
                        }
                    },
                }
            }
            StepKind::SetDnsRecord => {
                let Some((_, ip)) = input.direct.clone() else {
                    return Outcome::Failed("No address to point the record at.".into());
                };
                let kind = record_type(ip).to_string();
                let intent = Intent::DnsRecord {
                    hostname: address.clone(),
                    label: String::new(),
                    record_type: kind.clone(),
                };
                match self.begin(&intent).await {
                    Err(error) => Outcome::Failed(error),
                    Ok(Begin::Have(_)) => Outcome::Done(Some(format!("{address} points at {ip}."))),
                    Ok(Begin::Go) => match self.ops.set_dns_record(&address, ip).await {
                        Ok(label) => {
                            self.finish(
                                &intent.key(input.run_id),
                                Some(RunResource::DnsRecord {
                                    hostname: address.clone(),
                                    label,
                                    record_type: kind,
                                }),
                            )
                            .await;
                            Outcome::Done(Some(format!("{address} points at {ip}.")))
                        }
                        Err(error) => {
                            self.finish_failed(&intent).await;
                            Outcome::Failed(error)
                        }
                    },
                }
            }
            StepKind::ShowDnsRecord => {
                let Some((_, ip)) = input.direct.clone() else {
                    return Outcome::Failed("No address to point the record at.".into());
                };
                let wanted = format!(
                    "Add an A record for {address} pointing at {ip} where your domain's DNS is \
                     hosted"
                );
                self.set(
                    index,
                    RunStepStatus::Running,
                    Some(format!("{wanted}; waiting for it…")),
                )
                .await;
                let mut waited = Duration::ZERO;
                loop {
                    if self.ops.resolve(&address).await.contains(&ip) {
                        return Outcome::Done(Some(format!("{address} points at {ip}.")));
                    }
                    if waited >= self.timing.dns {
                        return Outcome::Stalled(format!(
                            "{wanted}, then resume this setup: it picks up where it stopped."
                        ));
                    }
                    self.ops.sleep(self.timing.poll).await;
                    waited += self.timing.poll.max(Duration::from_millis(1));
                }
            }
            StepKind::IssueCertificate => {
                let intent = Intent::Certificate {
                    domain: address.clone(),
                };
                if !self.known(&intent) {
                    match self.ops.certificate_exists(&address).await {
                        Ok(true) => {
                            return Outcome::Skipped("A certificate already covers it.".into())
                        }
                        Ok(false) => {}
                        Err(error) => return Outcome::Failed(error),
                    }
                }
                let Some(email) = input.acme_email.clone() else {
                    return Outcome::Failed(
                        "Let's Encrypt needs an email address, and your account has none. Add \
                         one to your profile, then try again."
                            .into(),
                    );
                };
                match self.begin(&intent).await {
                    Err(error) => return Outcome::Failed(error),
                    Ok(Begin::Have(_)) => {}
                    Ok(Begin::Go) => {
                        if let Err(error) = self.ops.start_certificate(&address).await {
                            self.finish_failed(&intent).await;
                            return Outcome::Failed(error);
                        }
                        self.finish(
                            &intent.key(input.run_id),
                            Some(RunResource::Certificate {
                                domain: address.clone(),
                            }),
                        )
                        .await;
                    }
                }
                self.set(
                    index,
                    RunStepStatus::Running,
                    Some("Let's Encrypt is checking the domain over HTTP…".into()),
                )
                .await;
                match self.ops.finish_certificate(&address, &email).await {
                    Ok(()) => Outcome::Done(Some("Issued by Let's Encrypt.".into())),
                    Err(error) => Outcome::Failed(error),
                }
            }
            StepKind::WaitForPublication => {
                let mut waited = Duration::ZERO;
                loop {
                    match self.ops.hostname_state(&address).await {
                        Ok((state, detail)) if state == "published" => {
                            return Outcome::Done(Some(detail))
                        }
                        Ok((state, detail)) if state == "error" => {
                            return Outcome::Stalled(detail)
                        }
                        Ok((_, detail)) => {
                            if waited >= self.timing.publish {
                                return Outcome::Stalled(format!(
                                    "Not published yet: {detail} Temps keeps trying every \
                                     minute."
                                ));
                            }
                            self.set(index, RunStepStatus::Running, Some(detail)).await;
                        }
                        Err(error) => {
                            if waited >= self.timing.publish {
                                return Outcome::Stalled(format!(
                                    "Could not tell whether it is published: {error}"
                                ));
                            }
                        }
                    }
                    self.ops.sleep(self.timing.poll).await;
                    waited += self.timing.poll.max(Duration::from_millis(1));
                }
            }
            StepKind::CheckReachability => {
                let verdict = self.ops.reachability(&address).await;
                if verdict.state == ReachState::Reachable {
                    self.record.live_url = Some(format!("https://{address}"));
                    Outcome::Done(Some(verdict.reason))
                } else {
                    Outcome::Stalled(verdict.reason)
                }
            }
        }
    }

    async fn drive(mut self, from: usize) -> RunRecord {
        self.save().await;
        for index in from..self.input.steps.len() {
            let kind = self.input.steps[index].kind;
            self.set(index, RunStepStatus::Running, None).await;
            match self.step(index, kind).await {
                Outcome::Done(detail) => self.set(index, RunStepStatus::Done, detail).await,
                Outcome::Skipped(detail) => {
                    self.set(index, RunStepStatus::Skipped, Some(detail)).await
                }
                Outcome::Stalled(reason) => {
                    self.set(index, RunStepStatus::Failed, Some(reason.clone()))
                        .await;
                    self.record.state = RunState::Waiting;
                    self.record.error = Some(reason);
                    self.save().await;
                    return self.record;
                }
                Outcome::Failed(error) => {
                    self.set(index, RunStepStatus::Failed, Some(error.clone()))
                        .await;
                    self.record.state = RunState::Undoing;
                    self.save().await;
                    let had = !self.record.created().is_empty() || self.record.has_pending();
                    let results = undo(&mut self.record.journal, self.ops).await;
                    let failed: Vec<String> = results
                        .iter()
                        .filter(|r| !r.ok)
                        .map(|r| r.label.clone())
                        .collect();
                    self.record.undo = results;
                    self.record.state = RunState::Failed;
                    self.record.error = Some(if !had {
                        error
                    } else if failed.is_empty() {
                        format!("{error} What this run had created was removed.")
                    } else {
                        format!(
                            "{error} Could not remove everything this run created: {}.",
                            failed.join("; ")
                        )
                    });
                    self.save().await;
                    return self.record;
                }
            }
        }
        self.record.state = if self.record.live_url.is_some() {
            RunState::Succeeded
        } else {
            RunState::Waiting
        };
        self.save().await;
        self.record
    }
}

/// The context earlier steps left, from what the journal says they made.
fn context(input: &RunInput, journal: &[JournalEntry]) -> Context {
    let mut ctx = Context {
        provider_id: input.new_tunnel.as_ref().and_then(|t| t.provider_id),
        connector_id: None,
    };
    for resource in created(journal) {
        match resource {
            RunResource::DnsProvider { id, .. } => ctx.provider_id = Some(id),
            RunResource::Connector { id, .. } => ctx.connector_id = Some(id),
            _ => {}
        }
    }
    ctx
}

/// Carry out a run. The record is saved after every change and returned
/// finished.
pub async fn execute(
    input: &RunInput,
    ops: &dyn RunOps,
    sink: &dyn RunSink,
    timing: Timing,
) -> RunRecord {
    Runner {
        input,
        ops,
        sink,
        timing,
        record: RunRecord::new(&input.steps),
        ctx: context(input, &[]),
    }
    .drive(0)
    .await
}

/// Carry on a run that stopped (waiting, or interrupted by a restart) from
/// its first step not done. Its journal must have no pending entry left:
/// the reconciler resolves those first.
pub async fn resume(
    input: &RunInput,
    mut record: RunRecord,
    ops: &dyn RunOps,
    sink: &dyn RunSink,
    timing: Timing,
) -> RunRecord {
    let from = record
        .steps
        .iter()
        .position(|s| !matches!(s.status, RunStepStatus::Done | RunStepStatus::Skipped))
        .unwrap_or(record.steps.len());
    for step in record.steps.iter_mut().skip(from) {
        step.status = RunStepStatus::Pending;
        step.detail = None;
    }
    record.state = RunState::Running;
    record.error = None;
    record.live_url = None;
    let ctx = context(input, &record.journal);
    Runner {
        input,
        ops,
        sink,
        timing,
        record,
        ctx,
    }
    .drive(from)
    .await
}

/// Remove what a run created, newest first, and mark it removed in the
/// journal. A pending entry is looked up first. Nothing else is touched; a
/// failure is reported and the rest is still attempted.
pub async fn undo(journal: &mut [JournalEntry], ops: &dyn RunOps) -> Vec<UndoResult> {
    let mut results = Vec::new();
    for entry in journal.iter_mut().rev() {
        let resource = match (entry.state, entry.resource.clone()) {
            (JournalState::Done, Some(resource)) => resource,
            (JournalState::Pending, _) => match ops.find(&entry.intent).await {
                Ok(Some(resource)) => resource,
                Ok(None) => {
                    entry.state = JournalState::Absent;
                    continue;
                }
                Err(error) => {
                    results.push(UndoResult {
                        resource: placeholder(&entry.intent),
                        label: format!("Could not tell whether {} exists", entry.intent.describe()),
                        ok: false,
                        error: Some(error),
                    });
                    continue;
                }
            },
            _ => continue,
        };
        let outcome = match &resource {
            RunResource::DnsProvider { id, .. } => ops.delete_dns_provider(*id).await,
            RunResource::ManagedDomain {
                provider_id,
                domain,
            } => ops.remove_managed_zone(*provider_id, domain).await,
            RunResource::AutoManageEnabled {
                provider_id,
                domain,
            } => ops.set_auto_manage(*provider_id, domain, false).await,
            RunResource::Connector { id, .. } => ops.delete_tunnel(*id).await,
            RunResource::CustomDomain { id, .. } => ops.detach_domain(*id).await,
            RunResource::HostnameChoice {
                hostname, previous, ..
            } => ops.restore_hostname_choice(hostname, *previous).await,
            RunResource::DnsRecord {
                hostname,
                label,
                record_type,
            } => ops.remove_dns_record(hostname, label, record_type).await,
            RunResource::Certificate { domain } => ops.delete_certificate(domain).await,
        };
        let label = match &resource {
            RunResource::AutoManageEnabled { domain, .. } => {
                format!("Turned automatic DNS for {domain} back off")
            }
            RunResource::HostnameChoice {
                hostname,
                previous: PreviousChoice::Off,
                ..
            } => format!("Kept {hostname} off tunnels again"),
            RunResource::HostnameChoice {
                hostname,
                previous: PreviousChoice::Tunnel { .. },
                ..
            } => format!("Put {hostname} back on the tunnel chosen before"),
            other => format!("Removed {}", other.describe()),
        };
        if outcome.is_ok() {
            entry.state = JournalState::Absent;
            entry.at = chrono::Utc::now();
        } else {
            entry.state = JournalState::Done;
            entry.resource = Some(resource.clone());
        }
        results.push(UndoResult {
            resource,
            label,
            ok: outcome.is_ok(),
            error: outcome.err(),
        });
    }
    results
}

/// What an intent would have made, for reporting one that could not be
/// looked up.
fn placeholder(intent: &Intent) -> RunResource {
    match intent.clone() {
        Intent::DnsProvider { name } => RunResource::DnsProvider { id: 0, name },
        Intent::ManagedDomain {
            provider_id,
            domain,
        } => RunResource::ManagedDomain {
            provider_id,
            domain,
        },
        Intent::AutoManageEnabled {
            provider_id,
            domain,
        } => RunResource::AutoManageEnabled {
            provider_id,
            domain,
        },
        Intent::Connector { name } => RunResource::Connector { id: 0, name },
        Intent::CustomDomain { domain, .. } => RunResource::CustomDomain { id: 0, domain },
        Intent::HostnameChoice {
            hostname,
            connector_id,
            previous,
        } => RunResource::HostnameChoice {
            hostname,
            connector_id,
            previous,
        },
        Intent::DnsRecord {
            hostname,
            label,
            record_type,
        } => RunResource::DnsRecord {
            hostname,
            label,
            record_type,
        },
        Intent::Certificate { domain } => RunResource::Certificate { domain },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reachability::model::{AddressSource, Audience};
    use crate::reachability::token_check::{PermissionCheck, PermissionState, TokenPermission};

    fn done(intent: Intent, resource: RunResource) -> JournalEntry {
        JournalEntry {
            key: intent.key(0),
            intent,
            state: JournalState::Done,
            resource: Some(resource),
            at: chrono::Utc::now(),
        }
    }
    use std::sync::Mutex;

    /// Records every call; fails the ones named in `fail`.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        fail: Vec<&'static str>,
        zone: Option<ZoneState>,
        existing_domain: Option<(i32, i32)>,
        choice: Option<Option<i32>>,
        published_after: usize,
        reachable: bool,
        resolves: bool,
        polls: Mutex<usize>,
        /// What `find` answers, by intent key without the run prefix.
        found: Mutex<Vec<(String, RunResource)>>,
    }

    impl Fake {
        fn log(&self, call: impl Into<String>) {
            self.calls.lock().expect("lock").push(call.into());
        }
        fn result(&self, name: &'static str) -> Result<(), String> {
            self.log(name);
            if self.fail.contains(&name) {
                Err(format!("{name} broke"))
            } else {
                Ok(())
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock").clone()
        }
        fn exists(&self, intent: &Intent, resource: RunResource) {
            self.found
                .lock()
                .expect("lock")
                .push((intent.key(0), resource));
        }
    }

    #[async_trait]
    impl RunOps for Fake {
        async fn check_token(&self, _token: &str, _zone: &str) -> TokenCheckResult {
            self.log("check_token");
            let state = if self.fail.contains(&"check_token") {
                PermissionState::Missing
            } else {
                PermissionState::Present
            };
            TokenCheckResult {
                valid: true,
                error: None,
                zones: vec!["example.com".into()],
                checked_zone: Some("example.com".into()),
                permissions: vec![PermissionCheck {
                    permission: TokenPermission::DnsEdit,
                    label: "Zone · DNS · Edit".into(),
                    state,
                    detail: String::new(),
                }],
                scope: None,
                ready: state == PermissionState::Present,
                template_url: String::new(),
            }
        }
        async fn find(&self, intent: &Intent) -> Result<Option<RunResource>, String> {
            self.log(format!("find {}", intent.key(0)));
            if self.fail.contains(&"find") {
                return Err("database is down".into());
            }
            let key = intent.key(0);
            Ok(self
                .found
                .lock()
                .expect("lock")
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, r)| r.clone()))
        }
        async fn save_dns_provider(
            &self,
            name: &str,
            _zone: &str,
            _token: &str,
        ) -> Result<i32, String> {
            self.result("save_dns_provider")?;
            assert_eq!(name, "Cloudflare example.com");
            Ok(30)
        }
        async fn delete_dns_provider(&self, id: i32) -> Result<(), String> {
            self.log(format!("delete_dns_provider {id}"));
            Ok(())
        }
        async fn managed_zone(&self, _p: i32, _z: &str) -> Result<Option<ZoneState>, String> {
            self.log("managed_zone");
            Ok(self.zone)
        }
        async fn add_managed_zone(&self, _p: i32, _z: &str) -> Result<(), String> {
            self.result("add_managed_zone")
        }
        async fn set_auto_manage(&self, _p: i32, _z: &str, on: bool) -> Result<(), String> {
            self.log(format!("set_auto_manage {on}"));
            Ok(())
        }
        async fn verify_zone(&self, _p: i32, _z: &str) -> Result<bool, String> {
            self.log("verify_zone");
            Ok(!self.fail.contains(&"verify_zone"))
        }
        async fn remove_managed_zone(&self, _p: i32, zone: &str) -> Result<(), String> {
            self.log(format!("remove_managed_zone {zone}"));
            Ok(())
        }
        async fn create_tunnel(
            &self,
            _name: &str,
            _provider_id: i32,
            _domain: &str,
            _node_ids: &[i32],
        ) -> Result<(i32, TunnelStatus), (Option<i32>, String)> {
            self.log("create_tunnel");
            if self.fail.contains(&"create_tunnel") {
                return Err((Some(40), "Cloudflare refused it".into()));
            }
            Ok((
                40,
                TunnelStatus {
                    status: "ready".into(),
                    last_error: None,
                    replicas: Vec::new(),
                },
            ))
        }
        async fn tunnel_status(&self, _id: i32) -> Result<TunnelStatus, String> {
            self.log("tunnel_status");
            Ok(TunnelStatus {
                status: "ready".into(),
                last_error: None,
                replicas: vec![(2, "ready".into(), Some(4), None)],
            })
        }
        async fn delete_tunnel(&self, id: i32) -> Result<(), String> {
            self.log(format!("delete_tunnel {id}"));
            if self.fail.contains(&"delete_tunnel") {
                return Err("Cloudflare is down".into());
            }
            Ok(())
        }
        async fn find_custom_domain(&self, _host: &str) -> Result<Option<(i32, i32)>, String> {
            self.log("find_custom_domain");
            Ok(self.existing_domain)
        }
        async fn attach_domain(&self, _p: i32, _e: i32, _host: &str) -> Result<i32, String> {
            self.result("attach_domain")?;
            Ok(50)
        }
        async fn detach_domain(&self, id: i32) -> Result<(), String> {
            self.log(format!("detach_domain {id}"));
            Ok(())
        }
        async fn hostname_choice(&self, _host: &str) -> Result<Option<Option<i32>>, String> {
            self.log("hostname_choice");
            Ok(self.choice)
        }
        async fn route_hostname(&self, _host: &str, connector_id: i32) -> Result<(), String> {
            self.log(format!("route_hostname {connector_id}"));
            Ok(())
        }
        async fn restore_hostname_choice(
            &self,
            host: &str,
            previous: PreviousChoice,
        ) -> Result<(), String> {
            self.log(format!("restore_hostname_choice {host} {previous:?}"));
            Ok(())
        }
        async fn hostname_state(&self, _host: &str) -> Result<(String, String), String> {
            let mut polls = self.polls.lock().expect("lock");
            *polls += 1;
            self.log("hostname_state");
            Ok(if *polls > self.published_after {
                ("published".into(), "Published through tunnel 'example-com'.".into())
            } else {
                ("pending".into(), "Being published.".into())
            })
        }
        async fn set_dns_record(&self, _host: &str, _ip: IpAddr) -> Result<String, String> {
            self.result("set_dns_record")?;
            Ok("myapp".into())
        }
        async fn remove_dns_record(
            &self,
            host: &str,
            label: &str,
            record_type: &str,
        ) -> Result<(), String> {
            self.log(format!("remove_dns_record {host} {label} {record_type}"));
            Ok(())
        }
        async fn certificate_exists(&self, _host: &str) -> Result<bool, String> {
            self.log("certificate_exists");
            Ok(false)
        }
        async fn start_certificate(&self, _host: &str) -> Result<(), String> {
            self.result("start_certificate")
        }
        async fn finish_certificate(&self, _host: &str, _email: &str) -> Result<(), String> {
            self.result("finish_certificate")
        }
        async fn delete_certificate(&self, host: &str) -> Result<(), String> {
            self.log(format!("delete_certificate {host}"));
            Ok(())
        }
        async fn resolve(&self, _host: &str) -> Vec<IpAddr> {
            self.log("resolve");
            if self.resolves {
                vec!["203.0.113.5".parse().expect("ip")]
            } else {
                Vec::new()
            }
        }
        async fn reachability(&self, host: &str) -> AddressReachability {
            self.log("reachability");
            AddressReachability {
                address: host.into(),
                audience: Audience::Public,
                source: AddressSource::CustomDomain,
                custom_domain_id: None,
                way_in: None,
                state: if self.reachable {
                    ReachState::Reachable
                } else {
                    ReachState::NotReachable
                },
                reason: "checked".into(),
                action: None,
            }
        }
        async fn sleep(&self, _duration: Duration) {}
    }

    #[derive(Default)]
    struct Saved(Mutex<Vec<RunRecord>>, bool);

    #[async_trait]
    impl RunSink for Saved {
        async fn save(&self, record: &RunRecord) -> Result<(), String> {
            if self.1 {
                return Err("disk full".into());
            }
            self.0.lock().expect("lock").push(record.clone());
            Ok(())
        }
    }

    fn timing() -> Timing {
        Timing {
            poll: Duration::from_millis(1),
            connect: Duration::from_millis(5),
            publish: Duration::from_millis(5),
            dns: Duration::from_millis(5),
        }
    }

    fn steps(kinds: &[StepKind]) -> Vec<PlanStep> {
        kinds
            .iter()
            .map(|kind| PlanStep {
                kind: *kind,
                label: format!("{kind:?}"),
            })
            .collect()
    }

    fn token_input() -> RunInput {
        RunInput {
            run_id: 0,
            route_confirmed: false,
            project_id: 1,
            environment_id: 7,
            environment_name: "production".into(),
            option: PlanOptionKind::NewTunnelWithToken,
            address: "myapp.example.com".into(),
            steps: steps(&[
                StepKind::CheckToken,
                StepKind::SaveDnsProvider,
                StepKind::ManageDomain,
                StepKind::CreateTunnel,
                StepKind::WaitForTunnel,
                StepKind::AttachDomain,
                StepKind::RouteHostname,
                StepKind::WaitForPublication,
                StepKind::CheckReachability,
            ]),
            tunnel: None,
            new_tunnel: Some(NewTunnel {
                provider_id: None,
                provider_name: "Cloudflare example.com".into(),
                zone: "example.com".into(),
                domain: "example.com".into(),
                name: "example-com".into(),
                worker: NamedId {
                    id: 2,
                    name: "master".into(),
                },
            }),
            token: Some("secret-token".into()),
            direct: None,
            acme_email: None,
        }
    }

    #[tokio::test]
    async fn a_token_run_records_everything_it_creates_in_order() {
        let ops = Fake {
            reachable: true,
            ..Default::default()
        };
        let sink = Saved::default();
        let record = execute(&token_input(), &ops, &sink, timing()).await;
        assert_eq!(record.state, RunState::Succeeded);
        assert_eq!(record.live_url.as_deref(), Some("https://myapp.example.com"));
        assert_eq!(
            record.created(),
            vec![
                RunResource::DnsProvider {
                    id: 30,
                    name: "Cloudflare example.com".into()
                },
                RunResource::ManagedDomain {
                    provider_id: 30,
                    domain: "example.com".into()
                },
                RunResource::Connector {
                    id: 40,
                    name: "example-com".into()
                },
                RunResource::CustomDomain {
                    id: 50,
                    domain: "myapp.example.com".into()
                },
                RunResource::HostnameChoice {
                    hostname: "myapp.example.com".into(),
                    connector_id: 40,
                    previous: PreviousChoice::Automatic,
                },
            ]
        );
        assert!(record.steps.iter().all(|s| s.status == RunStepStatus::Done));
        // Progress was saved as it happened, and the token never was.
        let saved = sink.0.lock().expect("lock");
        assert!(saved.len() > record.steps.len());
        assert!(!format!("{saved:?}").contains("secret-token"));
    }

    #[tokio::test]
    async fn a_failed_step_undoes_the_run_newest_first() {
        let ops = Fake {
            fail: vec!["attach_domain"],
            ..Default::default()
        };
        let record = execute(&token_input(), &ops, &Saved::default(), timing()).await;
        assert_eq!(record.state, RunState::Failed);
        let error = record.error.expect("error");
        assert!(error.starts_with("attach_domain broke"), "{error}");
        assert!(error.contains("was removed"));
        let undo_calls: Vec<String> = ops
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("delete_") || c.starts_with("remove_"))
            .collect();
        assert_eq!(
            undo_calls,
            vec![
                "delete_tunnel 40",
                "remove_managed_zone example.com",
                "delete_dns_provider 30"
            ]
        );
        assert!(record.undo.iter().all(|u| u.ok));
        let attach = record
            .steps
            .iter()
            .find(|s| s.kind == StepKind::AttachDomain)
            .expect("step");
        assert_eq!(attach.status, RunStepStatus::Failed);
    }

    #[tokio::test]
    async fn undo_leaves_alone_what_the_run_found_already_there() {
        // The zone was managed with automatic DNS off; the domain was already
        // on this environment and already chosen for the tunnel.
        let ops = Fake {
            zone: Some(ZoneState {
                auto_manage: false,
                verified: true,
            }),
            existing_domain: Some((51, 7)),
            choice: Some(Some(40)),
            reachable: true,
            ..Default::default()
        };
        let mut input = token_input();
        input.new_tunnel = Some(NewTunnel {
            provider_id: Some(3),
            ..input.new_tunnel.clone().expect("tunnel")
        });
        input.token = None;
        input.option = PlanOptionKind::NewTunnel;
        input.steps.retain(|s| {
            !matches!(s.kind, StepKind::CheckToken | StepKind::SaveDnsProvider)
        });
        let record = execute(&input, &ops, &Saved::default(), timing()).await;
        assert_eq!(record.state, RunState::Succeeded);
        assert_eq!(
            record.created(),
            vec![
                RunResource::AutoManageEnabled {
                    provider_id: 3,
                    domain: "example.com".into()
                },
                RunResource::Connector {
                    id: 40,
                    name: "example-com".into()
                },
            ]
        );
        let mut journal = record.journal.clone();
        let results = undo(&mut journal, &ops).await;
        assert_eq!(
            results.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(),
            vec![
                "Removed Tunnel example-com",
                "Turned automatic DNS for example.com back off"
            ]
        );
        assert!(!ops.calls().iter().any(|c| c.starts_with("detach_domain")));
    }

    #[tokio::test]
    async fn an_undo_failure_is_reported_and_the_rest_still_removed() {
        let ops = Fake {
            fail: vec!["delete_tunnel"],
            ..Default::default()
        };
        let mut journal = vec![
            done(
                Intent::DnsProvider { name: "cf".into() },
                RunResource::DnsProvider {
                    id: 30,
                    name: "cf".into(),
                },
            ),
            done(
                Intent::Connector { name: "t".into() },
                RunResource::Connector {
                    id: 40,
                    name: "t".into(),
                },
            ),
        ];
        let results = undo(&mut journal, &ops).await;
        assert!(!results[0].ok);
        assert_eq!(results[0].error.as_deref(), Some("Cloudflare is down"));
        assert!(results[1].ok);
    }

    #[tokio::test]
    async fn a_missing_permission_stops_before_anything_is_created() {
        let ops = Fake {
            fail: vec!["check_token"],
            ..Default::default()
        };
        let record = execute(&token_input(), &ops, &Saved::default(), timing()).await;
        assert_eq!(record.state, RunState::Failed);
        assert!(record.created().is_empty());
        assert!(record
            .error
            .expect("error")
            .contains("The token lacks Zone · DNS · Edit"));
        assert!(!ops.calls().contains(&"save_dns_provider".to_string()));
    }

    #[tokio::test]
    async fn slow_publication_keeps_everything_and_waits() {
        let ops = Fake {
            published_after: usize::MAX,
            ..Default::default()
        };
        let record = execute(&token_input(), &ops, &Saved::default(), timing()).await;
        assert_eq!(record.state, RunState::Waiting);
        assert_eq!(record.created().len(), 5);
        assert!(record.undo.is_empty());
        assert!(record.error.expect("reason").contains("Not published yet"));
        let check = record.steps.last().expect("step");
        assert_eq!(check.status, RunStepStatus::Pending);
    }

    #[tokio::test]
    async fn a_direct_run_sets_dns_and_issues_the_certificate() {
        let ops = Fake {
            reachable: true,
            resolves: true,
            ..Default::default()
        };
        let input = RunInput {
            option: PlanOptionKind::DirectManagedDns,
            steps: steps(&[
                StepKind::AttachDomain,
                StepKind::SetDnsRecord,
                StepKind::IssueCertificate,
                StepKind::CheckReachability,
            ]),
            new_tunnel: None,
            token: None,
            direct: Some(("edge-1".into(), "203.0.113.5".parse().expect("ip"))),
            acme_email: Some("ops@example.com".into()),
            ..token_input()
        };
        let record = execute(&input, &ops, &Saved::default(), timing()).await;
        assert_eq!(record.state, RunState::Succeeded);
        assert_eq!(
            record.created(),
            vec![
                RunResource::CustomDomain {
                    id: 50,
                    domain: "myapp.example.com".into()
                },
                RunResource::DnsRecord {
                    hostname: "myapp.example.com".into(),
                    label: "myapp".into(),
                    record_type: "A".into(),
                },
                RunResource::Certificate {
                    domain: "myapp.example.com".into()
                },
            ]
        );

        // Without an email the certificate step fails and everything goes.
        let ops = Fake::default();
        let record = execute(
            &RunInput {
                acme_email: None,
                ..input.clone()
            },
            &ops,
            &Saved::default(),
            timing(),
        )
        .await;
        assert_eq!(record.state, RunState::Failed);
        assert!(ops
            .calls()
            .contains(&"remove_dns_record myapp.example.com myapp A".to_string()));
    }

    #[tokio::test]
    async fn a_dns_record_the_user_has_not_added_yet_waits() {
        let ops = Fake::default();
        let input = RunInput {
            option: PlanOptionKind::DirectManualDns,
            steps: steps(&[
                StepKind::AttachDomain,
                StepKind::ShowDnsRecord,
                StepKind::IssueCertificate,
                StepKind::CheckReachability,
            ]),
            new_tunnel: None,
            token: None,
            direct: Some(("control-plane".into(), "203.0.113.5".parse().expect("ip"))),
            ..token_input()
        };
        let record = execute(&input, &ops, &Saved::default(), timing()).await;
        assert_eq!(record.state, RunState::Waiting);
        assert_eq!(record.created().len(), 1);
        assert!(record
            .error
            .expect("reason")
            .contains("Add an A record for myapp.example.com pointing at 203.0.113.5"));
    }

    #[tokio::test]
    async fn every_creating_call_is_journaled_pending_first() {
        let ops = Fake {
            reachable: true,
            ..Default::default()
        };
        let sink = Saved::default();
        execute(&token_input(), &ops, &sink, timing()).await;
        let saved = sink.0.lock().expect("lock");
        // The tunnel's entry was stored pending before it was created, and
        // done after.
        let key = Intent::Connector {
            name: "example-com".into(),
        }
        .key(0);
        let states: Vec<JournalState> = saved
            .iter()
            .filter_map(|r| r.journal.iter().find(|e| e.key == key).map(|e| e.state))
            .collect();
        assert_eq!(states.first(), Some(&JournalState::Pending));
        assert_eq!(states.last(), Some(&JournalState::Done));
    }

    #[tokio::test]
    async fn nothing_is_created_when_the_pending_entry_cannot_be_stored() {
        let ops = Fake::default();
        let sink = Saved(Mutex::new(Vec::new()), true);
        let record = execute(&token_input(), &ops, &sink, timing()).await;
        assert_eq!(record.state, RunState::Failed);
        assert!(!ops.calls().contains(&"save_dns_provider".to_string()));
        let error = record.error.expect("error");
        assert!(error.contains("could not record"), "{error}");
    }

    #[tokio::test]
    async fn a_resume_finds_what_an_interrupted_attempt_made_instead_of_making_it_twice() {
        // Temps stopped after the tunnel was created but before its entry
        // was marked done.
        let ops = Fake {
            reachable: true,
            ..Default::default()
        };
        let input = token_input();
        let mut record = RunRecord::new(&input.steps);
        record.journal = vec![
            done(
                Intent::DnsProvider {
                    name: "Cloudflare example.com".into(),
                },
                RunResource::DnsProvider {
                    id: 30,
                    name: "Cloudflare example.com".into(),
                },
            ),
            done(
                Intent::ManagedDomain {
                    provider_id: 30,
                    domain: "example.com".into(),
                },
                RunResource::ManagedDomain {
                    provider_id: 30,
                    domain: "example.com".into(),
                },
            ),
            JournalEntry {
                state: JournalState::Pending,
                resource: None,
                ..done(
                    Intent::Connector {
                        name: "example-com".into(),
                    },
                    RunResource::Connector {
                        id: 0,
                        name: String::new(),
                    },
                )
            },
        ];
        for step in record.steps.iter_mut().take(3) {
            step.status = RunStepStatus::Done;
        }
        record.state = RunState::Interrupted;
        ops.exists(
            &Intent::Connector {
                name: "example-com".into(),
            },
            RunResource::Connector {
                id: 40,
                name: "example-com".into(),
            },
        );
        let record = resume(&input, record, &ops, &Saved::default(), timing()).await;
        assert_eq!(record.state, RunState::Succeeded);
        assert!(!ops.calls().contains(&"create_tunnel".to_string()));
        assert!(!ops.calls().contains(&"save_dns_provider".to_string()));
        // The found tunnel carries on: it is routed and waited for.
        assert!(ops.calls().contains(&"route_hostname 40".to_string()));
        assert_eq!(record.created().len(), 5);
        assert!(!record.has_pending());
    }

    #[tokio::test]
    async fn the_reconciler_resolves_pending_entries_by_lookup() {
        let ops = Fake::default();
        let tunnel = Intent::Connector { name: "t".into() };
        let domain = Intent::CustomDomain {
            domain: "myapp.example.com".into(),
            environment_id: 7,
        };
        ops.exists(
            &tunnel,
            RunResource::Connector {
                id: 40,
                name: "t".into(),
            },
        );
        let pending = |intent: Intent| JournalEntry {
            key: intent.key(0),
            intent,
            state: JournalState::Pending,
            resource: None,
            at: chrono::Utc::now(),
        };
        let mut journal = vec![pending(tunnel), pending(domain)];
        assert_eq!(reconcile_journal(&mut journal, &ops).await, Ok(2));
        assert_eq!(journal[0].state, JournalState::Done);
        assert_eq!(journal[1].state, JournalState::Absent);

        // When the lookup itself fails the entry stays pending.
        let broken = Fake {
            fail: vec!["find"],
            ..Default::default()
        };
        let mut journal = vec![pending(Intent::Connector { name: "t".into() })];
        assert!(reconcile_journal(&mut journal, &broken).await.is_err());
        assert_eq!(journal[0].state, JournalState::Pending);
    }

    #[tokio::test]
    async fn a_confirmed_route_records_and_undo_restores_the_previous_choice() {
        let ops = Fake {
            choice: Some(None),
            reachable: true,
            ..Default::default()
        };
        let input = RunInput {
            option: PlanOptionKind::ExistingTunnel,
            steps: steps(&[
                StepKind::AttachDomain,
                StepKind::RouteHostname,
                StepKind::WaitForPublication,
                StepKind::CheckReachability,
            ]),
            tunnel: Some(NamedId {
                id: 41,
                name: "homelab".into(),
            }),
            new_tunnel: None,
            token: None,
            route_confirmed: true,
            ..token_input()
        };
        let mut record = execute(&input, &ops, &Saved::default(), timing()).await;
        assert_eq!(record.state, RunState::Succeeded);
        assert!(record.created().contains(&RunResource::HostnameChoice {
            hostname: "myapp.example.com".into(),
            connector_id: 41,
            previous: PreviousChoice::Off,
        }));
        let results = undo(&mut record.journal, &ops).await;
        assert!(results.iter().all(|r| r.ok));
        assert!(ops
            .calls()
            .contains(&"restore_hostname_choice myapp.example.com Off".to_string()));
        assert!(record.created().is_empty());

        // Unconfirmed, another choice is never overridden.
        let ops = Fake {
            choice: Some(None),
            ..Default::default()
        };
        let record = execute(
            &RunInput {
                route_confirmed: false,
                ..input
            },
            &ops,
            &Saved::default(),
            timing(),
        )
        .await;
        assert_eq!(record.state, RunState::Failed);
        assert!(!ops.calls().iter().any(|c| c.starts_with("route_hostname")));
    }

    #[test]
    fn the_token_is_never_serialized_with_the_input() {
        let input = token_input();
        let json = serde_json::to_string(&input).expect("json");
        assert!(!json.contains("secret-token"));
        let back: RunInput = serde_json::from_str(&json).expect("input");
        assert_eq!(back.token, None);
        assert_eq!(back.address, input.address);
    }
}
