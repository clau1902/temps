// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tailscale ways in for a private address (ADR 065): what the agent and the
//! control plane must agree on.
//!
//! The agent drives tailscaled on its own machine through the LocalAPI, and
//! only with the worker owner's consent (`--tailscale-serve` in
//! `agent.json`). It reports what it found ([`TailscaleReport`]), what it did
//! with each Serve entry the control plane declared ([`ServeEntryReport`]),
//! and its inside checks of the tailnet names it serves ([`TailnetCheck`]).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// tailscaled's LocalAPI socket on Linux.
pub const TAILSCALED_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// The directory holding the per-address Serve sockets (ADR 065 D3), `0700`
/// and owned by root: only tailscaled (root) can connect to them. A Serve
/// handler whose target is a socket here is the agent's; every other one is
/// not.
pub const SERVE_SOCKET_DIR: &str = "/run/temps/serve";

/// The oldest tailscaled that can point Serve at a unix socket (v1.94.0).
pub const MIN_TAILSCALED: (u32, u32) = (1, 94);

/// What the worker owner runs to allow the agent to drive tailscaled.
pub const TAILSCALE_SERVE_COMMAND: &str =
    "temps agent ingress private --listen tailscale --tailscale-serve";

/// The capability an agent reports when it enforces `tailnet_users` (ADR 065
/// D6). A route with that audience is never sent to an agent without it.
pub const CAPABILITY_TAILNET_IDENTITY: &str = "tailnet_identity.v1";

/// The port every tailnet name is served on.
pub const SERVE_HTTPS_PORT: u16 = 443;

/// What the agent's inside check asks a Serve socket for (ADR 065 D5 step 5):
/// answered by the socket itself, never the app, and only to a request
/// without `X-Forwarded-For`, which tailscaled always sets.
pub const SERVE_PROBE_PATH: &str = "/.well-known/temps/serve-probe";

/// Set to the app by a Serve socket with the visitor's tailnet login (ADR
/// 065 D6). Removed from every incoming request on every listener.
pub const TAILNET_LOGIN_HEADER: &str = "x-temps-user-login";

/// Whether a request header carries tailnet identity (ADR 065 D3): every
/// `Tailscale-*` header and Temps' own login header. Lower-case `name`.
pub fn is_identity_header(name: &str) -> bool {
    name.starts_with("tailscale-") || name == TAILNET_LOGIN_HEADER
}

/// The identity headers tailscaled replaces on every request it serves
/// (`addTailscaleIdentityHeaders`): it deletes any client copy and sets its
/// own. Any other `Tailscale-User-*` name is the client's, not tailscaled's.
pub const SERVE_IDENTITY_HEADERS: [&str; 3] = [
    "tailscale-user-login",
    "tailscale-user-name",
    "tailscale-user-profile-pic",
];

/// Whether an incoming identity header is passed to the app: only the
/// [`SERVE_IDENTITY_HEADERS`] tailscaled sets, and only on a Serve socket.
/// Anything else that is not an identity header always passes.
pub fn forwards_header(name: &str, serve_socket: bool) -> bool {
    !is_identity_header(name) || (serve_socket && SERVE_IDENTITY_HEADERS.contains(&name))
}

/// The tailscaled resolver, asked directly by the inside check.
pub const MAGIC_DNS_RESOLVER: &str = "100.100.100.100";

/// Tailscale's IPv6 range, next to [`crate::worker_ingress::TAILSCALE_RANGE`].
pub const TAILSCALE_RANGE_V6: &str = "fd7a:115c:a1e0::/48";

/// The socket file for an address. Address ids are `custom:<id>` or
/// `environment:<id>`; the colon becomes `-` so the path never needs quoting
/// in a Serve target. `None` for anything else, so a malformed id can never
/// name a path outside [`SERVE_SOCKET_DIR`].
pub fn serve_socket_path(address_id: &str) -> Option<std::path::PathBuf> {
    let (kind, id) = address_id.split_once(':')?;
    if !matches!(kind, "custom" | "environment")
        || id.is_empty()
        || id.len() > 10
        || !id.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(std::path::Path::new(SERVE_SOCKET_DIR).join(format!("{kind}-{id}.sock")))
}

