// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Whether each address of an environment is reachable, from facts the
//! service gathered (ADR 064 D3). Pure: no database, no network.
//!
//! Reachability is per address. Under ADR-020 any ingress serves any app and
//! forwards over the mesh, so the test is "does a way in serve this hostname,
//! and does DNS lead there", never "does the worker it runs on have one".

use std::net::IpAddr;

use temps_core::worker_ingress::{address_scope, AddressScope};

use super::model::{
    ActionKind, AddressReachability, AddressSource, Audience, ReachAction, ReachState,
};

/// The ingress setup page, where tunnels and worker ingress are managed.
pub const INGRESS_SETUP_PATH: &str = "/settings/nodes/ingress";

/// One address to judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressInput {
    pub address: String,
    pub audience: Audience,
    pub source: AddressSource,
    pub custom_domain_id: Option<i32>,
}

/// A way in that is a listener of Temps' own: the control plane's proxy or
/// a worker's direct ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectIngress {
    /// `control-plane`, or the worker's name.
    pub name: String,
    /// `None` for the control plane.
    pub node_id: Option<i32>,
    /// Addresses DNS should point at to reach it.
    pub ips: Vec<IpAddr>,
    /// Whether it answers plain HTTP on port 80, which HTTP-01 needs.
    pub takes_port_80: bool,
    /// The last reachability check from the control plane failed, and why.
    pub check_failed: Option<String>,
}

/// ADR 058 D9's per-hostname tunnel state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelState {
    /// `published`, `pending`, `error` or `not_published`.
    pub state: String,
    pub connector_name: Option<String>,
    pub detail: String,
    /// Whether some tunnel's domain covers it at all.
    pub has_candidates: bool,
}

/// What DNS answered for the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Ips(Vec<IpAddr>),
    NoRecord,
    Failed(String),
    /// Not looked up: the address was settled without DNS.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressFacts {
    pub input: AddressInput,
    pub tunnel: Option<TunnelState>,
    pub resolution: Resolution,
    /// Temps manages a DNS record for it (outside tunnels).
    pub temps_managed_dns: bool,
}

/// Names that only ever resolve to the machine they are opened on.
pub fn is_local_only(address: &str) -> bool {
    let host = address.trim_end_matches('.').to_ascii_lowercase();
    ["localhost", "localho.st"]
        .iter()
        .any(|base| host == *base || host.ends_with(&format!(".{base}")))
}

