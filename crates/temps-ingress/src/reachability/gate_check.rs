// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The reachability check of a signed-in address (ADR 065 D10).
//!
//! Reaching the address is not enough: what answers must be the gate. The
//! control plane asks for the address the way a browser would, without a
//! cookie, and expects the gate's own answer (`302` with
//! `x-temps-gate: sign-in` for the Temps gate; for Cloudflare Access, a
//! redirect to the team's `*.cloudflareaccess.com` sign-in). An answer from
//! the app instead means anyone can open it, and is shown as a failure.
//!
//! The probe dials only addresses the shared outbound policy accepts, pinned
//! so a rebinding answer cannot slip in, follows no redirect, uses no proxy
//! and reads nothing but the status and two headers.

use std::time::Duration;

use temps_core::gate::{GateKind, GATE_ACCESS, GATE_HEADER, GATE_SIGN_IN, GATE_UNAVAILABLE};
use temps_core::url_validation::{resolve_and_validate_domain, UrlValidationError};

use super::model::{AddressReachability, ReachState};

/// How long the probe waits for the gate's answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What answered at a signed-in address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateProbe {
    /// The gate, asking the visitor to sign in.
    InFront,
    /// The gate, unable to sign anyone in (a worker cut off from the
    /// control plane, D8); its message.
    Unavailable(String),
    /// Something other than the gate: the app is open to anyone.
    Open { status: u16 },
    /// Temps could not ask from here; why.
    Unchecked(String),
}

/// Whether `location` sends the browser to a Cloudflare Access sign-in:
/// an `https` URL on a `<team>.cloudflareaccess.com` host.
fn is_access_sign_in(location: Option<&str>) -> bool {
    let Some(rest) = location.and_then(|l| l.trim().strip_prefix("https://")) else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    temps_routes::access_keys::team_domain(host).is_some()
}

/// Classify an answer for `gate`. `location` is the redirect's target.
pub fn classify(
    gate: GateKind,
    status: u16,
    gate_header: Option<&str>,
    location: Option<&str>,
) -> GateProbe {
    match (gate, gate_header) {
        (GateKind::Temps, Some(value)) if value.eq_ignore_ascii_case(GATE_SIGN_IN) => {
            if status == 302 {
                GateProbe::InFront
            } else {
                // A `401` means the probe was not taken for a navigation;
                // still the gate, but not the answer a browser gets.
                GateProbe::Unchecked(format!(
                    "the sign-in answered HTTP {status} instead of sending a browser to it"
                ))
            }
        }
        (GateKind::Temps, Some(value)) if value.eq_ignore_ascii_case(GATE_UNAVAILABLE) => {
            GateProbe::Unavailable(
                "the sign-in is in front of it, but nobody can sign in right now: the server \
                 that serves it cannot reach the control plane"
                    .to_string(),
            )
        }
        (GateKind::Temps, _) => GateProbe::Open { status },
        (GateKind::CloudflareAccess, _)
            if matches!(status, 302 | 303) && is_access_sign_in(location) =>
        {
            GateProbe::InFront
        }
        // Temps refused it itself: safe, but Access did not stop the visitor,
        // so nobody can sign in through it.
        (GateKind::CloudflareAccess, Some(value)) if value.eq_ignore_ascii_case(GATE_ACCESS) => {
            GateProbe::Unavailable(
                "Cloudflare Access is not in front of it: requests reach Temps without an \
                 Access sign-in, and Temps refuses them. Check the Access app for this \
                 hostname in Cloudflare Zero Trust"
                    .to_string(),
            )
        }
        (GateKind::CloudflareAccess, _) => GateProbe::Open { status },
        (GateKind::Unsupported, _) => {
            GateProbe::Unchecked("Temps cannot check this gate from here".to_string())
        }
    }
}