/// The Serve proxy target for an address: `unix:<socket path>`.
pub fn serve_target(address_id: &str) -> Option<String> {
    serve_socket_path(address_id).map(|path| format!("unix:{}", path.display()))
}

/// Whether a Serve proxy target is one of the agent's own sockets: the only
/// handlers it ever edits or removes (ADR 065 D2).
pub fn is_owned_target(target: &str) -> bool {
    is_owned_target_in(SERVE_SOCKET_DIR, target)
}

/// [`is_owned_target`] for sockets under `dir` (the agent's tests use a
/// temporary directory).
pub fn is_owned_target_in(dir: &str, target: &str) -> bool {
    target
        .strip_prefix("unix:")
        .and_then(|path| path.strip_prefix(dir.trim_end_matches('/')))
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| {
            !name.is_empty()
                && !name.contains('/')
                && name.ends_with(".sock")
                && !name.contains("..")
        })
}

/// `1.94.1-t0123…` → `(1, 94)`. `None` when it is not a version.
pub fn parse_version(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim().split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Whether this tailscaled can point Serve at a socket.
pub fn version_supported(version: &str) -> bool {
    parse_version(version).is_some_and(|v| v >= MIN_TAILSCALED)
}

/// Whether `ip` is a Tailscale node or Service address.
pub fn is_tailscale_ip(ip: std::net::IpAddr) -> bool {
    crate::worker_ingress::ip_in_range(ip, crate::worker_ingress::TAILSCALE_RANGE)
        || crate::worker_ingress::ip_in_range(ip, TAILSCALE_RANGE_V6)
}

/// A machine name or Service name as a DNS name: lower-case, no trailing dot.
pub fn normalize_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether `name` is usable as a Service name (`svc:` prefix optional): one
/// DNS label, lower-case letters, digits and `-`, not starting or ending with
/// `-`, at most 63 characters.
pub fn valid_service_label(name: &str) -> bool {
    let label = name.strip_prefix("svc:").unwrap_or(name);
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Where the agent stands with tailscaled, decided in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum TailscaleState {
    /// The worker owner has not allowed it: the agent never opened the
    /// socket.
    #[default]
    NoConsent,
    /// The agent is not root, so tailscaled would refuse a socket target.
    NotRoot,
    /// No tailscaled socket on this machine.
    NoSocket,
    /// tailscaled answered something this agent does not understand.
    NotUnderstood,
    /// tailscaled older than [`MIN_TAILSCALED`].
    Unsupported,
    /// tailscaled answers, at a supported version.
    Ready,
}

/// What the agent found about its tailscaled (ADR 065 D2), reported with the
/// listener report. Only `consent`, `root` and `state` are meaningful
/// without consent: the agent never opens the socket then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
pub struct TailscaleReport {
    /// `--tailscale-serve` in `agent.json`.
    pub consent: bool,
    pub root: bool,
    /// The user the agent runs as, when not root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub state: TailscaleState,
    /// The failure message for `state`, or for a LocalAPI call that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `BackendState`: `Running`, `NeedsLogin`, `Stopped`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_state: Option<String>,
    /// `Self.DNSName`, normalized: `master.example.ts.net`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_name: Option<String>,
    /// `Self.ID`, the stable node id the Tailscale API lists a Service's
    /// hosts by (ADR 065 D4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// The MagicDNS suffix: `example.ts.net`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<String>,
    #[serde(default)]
    pub magic_dns: bool,
    /// `CertDomains`, normalized. HTTPS certificates are on when not empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cert_domains: Vec<String>,
    /// `Self.Tags`. A device signed in as a user has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The login the device is signed in as, when it has no tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// `Self.TailscaleIPs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    /// What holds `<machine>:443` in the Serve config that the agent did not
    /// write, in words (`http://127.0.0.1:3000`). `None` when free or the
    /// agent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_443_held_by: Option<String>,
    /// Funnel is on for `<machine>:443`.
    #[serde(default)]
    pub port_443_funnel: bool,
    /// What the agent did with each declared Serve entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ServeEntryReport>,
    /// The inside checks of the tailnet names it serves (ADR 065 D5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<TailnetCheck>,
}

impl TailscaleReport {
    /// HTTPS certificates are on for the tailnet.
    pub fn https(&self) -> bool {
        !self.cert_domains.is_empty()
    }

    /// The machine can serve through Serve now: consent, root, a supported
    /// tailscaled that is running, MagicDNS and HTTPS on.
    pub fn serve_ready(&self) -> bool {
        self.consent
            && self.root
            && self.state == TailscaleState::Ready
            && self.backend_state.as_deref() == Some("Running")
            && self.magic_dns
            && self.https()
            && self.dns_name.is_some()
    }
}

/// What the agent did with one Serve entry (ADR 065 D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServeOutcome {
    /// Written, at `etag`.
    Applied,
    /// The slot holds something the agent did not write; nothing written.
    Conflict,
    /// The write failed; it may have landed.
    Error,
    /// Being removed: the agent removed its handler.
    Removed,
    /// Being removed: no handler of the agent's was there.
    Absent,
}

/// One entry's outcome, naming the row it is for: a report about an entry
/// that was since replaced is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ServeEntryReport {
    pub entry_id: i32,
    pub address_id: String,
    pub outcome: ServeOutcome,
    /// The Serve config `Etag` after the agent's write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// What holds the slot, or the error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Funnel is on for this entry's name, or a Funnel request reached its
    /// socket: the agent stopped serving it.
    #[serde(default)]
    pub funnel: bool,
}