fn ip_list(ips: &[IpAddr]) -> String {
    ips.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn make_reachable() -> ReachAction {
    ReachAction::new(ActionKind::MakeReachable, "Make it reachable")
}

/// Whether DNS has to be looked up to judge this address: not for local
/// names, undeployed environments or tunnel-settled ones.
pub fn needs_lookup(address: &str, deployed: bool, tunnel: Option<&TunnelState>) -> bool {
    !is_local_only(address)
        && deployed
        && !matches!(
            tunnel.map(|t| t.state.as_str()),
            Some("published" | "pending" | "error")
        )
}

/// Judge one address.
pub fn address_reachability(
    facts: &AddressFacts,
    directs: &[DirectIngress],
    deployed: bool,
    environment_name: &str,
) -> AddressReachability {
    let input = &facts.input;
    let address = input.address.as_str();
    let verdict = |state: ReachState,
                   way_in: Option<String>,
                   reason: String,
                   action: Option<ReachAction>| AddressReachability {
        address: input.address.clone(),
        audience: input.audience,
        source: input.source,
        custom_domain_id: input.custom_domain_id,
        way_in,
        state,
        reason,
        action,
    };

    if is_local_only(address) {
        return verdict(
            ReachState::NotReachable,
            None,
            format!(
                "{address} only resolves to the computer it is opened on, so nobody else can \
                 reach it."
            ),
            Some(make_reachable()),
        );
    }
    if !deployed {
        return verdict(
            ReachState::NotReachable,
            None,
            format!("Nothing is deployed to {environment_name} yet."),
            Some(ReachAction::new(ActionKind::Deploy, "Deploy it")),
        );
    }

    let mut tunnel_reason = None;
    if let Some(tunnel) = &facts.tunnel {
        let way_in = tunnel
            .connector_name
            .as_ref()
            .map(|name| format!("tunnel:{name}"));
        match tunnel.state.as_str() {
            "published" => {
                return verdict(ReachState::Reachable, way_in, tunnel.detail.clone(), None)
            }
            "pending" => {
                return verdict(
                    ReachState::Checking,
                    way_in,
                    tunnel.detail.clone(),
                    Some(ReachAction::new(ActionKind::Wait, "Wait a moment")),
                )
            }
            "error" => {
                return verdict(
                    ReachState::NotReachable,
                    way_in,
                    tunnel.detail.clone(),
                    Some(
                        ReachAction::new(ActionKind::OpenIngress, "Open the tunnel")
                            .with_href(INGRESS_SETUP_PATH),
                    ),
                )
            }
            // A tunnel covers it but does not publish it: that is the reason
            // if nothing else serves it either.
            _ if tunnel.has_candidates => tunnel_reason = Some(tunnel.detail.clone()),
            _ => {}
        }
    }

    let ips = match &facts.resolution {
        Resolution::Ips(ips) if !ips.is_empty() => ips,
        Resolution::Failed(error) => {
            return verdict(
                ReachState::Unknown,
                None,
                format!("Could not look up {address}: {error}"),
                None,
            )
        }
        Resolution::Skipped => {
            return verdict(
                ReachState::Unknown,
                None,
                format!("{address} was not checked."),
                None,
            )
        }
        Resolution::Ips(_) | Resolution::NoRecord => {
            let reason = tunnel_reason
                .unwrap_or_else(|| format!("No DNS record for {address} exists yet."));
            let target = directs
                .iter()
                .find(|d| d.check_failed.is_none() && !d.ips.is_empty());
            let action = match target {
                Some(direct) => Some(
                    ReachAction::new(
                        ActionKind::SetDnsRecord,
                        format!(
                            "Add an A record for {address} pointing at {} ({})",
                            direct.ips[0], direct.name
                        ),
                    ),
                ),
                None => Some(make_reachable()),
            };
            return verdict(ReachState::NotReachable, None, reason, action);
        }
    };

    if let Some(direct) = directs
        .iter()
        .find(|d| d.ips.iter().any(|ip| ips.contains(ip)))
    {
        let way_in = Some(format!("direct:{}", direct.name));
        return match &direct.check_failed {
            Some(why) => verdict(
                ReachState::NotReachable,
                way_in,
                format!(
                    "DNS points at {} ({}), but it failed its last reachability check: {why}",
                    direct.name,
                    ip_list(ips)
                ),
                Some(
                    ReachAction::new(ActionKind::OpenWorker, format!("Check {}", direct.name))
                        .with_href(INGRESS_SETUP_PATH),
                ),
            ),
            None => verdict(
                ReachState::Reachable,
                way_in,
                format!("DNS points at {} ({}).", direct.name, ip_list(ips)),
                None,
            ),
        };
    }

    if ips
        .iter()
        .all(|ip| address_scope(*ip) != AddressScope::Public)
    {
        return verdict(
            ReachState::NotReachable,
            None,
            format!(
                "{address} points at {}, a private address nobody on the internet can reach.",
                ip_list(ips)
            ),
            Some(make_reachable()),
        );
    }

    if facts.temps_managed_dns {
        return verdict(
            ReachState::Reachable,
            Some("direct".to_string()),
            format!(
                "Its DNS record is managed by Temps and points at {}.",
                ip_list(ips)
            ),
            None,
        );
    }

    verdict(
        ReachState::Unknown,
        None,
        format!(
            "{address} points at {}, which is not this control plane or a worker Temps knows. \
             It works if what answers there forwards to Temps.",
            ip_list(ips)
        ),
        None,
    )
}

/// The environment's overall state: reachable when any address is.
pub fn overall_state(addresses: &[AddressReachability]) -> ReachState {
    let any = |state: ReachState| addresses.iter().any(|a| a.state == state);
    if any(ReachState::Reachable) {
        ReachState::Reachable
    } else if any(ReachState::Checking) {
        ReachState::Checking
    } else if any(ReachState::Unknown) {
        ReachState::Unknown
    } else {
        ReachState::NotReachable
    }
}

/// The header's line. Custom domains come before the generated hostname,
/// which is usually the less memorable one.
pub fn summary(
    addresses: &[AddressReachability],
    deployed: bool,
    environment_name: &str,
    running_on: &[String],
) -> String {
    let rank = |a: &AddressReachability| match a.source {
        AddressSource::CustomDomain => 0,
        AddressSource::EnvironmentDomain => 1,
        AddressSource::Generated => 2,
    };
    let best = |state: ReachState| {
        addresses
            .iter()
            .filter(|a| a.state == state)
            .min_by_key(|a| rank(a))
    };
    if let Some(address) = best(ReachState::Reachable) {
        return format!("Public at {}", address.address);
    }
    if let Some(address) = best(ReachState::Checking) {
        return format!("Being set up at {}: {}", address.address, address.reason);
    }
    if !deployed {
        return format!("Nothing is deployed to {environment_name} yet.");
    }
    let place = match running_on {
        [] => String::new(),
        [one] => format!(" on {one}"),
        many => format!(" on {}", many.join(", ")),
    };
    if let Some(address) = best(ReachState::Unknown) {
        return format!(
            "Deployed{place}; Temps cannot tell whether {} is reachable: {}",
            address.address, address.reason
        );
    }
    match addresses.iter().min_by_key(|a| rank(a)) {
        Some(address) => format!(
            "Deployed{place}, but nobody can reach it yet: {}",
            address.reason
        ),
        None => format!("Deployed{place}, but it has no address yet."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(address: &str, source: AddressSource) -> AddressInput {
        AddressInput {
            address: address.to_string(),
            audience: Audience::Public,
            source,
            custom_domain_id: None,
        }
    }

    fn facts(address: &str, resolution: Resolution) -> AddressFacts {
        AddressFacts {
            input: input(address, AddressSource::CustomDomain),
            tunnel: None,
            resolution,
            temps_managed_dns: false,
        }
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().expect("test IP")
    }

    fn control_plane() -> DirectIngress {
        DirectIngress {
            name: "control-plane".into(),
            node_id: None,
            ips: vec![ip("203.0.113.10")],
            takes_port_80: true,
            check_failed: None,
        }
    }

    #[test]
    fn a_localho_st_preview_is_never_reachable() {
        // Even when something resolved it.
        let verdict = address_reachability(
            &AddressFacts {
                input: input("app-production.localho.st", AddressSource::Generated),
                tunnel: None,
                resolution: Resolution::Ips(vec![ip("127.0.0.1")]),
                temps_managed_dns: false,
            },
            &[control_plane()],
            true,
            "production",
        );
        assert_eq!(verdict.state, ReachState::NotReachable);
        assert_eq!(
            verdict.action.map(|a| a.kind),
            Some(ActionKind::MakeReachable)
        );
        assert!(is_local_only("localhost"));
        assert!(is_local_only("x.localhost."));
        assert!(!is_local_only("localho.st.example.com"));
    }

    #[test]
    fn nothing_deployed_is_not_reachable_with_deploy_as_the_fix() {
        let verdict = address_reachability(
            &facts("app.example.com", Resolution::Skipped),
            &[],
            false,
            "production",
        );
        assert_eq!(verdict.state, ReachState::NotReachable);
        assert_eq!(verdict.reason, "Nothing is deployed to production yet.");
        assert_eq!(verdict.action.map(|a| a.kind), Some(ActionKind::Deploy));
    }

    #[test]
    fn the_tunnel_state_decides_a_tunneled_address() {
        let tunneled = |state: &str| AddressFacts {
            tunnel: Some(TunnelState {
                state: state.into(),
                connector_name: Some("homelab".into()),
                detail: format!("{state} detail"),
                has_candidates: true,
            }),
            ..facts("app.example.com", Resolution::Skipped)
        };
        let published = address_reachability(&tunneled("published"), &[], true, "production");
        assert_eq!(published.state, ReachState::Reachable);
        assert_eq!(published.way_in.as_deref(), Some("tunnel:homelab"));
        assert_eq!(
            address_reachability(&tunneled("pending"), &[], true, "production").state,
            ReachState::Checking
        );
        let error = address_reachability(&tunneled("error"), &[], true, "production");
        assert_eq!(error.state, ReachState::NotReachable);
        assert_eq!(error.reason, "error detail");
        assert!(!needs_lookup("app.example.com", true, tunneled("pending").tunnel.as_ref()));
        assert!(needs_lookup("app.example.com", true, tunneled("not_published").tunnel.as_ref()));
    }

    #[test]
    fn a_covering_tunnel_that_does_not_publish_explains_a_missing_record() {
        let verdict = address_reachability(
            &AddressFacts {
                tunnel: Some(TunnelState {
                    state: "not_published".into(),
                    connector_name: None,
                    detail: "Tunnel 'homelab' covers it but only publishes hostnames chosen for it.".into(),
                    has_candidates: true,
                }),
                ..facts("app.example.com", Resolution::NoRecord)
            },
            &[],
            true,
            "production",
        );
        assert_eq!(verdict.state, ReachState::NotReachable);
        assert!(verdict.reason.starts_with("Tunnel 'homelab' covers it"));
    }

    #[test]
    fn a_single_box_install_is_reachable_through_its_own_proxy() {
        let verdict = address_reachability(
            &facts("app.example.com", Resolution::Ips(vec![ip("203.0.113.10")])),
            &[control_plane()],
            true,
            "production",
        );
        assert_eq!(verdict.state, ReachState::Reachable);
        assert_eq!(verdict.way_in.as_deref(), Some("direct:control-plane"));
    }

    #[test]
    fn a_worker_that_failed_its_check_is_not_reachable() {
        let worker = DirectIngress {
            name: "edge-1".into(),
            node_id: Some(4),
            ips: vec![ip("198.51.100.7")],
            takes_port_80: true,
            check_failed: Some("port 443 timed out".into()),
        };
        let verdict = address_reachability(
            &facts("app.example.com", Resolution::Ips(vec![ip("198.51.100.7")])),
            &[worker],
            true,
            "production",
        );
        assert_eq!(verdict.state, ReachState::NotReachable);
        assert_eq!(verdict.way_in.as_deref(), Some("direct:edge-1"));
        assert!(verdict.reason.contains("port 443 timed out"));
    }

    #[test]
    fn dns_answers_without_a_known_ingress() {
        let private = address_reachability(
            &facts("app.example.com", Resolution::Ips(vec![ip("192.168.1.20")])),
            &[control_plane()],
            true,
            "production",
        );
        assert_eq!(private.state, ReachState::NotReachable);
        assert!(private.reason.contains("private address"));

        let elsewhere = address_reachability(
            &facts("app.example.com", Resolution::Ips(vec![ip("93.184.215.14")])),
            &[control_plane()],
            true,
            "production",
        );
        assert_eq!(elsewhere.state, ReachState::Unknown);

        let managed = address_reachability(
            &AddressFacts {
                temps_managed_dns: true,
                ..facts("app.example.com", Resolution::Ips(vec![ip("104.16.1.1")]))
            },
            &[control_plane()],
            true,
            "production",
        );
        assert_eq!(managed.state, ReachState::Reachable);

        let missing = address_reachability(
            &facts("app.example.com", Resolution::NoRecord),
            &[control_plane()],
            true,
            "production",
        );
        assert_eq!(missing.state, ReachState::NotReachable);
        let action = missing.action.expect("an action");
        assert_eq!(action.kind, ActionKind::SetDnsRecord);
        assert!(action.label.contains("203.0.113.10"));
    }

    #[test]
    fn the_summary_names_the_best_address_or_the_reason() {
        let reachable = |address: &str, source| AddressReachability {
            address: address.into(),
            audience: Audience::Public,
            source,
            custom_domain_id: None,
            way_in: None,
            state: ReachState::Reachable,
            reason: String::new(),
            action: None,
        };
        let both = vec![
            reachable("app-production.example.com", AddressSource::Generated),
            reachable("myapp.example.com", AddressSource::CustomDomain),
        ];
        assert_eq!(overall_state(&both), ReachState::Reachable);
        assert_eq!(
            summary(&both, true, "production", &[]),
            "Public at myapp.example.com"
        );

        let local = vec![AddressReachability {
            state: ReachState::NotReachable,
            reason: "x only resolves to the computer it is opened on.".into(),
            ..reachable("x.localho.st", AddressSource::Generated)
        }];
        assert_eq!(overall_state(&local), ReachState::NotReachable);
        assert_eq!(
            summary(&local, true, "production", &["master".to_string()]),
            "Deployed on master, but nobody can reach it yet: x only resolves to the computer it is opened on."
        );
        assert_eq!(
            summary(&[], true, "production", &[]),
            "Deployed, but it has no address yet."
        );
    }
}
