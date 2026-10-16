// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Marker for requests originating from the AI agent's `call_api` tool.
//!
//! The agent does not call the HTTP API over the network — `InternalApiCaller`
//! replays a synthetic request through the real Axum router with the caller's
//! `AuthContext` injected, so `permission_guard!` and project scoping stay the
//! single enforcement point. That is exactly what we want for authorization,
//! but it also makes agent traffic indistinguishable from console traffic.
//!
//! Some endpoints need to treat the two differently. Reading table rows is the
//! motivating case: a human who already has `ExternalServicesRead` may browse
//! their own data freely, but the *agent* doing the same thing ships those rows
//! to a third-party LLM provider, which the operator must opt into per service.
//!
//! The caller inserts [`AiToolCall`] into the request extensions; handlers that
//! care extract it with `Option<Extension<AiToolCall>>`. Absence means a normal
//! console/CLI request — the marker can only be set in-process by the tool
//! caller, never by a remote client, because extensions are not derived from
//! anything on the wire.

/// Present in request extensions when the request was issued by the AI agent's
/// API tool rather than by a human client.
///
/// ```ignore
/// async fn handler(
///     ai_call: Option<Extension<AiToolCall>>,
///     // …
/// ) -> Result<impl IntoResponse, Problem> {
///     if ai_call.is_some() {
///         // apply the agent-specific gate
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AiToolCall;

/// Where an agent-issued request came from: the human client behind it.
///
/// The replayed request has no socket and no `User-Agent` of its own, so
/// without this every audit row written by an assistant action would record
/// `unknown` for both. The caller that holds the real approver's metadata
/// inserts it next to [`AiToolCall`]; [`crate::request_metadata`] folds it into
/// the [`crate::RequestMetadata`] the handlers hand to the audit log, together
/// with a visible "via Temps AI assistant" label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiToolOrigin {
    pub ip_address: String,
    pub user_agent: String,
}

/// Suffix appended to the recorded `user_agent` of every request issued by the
/// AI assistant, so an audit entry reads as "done from the assistant".
pub const AI_ASSISTANT_USER_AGENT_LABEL: &str = "via Temps AI assistant";

/// The `user_agent` an audit entry records for an assistant-issued request.
pub fn assistant_user_agent(origin: Option<&AiToolOrigin>) -> String {
    match origin {
        Some(origin) => format!("{} ({})", origin.user_agent, AI_ASSISTANT_USER_AGENT_LABEL),
        None => format!("Temps AI assistant ({AI_ASSISTANT_USER_AGENT_LABEL})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_tool_call_round_trips_through_http_extensions() {
        // The gate is only sound if the marker survives the extension
        // insert/extract cycle the caller and handler rely on.
        let mut extensions = axum::http::Extensions::new();
        assert!(extensions.get::<AiToolCall>().is_none());

        extensions.insert(AiToolCall);
        assert_eq!(extensions.get::<AiToolCall>(), Some(&AiToolCall));
    }

    #[test]
    fn assistant_user_agent_keeps_the_real_client_and_names_the_assistant() {
        let origin = AiToolOrigin {
            ip_address: "203.0.113.7".to_string(),
            user_agent: "Mozilla/5.0".to_string(),
        };
        assert_eq!(
            assistant_user_agent(Some(&origin)),
            "Mozilla/5.0 (via Temps AI assistant)"
        );
        assert!(assistant_user_agent(None).contains("via Temps AI assistant"));
    }
}