/// How an inside check of a tailnet name came out (ADR 065 D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TailnetCheckOutcome {
    /// Every check passed.
    Reachable,
    /// tailscaled is not running or not supported.
    Tailscale,
    /// MagicDNS or HTTPS certificates are off, or the name is not covered.
    Certificates,
    /// The Serve entry is missing, changed, or not pointing at its socket.
    Entry,
    /// The name is on Funnel.
    Funnel,
    /// MagicDNS does not resolve the name.
    NotResolved,
    /// MagicDNS resolves it somewhere other than this worker or Service.
    ResolvesElsewhere,
    /// The socket is not bound, or does not route the address.
    NoRoute,
}

/// One inside check of a tailnet name, as the worker reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TailnetCheck {
    pub address_id: String,
    pub hostname: String,
    pub outcome: TailnetCheckOutcome,
    /// What happened, with what to do when it failed (ADR 065 D5's table).
    pub message: String,
}

/// The D5 failure messages, word for word, so the agent and the plan say
/// the same thing.
pub mod messages {
    pub fn no_socket(node: &str) -> String {
        format!(
            "Tailscale is not running on `{node}` (no `{}`). Install Tailscale and run `sudo \
             tailscale up`.",
            super::TAILSCALED_SOCKET
        )
    }

    pub fn not_logged_in(node: &str, state: &str) -> String {
        format!("Tailscale on `{node}` is `{state}`. Run `sudo tailscale up`.")
    }

    pub fn magic_dns_off(tailnet: &str) -> String {
        format!(
            "MagicDNS is off for tailnet `{tailnet}`. Turn it on at \
             login.tailscale.com/admin/dns, then turn on HTTPS certificates."
        )
    }

    pub fn https_off(tailnet: &str, name: &str) -> String {
        format!(
            "HTTPS certificates are off for tailnet `{tailnet}`, so Serve cannot give `{name}` a \
             certificate. Turn them on at login.tailscale.com/admin/dns. Machine names become \
             public in certificate logs."
        )
    }

    pub fn old_tailscaled(version: &str, node: &str) -> String {
        let (major, minor) = super::MIN_TAILSCALED;
        let shown = super::parse_version(version)
            .map(|(a, b)| format!("{a}.{b}"))
            .unwrap_or_else(|| version.to_string());
        format!(
            "tailscaled {shown} on `{node}` cannot send Serve traffic to a socket. Temps needs \
             {major}.{minor} or later."
        )
    }

