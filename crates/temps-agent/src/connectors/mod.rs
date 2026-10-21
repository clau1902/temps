// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ingress connectors on the node (ADR 058).
//!
//! The control plane says *what* to run ([`wire::NodeConnectorSpec`]); this
//! module decides *how*. A [`ConnectorDriver`] knows one way of running a
//! connector: a pinned binary as a child process, an image as a container, or
//! a connector compiled into the agent. The [`ConnectorSupervisor`] is the
//! same for all of them: it picks the first driver in the spec's preference
//! list this node supports, restarts with backoff, and reports status. No
//! driver reimplements any of that.
//!
//! Traffic reaches the node's edge through an [`EdgeHandoff`]: process and
//! container drivers forward to the loopback tunnel origin listener, which
//! trusts the provider's client-address header from loopback only.

pub mod process;
pub mod sync;
pub mod wire;

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::route_store::SharedRouteStore;
use wire::{ConnectorStatus, DriverChoice, NodeConnectorSpec};

/// How a driver runs a connector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverKind {
    Process,
    Container,
    Library(String),
}

impl DriverKind {
    pub fn label(&self) -> String {
        match self {
            DriverKind::Process => "process".into(),
            DriverKind::Container => "container".into(),
            DriverKind::Library(name) => format!("library:{name}"),
        }
    }
}

/// How visitor traffic reaches the node's edge from a running connector.
pub enum EdgeHandoff {
    /// The connector forwards plain HTTP to the loopback tunnel origin
    /// listener; `client_address_header` is trusted from loopback only.
    Loopback {
        addr: SocketAddr,
        client_address_header: Option<String>,
    },
    /// A compiled-in connector hands accepted streams over directly, with the
    /// visitor's real address. Reserved for the first library driver.
    Direct(tokio::sync::mpsc::Sender<DirectConnection>),
}

pub struct DirectConnection {
    pub stream: tokio::net::TcpStream,
    pub client_address: Option<IpAddr>,
}

/// Everything a driver needs to start one connector.
pub struct ResolvedSpec {
    pub connector_id: i32,
    pub provider: String,
    pub choice: DriverChoice,
    pub secrets: std::collections::BTreeMap<String, String>,
    /// Per-connector directory for secret files and state.
    pub work_dir: PathBuf,
    /// Where downloaded artifacts are cached, by sha256.
    pub bin_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub enum ConnectorHealth {
    Starting,
    Ready {
        connections: Option<i32>,
        connector_ref: Option<String>,
    },
    Degraded(String),
    Exited(String),
}

#[async_trait]
pub trait ConnectorDriver: Send + Sync {
    fn kind(&self) -> DriverKind;

    /// Whether this driver handles `choice` at all.
    fn accepts(&self, choice: &DriverChoice) -> bool;

    /// Whether this node can run `choice` with this driver; the reason when
    /// it cannot, for the operator.
    fn available(&self, choice: &DriverChoice) -> Result<(), String>;

    async fn start(
        &self,
        spec: &ResolvedSpec,
        edge: &EdgeHandoff,
    ) -> Result<Box<dyn RunningConnector>, String>;
}

#[async_trait]
pub trait RunningConnector: Send {
    async fn health(&mut self) -> ConnectorHealth;
    async fn stop(self: Box<Self>, grace: Duration);
}

struct Running {
    revision: String,
    stop: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
    status: Arc<Mutex<ConnectorStatus>>,
}

pub struct ConnectorSupervisor {
    drivers: Vec<Arc<dyn ConnectorDriver>>,
    data_dir: PathBuf,
    store: SharedRouteStore,
    shutdown: Arc<Notify>,
    running: tokio::sync::Mutex<HashMap<i32, Running>>,
    listeners: tokio::sync::Mutex<HashSet<u16>>,
}

impl ConnectorSupervisor {
    pub fn new(data_dir: PathBuf, store: SharedRouteStore, shutdown: Arc<Notify>) -> Arc<Self> {
        let drivers: Vec<Arc<dyn ConnectorDriver>> = vec![Arc::new(process::ProcessDriver::new())];
        Arc::new(Self {
            drivers,
            data_dir,
            store,
            shutdown,
            running: tokio::sync::Mutex::new(HashMap::new()),
            listeners: tokio::sync::Mutex::new(HashSet::new()),
        })
    }