/// Ask `host` over HTTPS as a browser would, without a cookie.
pub async fn probe(host: &str, gate: GateKind) -> GateProbe {
    let addresses = match resolve_and_validate_domain(host, 443).await {
        Ok(addresses) => addresses,
        Err(UrlValidationError::DnsResolutionFailed(reason)) => {
            return GateProbe::Unchecked(format!("{host} did not resolve: {reason}"))
        }
        Err(error) => {
            return GateProbe::Unchecked(format!(
                "{host} resolves to an address Temps does not probe ({error})"
            ))
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .user_agent("Temps-Reachability/1.0")
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .build()
    {
        Ok(client) => client,
        Err(error) => return GateProbe::Unchecked(format!("no HTTP client: {error}")),
    };
    let response = client
        .get(format!("https://{host}/"))
        .header("accept", "text/html")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-dest", "document")
        .send()
        .await;
    match response {
        Ok(response) => classify(
            gate,
            response.status().as_u16(),
            response
                .headers()
                .get(GATE_HEADER)
                .and_then(|v| v.to_str().ok()),
            response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
        ),
        Err(error) => GateProbe::Unchecked(format!("https://{host}/ did not answer: {error}")),
    }
}

/// The verdict on a signed-in address that the public checks judged
/// reachable, once the probe has answered.
pub fn judged(mut verdict: AddressReachability, probe: GateProbe) -> AddressReachability {
    let host = verdict.address.clone();
    match probe {
        GateProbe::InFront => {
            verdict.reason = format!("{} The sign-in is in front of it.", verdict.reason)
                .trim()
                .to_string();
        }
        GateProbe::Unavailable(why) => {
            verdict.state = ReachState::NotReachable;
            verdict.reason = format!("{host} answers, but {why}.");
        }
        GateProbe::Open { status } => {
            verdict.state = ReachState::NotReachable;
            verdict.reason = format!(
                "Anyone can open {host}: the sign-in is not in front of it (the app answered \
                 HTTP {status} to a visitor who had not signed in)."
            );
        }
        GateProbe::Unchecked(why) => {
            verdict.state = ReachState::Unknown;
            verdict.reason =
                format!("Temps could not check that the sign-in is in front of {host}: {why}.");
        }
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reachability::model::{AddressSource, Audience};

    fn reachable() -> AddressReachability {
        AddressReachability {
            address: "team.example.test".into(),
            audience: Audience::SignedIn,
            source: AddressSource::CustomDomain,
            custom_domain_id: Some(1),
            way_in: Some("direct:edge".into()),
            state: ReachState::Reachable,
            reason: "It resolves to edge.".into(),
            action: None,
            network: None,
        }
    }

    #[test]
    fn only_the_gates_own_redirect_passes() {
        assert_eq!(
            classify(GateKind::Temps, 302, Some("sign-in"), None),
            GateProbe::InFront
        );
        // The app's own login redirect is not the gate.
        assert_eq!(
            classify(GateKind::Temps, 302, None, None),
            GateProbe::Open { status: 302 }
        );
        assert_eq!(
            classify(GateKind::Temps, 200, None, None),
            GateProbe::Open { status: 200 }
        );
        // The password wall or a 403 from the app is not the gate either.
        assert_eq!(
            classify(GateKind::Temps, 403, Some("access"), None),
            GateProbe::Open { status: 403 }
        );
        assert!(matches!(
            classify(GateKind::Temps, 503, Some("unavailable"), None),
            GateProbe::Unavailable(_)
        ));
        assert!(matches!(
            classify(GateKind::Temps, 401, Some("sign-in"), None),
            GateProbe::Unchecked(_)
        ));
    }

    #[test]
    fn only_a_redirect_to_the_access_sign_in_passes_for_access() {
        let login =
            Some("https://team.cloudflareaccess.com/cdn-cgi/access/login/app.example.test?kid=x");
        assert_eq!(
            classify(GateKind::CloudflareAccess, 302, None, login),
            GateProbe::InFront
        );
        // A redirect anywhere else is the app's own.
        assert_eq!(
            classify(
                GateKind::CloudflareAccess,
                302,
                None,
                Some("https://team.cloudflareaccess.com.evil.example/login")
            ),
            GateProbe::Open { status: 302 }
        );
        assert_eq!(
            classify(
                GateKind::CloudflareAccess,
                302,
                None,
                Some("http://team.cloudflareaccess.com/")
            ),
            GateProbe::Open { status: 302 }
        );
        assert_eq!(
            classify(GateKind::CloudflareAccess, 200, None, None),
            GateProbe::Open { status: 200 }
        );
        // Temps refused it: Access is not in front of it.
        assert!(matches!(
            classify(GateKind::CloudflareAccess, 403, Some("access"), None),
            GateProbe::Unavailable(_)
        ));
        // The Temps gate's header never passes for Access.
        assert_eq!(
            classify(GateKind::CloudflareAccess, 302, Some("sign-in"), None),
            GateProbe::Open { status: 302 }
        );
    }

    #[test]
    fn an_open_app_fails_in_red_rather_than_passing() {
        let verdict = judged(reachable(), GateProbe::Open { status: 200 });
        assert_eq!(verdict.state, ReachState::NotReachable);
        assert!(
            verdict.reason.starts_with(
                "Anyone can open team.example.test: the sign-in is not in front of it"
            ),
            "{}",
            verdict.reason
        );
    }

    #[test]
    fn the_gate_in_front_keeps_it_reachable() {
        let verdict = judged(reachable(), GateProbe::InFront);
        assert_eq!(verdict.state, ReachState::Reachable);
        assert!(verdict.reason.ends_with("The sign-in is in front of it."));
    }

    #[test]
    fn an_unchecked_address_is_unknown_not_reachable() {
        let verdict = judged(reachable(), GateProbe::Unchecked("timeout".into()));
        assert_eq!(verdict.state, ReachState::Unknown);
        let verdict = judged(reachable(), GateProbe::Unavailable("cut off".into()));
        assert_eq!(verdict.state, ReachState::NotReachable);
    }

    #[tokio::test]
    async fn the_probe_never_dials_a_loopback_address() {
        // `localhost` resolves to loopback, which the outbound policy
        // refuses: nothing is dialled.
        assert!(matches!(
            probe("localhost", GateKind::Temps).await,
            GateProbe::Unchecked(_)
        ));
    }
}