    pub fn not_root(node: &str, user: &str) -> String {
        format!(
            "The Temps agent on `{node}` runs as `{user}`; Tailscale lets only root point Serve \
             at a socket."
        )
    }

    pub fn no_consent(node: &str) -> String {
        format!("Run `{}` on `{node}`.", super::TAILSCALE_SERVE_COMMAND)
    }

    pub fn not_tagged(node: &str, login: &str, machine_name: &str) -> String {
        format!(
            "`{node}` is signed in as {login}, and only tagged devices can host a Tailscale \
             Service. Run `sudo tailscale up --advertise-tags=tag:temps` (the tag must be in \
             your policy's `tagOwners`), or use the machine name `{machine_name}`."
        )
    }

    pub fn not_approved(service: &str, node: &str, tag: &str) -> String {
        format!(
            "`{service}` is advertised by `{node}` but not approved. Approve it at \
             login.tailscale.com/admin/services, or add `\"autoApprovers\": {{\"services\": \
             {{\"{service}\": [\"{tag}\"]}}}}` to your tailnet policy."
        )
    }

    pub fn slot_taken(name: &str, held_by: &str) -> String {
        format!(
            "`{name}` already serves `{held_by}` (set with `tailscale serve`, not by Temps). \
             Remove it with `tailscale serve --https=443 off`, or give this app a Service name."
        )
    }

    pub fn service_taken(service: &str) -> String {
        format!(
            "`{service}` already exists in your tailnet and Temps did not create it. Pick \
             another name."
        )
    }

    pub const CREDENTIAL_SCOPE: &str = "This Tailscale OAuth client cannot manage Services. \
         Create one with the `services` and `devices:core:read` scopes.";

    pub fn not_understood(version: &str, node: &str) -> String {
        let shown = super::parse_version(version)
            .map(|(a, b)| format!("{a}.{b}"))
            .unwrap_or_else(|| "1.x".to_string());
        format!(
            "tailscaled {shown} on `{node}` answered in a way this agent does not understand; \
             update Temps or report it"
        )
    }

    pub fn listener_exclusive(node: &str, listener: &str) -> String {
        format!(
            "`{node}` already serves private addresses on {listener}:443; give this app a \
             Service name instead"
        )
    }

    pub fn funnel(node: &str) -> String {
        format!(
            "This private address is also on Tailscale Funnel, which makes it public. Run \
             `tailscale funnel --https=443 off` on `{node}`."
        )
    }

    pub fn machine_name_cost(name: &str, node: &str) -> String {
        format!(
            "`{name}` becomes this app's address. Another app on `{node}` needs a Service name."
        )
    }

    /// Phase 2's plan, refusing the listener where Serve holds 443 (D1).
    pub fn serve_exclusive(node: &str, machine_name: &str, listener: &str) -> String {
        format!(
            "`{node}` serves `{machine_name}` with Tailscale Serve, which takes port 443 on its \
             Tailscale address before its private listener on {listener}:443 sees it. Give this \
             app a Service name instead, or bind the listener to another private address."
        )
    }