    /// Converge on `specs`: start new connectors, restart changed ones and
    /// stop the ones no longer assigned.
    pub async fn apply(self: &Arc<Self>, specs: Vec<NodeConnectorSpec>) {
        let wanted: HashMap<i32, NodeConnectorSpec> =
            specs.into_iter().map(|spec| (spec.id, spec)).collect();
        let mut running = self.running.lock().await;

        let stale: Vec<i32> = running
            .iter()
            .filter(|(id, run)| {
                wanted
                    .get(id)
                    .is_none_or(|spec| spec.revision != run.revision)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(run) = running.remove(&id) {
                info!(connector_id = id, "stopping ingress connector");
                run.stop.notify_one();
                let _ = run.task.await;
            }
        }

        for (id, spec) in wanted {
            if running.contains_key(&id) {
                continue;
            }
            if let Err(error) = self.ensure_listener(&spec).await {
                warn!(connector_id = id, %error, "tunnel origin listener failed to bind");
            }
            let status = Arc::new(Mutex::new(ConnectorStatus {
                connector_id: id,
                driver: None,
                state: "starting".into(),
                ready_connections: None,
                connector_ref: None,
                last_error: None,
                restarts: 0,
            }));
            let stop = Arc::new(Notify::new());
            let revision = spec.revision.clone();
            let task = tokio::spawn(Arc::clone(self).run_connector(
                spec,
                Arc::clone(&stop),
                Arc::clone(&status),
            ));
            running.insert(
                id,
                Running {
                    revision,
                    stop,
                    task,
                    status,
                },
            );
        }
    }

    pub async fn statuses(&self) -> Vec<ConnectorStatus> {
        self.running
            .lock()
            .await
            .values()
            .map(|run| run.status.lock().clone())
            .collect()
    }

    async fn ensure_listener(&self, spec: &NodeConnectorSpec) -> std::io::Result<()> {
        let mut listeners = self.listeners.lock().await;
        if listeners.contains(&spec.edge_port) {
            return Ok(());
        }
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), spec.edge_port);
        crate::internal_proxy::spawn_tunnel_origin(
            addr,
            self.store.clone(),
            self.shutdown.clone(),
            spec.client_address_header.clone(),
        )
        .await?;
        info!(%addr, "tunnel origin listener started");
        listeners.insert(spec.edge_port);
        Ok(())
    }

    fn choose(
        &self,
        spec: &NodeConnectorSpec,
    ) -> Result<(Arc<dyn ConnectorDriver>, DriverChoice), String> {
        let mut reasons = Vec::new();
        for choice in &spec.drivers {
            for driver in &self.drivers {
                if !driver.accepts(choice) {
                    continue;
                }
                match driver.available(choice) {
                    Ok(()) => return Ok((Arc::clone(driver), choice.clone())),
                    Err(reason) => reasons.push(format!("{}: {reason}", driver.kind().label())),
                }
            }
        }
        if reasons.is_empty() {
            reasons.push("this agent has no driver for any of the offered kinds".into());
        }
        Err(format!(
            "No driver on this node can run the {} connector ({}). Upgrade the agent.",
            spec.provider,
            reasons.join("; ")
        ))
    }

    async fn run_connector(
        self: Arc<Self>,
        spec: NodeConnectorSpec,
        stop: Arc<Notify>,
        status: Arc<Mutex<ConnectorStatus>>,
    ) {
        let id = spec.id;
        let set = |state: &str, error: Option<String>| {
            let mut s = status.lock();
            s.state = state.into();
            if error.is_some() {
                s.last_error = error;
            }
        };
        let edge = EdgeHandoff::Loopback {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), spec.edge_port),
            client_address_header: spec.client_address_header.clone(),
        };
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = match self.choose(&spec) {
                Err(reason) => Err(reason),
                Ok((driver, choice)) => {
                    status.lock().driver = Some(driver.kind().label());
                    let resolved = ResolvedSpec {
                        connector_id: id,
                        provider: spec.provider.clone(),
                        choice,
                        secrets: spec.secrets.clone(),
                        work_dir: self.data_dir.join("connectors").join(id.to_string()),
                        bin_dir: self.data_dir.join("connectors").join("bin"),
                    };
                    set("starting", None);
                    driver.start(&resolved, &edge).await
                }
            };
            match started {
                Err(reason) => {
                    warn!(connector_id = id, %reason, "ingress connector could not start");
                    set("down", Some(reason));
                }
                Ok(mut running) => {
                    info!(connector_id = id, "ingress connector started");
                    loop {
                        tokio::select! {
                            _ = stop.notified() => {
                                running.stop(Duration::from_secs(35)).await;
                                return;
                            }
                            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                        }
                        match running.health().await {
                            ConnectorHealth::Starting => set("starting", None),
                            ConnectorHealth::Ready {
                                connections,
                                connector_ref,
                            } => {
                                let mut s = status.lock();
                                if s.state != "ready" {
                                    info!(
                                        connector_id = id,
                                        ?connections,
                                        "ingress connector ready"
                                    );
                                }
                                s.state = "ready".into();
                                s.ready_connections = connections;
                                s.connector_ref = connector_ref;
                                s.last_error = None;
                                backoff = Duration::from_secs(1);
                            }
                            ConnectorHealth::Degraded(reason) => set("degraded", Some(reason)),
                            ConnectorHealth::Exited(reason) => {
                                warn!(connector_id = id, %reason, "ingress connector exited");
                                set("down", Some(reason));
                                break;
                            }
                        }
                    }
                }
            }
            status.lock().restarts += 1;
            tokio::select! {
                _ = stop.notified() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }
}