    /// A name under the tailnet that is not a machine's (D1).
    pub fn service_only(name: &str) -> String {
        format!(
            "`{name}` is not a machine's name in your tailnet, and only a Tailscale Service can \
             give an app any other name there. Use a machine's name, or a hostname on a domain \
             you own."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_paths_stay_in_the_serve_directory() {
        assert_eq!(
            serve_socket_path("custom:12").unwrap(),
            std::path::Path::new("/run/temps/serve/custom-12.sock")
        );
        assert_eq!(
            serve_target("environment:7").as_deref(),
            Some("unix:/run/temps/serve/environment-7.sock")
        );
        for bad in [
            "custom:../x",
            "custom:",
            "custom:1/2",
            "other:1",
            "custom",
            "custom:12345678901",
        ] {
            assert!(serve_socket_path(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn only_sockets_in_the_serve_directory_are_owned() {
        assert!(is_owned_target("unix:/run/temps/serve/custom-12.sock"));
        for foreign in [
            "http://127.0.0.1:3000",
            "unix:/run/temps/serve/../evil.sock",
            "unix:/run/temps/serve/sub/x.sock",
            "unix:/run/temps/servex/a.sock",
            "unix:/run/temps/serve/",
            "unix:/var/run/app.sock",
            "/run/temps/serve/custom-1.sock",
        ] {
            assert!(!is_owned_target(foreign), "{foreign}");
        }
    }

    #[test]
    fn versions_from_194_can_serve_to_a_socket() {
        assert!(version_supported("1.94.0"));
        assert!(version_supported("1.104.2-t5a2b1c-gdeadbeef"));
        assert!(version_supported("2.0.0"));
        assert!(!version_supported("1.92.3"));
        assert!(!version_supported("1.9"));
        assert!(!version_supported("unknown"));
        assert_eq!(parse_version("1.90.1-tabc"), Some((1, 90)));
    }

    #[test]
    fn messages_are_the_adr_table() {
        assert_eq!(
            messages::old_tailscaled("1.90.4", "master"),
            "tailscaled 1.90 on `master` cannot send Serve traffic to a socket. Temps needs \
             1.94 or later."
        );
        assert_eq!(
            messages::no_consent("master"),
            "Run `temps agent ingress private --listen tailscale --tailscale-serve` on `master`."
        );
        assert_eq!(
            messages::not_approved("svc:myapp", "master", "tag:temps"),
            "`svc:myapp` is advertised by `master` but not approved. Approve it at \
             login.tailscale.com/admin/services, or add `\"autoApprovers\": {\"services\": \
             {\"svc:myapp\": [\"tag:temps\"]}}` to your tailnet policy."
        );
        assert_eq!(
            messages::listener_exclusive("master", "100.101.102.103"),
            "`master` already serves private addresses on 100.101.102.103:443; give this app a \
             Service name instead"
        );
    }

    #[test]
    fn service_labels_are_one_dns_label() {
        assert!(valid_service_label("myapp-staging"));
        assert!(valid_service_label("svc:myapp"));
        for bad in ["", "MyApp", "-a", "a-", "a.b", "svc:", &"a".repeat(64)] {
            assert!(!valid_service_label(bad), "{bad}");
        }
    }

    #[test]
    fn a_report_is_ready_only_with_everything_on() {
        let ready = TailscaleReport {
            consent: true,
            root: true,
            state: TailscaleState::Ready,
            backend_state: Some("Running".into()),
            dns_name: Some("master.example.ts.net".into()),
            magic_dns: true,
            cert_domains: vec!["master.example.ts.net".into()],
            ..TailscaleReport::default()
        };
        assert!(ready.serve_ready());
        for broken in [
            TailscaleReport {
                consent: false,
                ..ready.clone()
            },
            TailscaleReport {
                root: false,
                ..ready.clone()
            },
            TailscaleReport {
                backend_state: Some("NeedsLogin".into()),
                ..ready.clone()
            },
            TailscaleReport {
                cert_domains: Vec::new(),
                ..ready.clone()
            },
            TailscaleReport {
                magic_dns: false,
                ..ready.clone()
            },
        ] {
            assert!(!broken.serve_ready());
        }
    }

    #[test]
    fn identity_headers_pass_only_on_a_serve_socket() {
        for name in [
            "tailscale-user-login",
            "tailscale-user-name",
            "tailscale-user-profile-pic",
        ] {
            assert!(forwards_header(name, true), "{name}");
            assert!(!forwards_header(name, false), "{name}");
        }
        // Funnel, anything else tailscaled may add, any `Tailscale-User-*`
        // it does not replace (a client could send it through tailscaled),
        // and Temps' own login header never reach the app from a request.
        for name in [
            "tailscale-user-role",
            "tailscale-user-groups",
            "tailscale-user-",
            "tailscale-funnel-request",
            "tailscale-app-capabilities",
            "tailscale-headers-info",
            TAILNET_LOGIN_HEADER,
        ] {
            assert!(!forwards_header(name, true), "{name}");
            assert!(!forwards_header(name, false), "{name}");
        }
        assert!(forwards_header("accept", false));
        assert!(forwards_header("x-tailscale-thing", false));
    }
}
