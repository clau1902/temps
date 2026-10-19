// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sentry Error Provider Implementation

use async_trait::async_trait;
use relay_protocol::{FromValue, IntoValue};
use serde_json::Value;
use std::sync::Arc;

use super::{AuthContext, ErrorProvider, ParsedErrorEvent, ProviderError};
use crate::sentry::{dsn_service::DSNService, envelope::Envelope, envelope::EnvelopeItem, mapper};

/// Sentry error provider implementation
pub struct SentryProvider {
    dsn_service: Arc<DSNService>,
}

impl SentryProvider {
    pub fn new(dsn_service: Arc<DSNService>) -> Self {
        Self { dsn_service }
    }
}

/// Fully validated dispatch result. No writes happen until every supported item
/// has been decoded. Unsupported names are collapsed to the fixed ItemType set.
pub struct ParsedSentryEnvelope {
    pub events: Vec<ParsedErrorEvent>,
    pub logs: Vec<temps_otel::types::LogRecord>,
    pub spans: Vec<temps_otel::types::SpanRecord>,
    pub metrics: Vec<temps_otel::types::MetricPoint>,
    pub replay: Option<crate::sentry::replay::ReplaySegment>,
    pub check_ins: Vec<temps_monitoring::cron_monitors::MonitorCheckIn>,
    pub unsupported: std::collections::BTreeMap<&'static str, usize>,
}

impl SentryProvider {
    pub fn parse_envelope(
        &self,
        payload: &[u8],
        auth: &AuthContext,
    ) -> Result<ParsedSentryEnvelope, ProviderError> {
        // Parse Sentry envelope
        let envelope = Envelope::parse_for_ingestion(payload)
            .map_err(|e| ProviderError::Parsing(e.to_string()))?;

        let mut parsed_events = Vec::new();
        let mut logs = Vec::new();
        let mut check_ins = Vec::new();
        let mut unsupported = std::collections::BTreeMap::new();
        let mut saw_logs = false;
        let mut saw_metrics = false;
        let mut spans = Vec::new();
        let mut metrics = Vec::new();
        let mut replay_event = None;
        let mut replay_recording = None;

        // Process each item in the envelope
        for (header, item) in envelope.items_with_headers() {
            match item {
                EnvelopeItem::Event(event) => {
                    // Extract event ID first
                    let event_id = event
                        .value()
                        .and_then(|e| e.id.value().map(|id| id.to_string()))
                        .ok_or_else(|| ProviderError::Validation("Event missing ID".to_string()))?;

                    // Serialize the complete Sentry event to JSON (preserving all data)
                    // Convert Annotated<Event> -> Event -> relay Value -> serde_json::Value
                    let raw_event = if let Some(ev) = event.value() {
                        // Convert Event to relay Value
                        let relay_val: relay_protocol::Value = ev.clone().into_value();
                        mapper::relay_value_to_json(&relay_val).unwrap_or_else(
                            || serde_json::json!({"error": "Failed to serialize event"}),
                        )
                    } else {
                        serde_json::json!({"error": "Event has no value"})
                    };

                    // Convert to internal format
                    let error_data = mapper::convert_sentry_event_to_error_data(
                        event,
                        raw_event.clone(),
                        auth.project_id,
                        auth.environment_id,
                        auth.deployment_id,
                    )
                    .map_err(|e| ProviderError::Parsing(e.to_string()))?;

                    parsed_events.push(ParsedErrorEvent {
                        event_id,
                        raw_event,
                        error_data,
                    });
                }
                EnvelopeItem::Log(payload) => {
                    if saw_logs {
                        return Err(ProviderError::Validation(
                            "Only one log item is allowed per envelope".into(),
                        ));
                    }
                    saw_logs = true;
                    logs = crate::sentry::logs::decode_logs(header, payload, auth)
                        .map_err(ProviderError::LogBatch)?;
                }
                EnvelopeItem::Unsupported(payload)
                    if matches!(
                        header.ty,
                        crate::sentry::envelope::ItemType::Transaction
                            | crate::sentry::envelope::ItemType::Span
                    ) =>
                {
                    let streamed = header.ty == crate::sentry::envelope::ItemType::Span
                        && header.content_type.as_deref()
                            == Some("application/vnd.sentry.items.span.v2+json");
                    if !streamed
                        && (header
                            .content_type
                            .as_deref()
                            .is_some_and(|v| v != "application/json")
                            || header.item_count.is_some_and(|n| n != 1))
                    {
                        return Err(ProviderError::Validation(
                            "Unsupported trace content type or count".into(),
                        ));
                    }
                    let records = if streamed {
                        crate::sentry::traces::streamed(header, payload, auth)
                    } else if header.ty == crate::sentry::envelope::ItemType::Transaction {
                        crate::sentry::traces::transaction(payload, auth)
                    } else {
                        crate::sentry::traces::standalone(payload, auth).map(|s| vec![s])
                    }
                    .map_err(|_| ProviderError::Validation("Invalid Sentry trace".into()))?;
                    if spans.len() + records.len() > 1000 {
                        return Err(ProviderError::Validation(
                            "At most 1000 spans per envelope".into(),
                        ));
                    }
                    spans.extend(records);
                }
                EnvelopeItem::Unsupported(payload)
                    if header.ty == crate::sentry::envelope::ItemType::TraceMetric =>
                {
                    if saw_metrics {
                        return Err(ProviderError::Validation(
                            "Only one trace_metric batch per envelope".into(),
                        ));
                    }
                    saw_metrics = true;
                    metrics = crate::sentry::metrics::decode(header, payload, auth)
                        .map_err(|_| ProviderError::Validation("Invalid Sentry metrics".into()))?;
                }
                EnvelopeItem::Unsupported(payload)
                    if header.ty == crate::sentry::envelope::ItemType::ReplayEvent =>
                {
                    if replay_event.replace(*payload).is_some() {
                        return Err(ProviderError::Validation(
                            "Only one replay segment per envelope".into(),
                        ));
                    }
                }
                EnvelopeItem::Unsupported(payload)
                    if header.ty == crate::sentry::envelope::ItemType::ReplayRecording =>
                {
                    if replay_recording.replace(*payload).is_some() {
                        return Err(ProviderError::Validation(
                            "Only one replay recording per envelope".into(),
                        ));
                    }
                }
                EnvelopeItem::Unsupported(payload)
                    if matches!(header.ty, crate::sentry::envelope::ItemType::CheckIn) =>
                {
                    if header
                        .content_type
                        .as_deref()
                        .is_some_and(|v| v != "application/json")
                        || header.item_count.is_some_and(|count| count != 1)
                    {
                        return Err(ProviderError::Validation(
                            "Unsupported check-in content type or count".into(),
                        ));
                    }
                    if check_ins.len() >= 100 {
                        return Err(ProviderError::Validation(
                            "At most 100 check-ins per envelope".into(),
                        ));
                    }
                    check_ins.push(
                        crate::sentry::check_ins::decode(payload)
                            .map_err(|e| ProviderError::Validation(e.to_string()))?,
                    );
                }
                _ => {
                    *unsupported.entry(header.ty.name()).or_insert(0) += 1;
                }
            }
        }

        let replay = match (replay_event, replay_recording) {
            (None, None) => None,
            // Native/video replay is not rrweb. Keep the paired metadata
            // unsupported too, without rejecting other valid envelope items.
            (Some(_), None) if unsupported.contains_key("replay_video") => {
                *unsupported.entry("replay_event").or_insert(0) += 1;
                None
            }
            (Some(event), Some(recording)) => Some(
                crate::sentry::replay::decode(event, recording, auth).map_err(|_| {
                    ProviderError::Validation("Invalid browser replay segment".into())
                })?,
            ),
            _ => {
                return Err(ProviderError::Validation(
                    "Replay metadata and recording must be paired".into(),
                ))
            }
        };
        Ok(ParsedSentryEnvelope {
            spans,
            metrics,
            replay,
            events: parsed_events,
            logs,
            check_ins,
            unsupported,
        })
    }
}

#[async_trait]
impl ErrorProvider for SentryProvider {
    fn name(&self) -> &'static str {
        "sentry"
    }

    async fn authenticate(
        &self,
        project_id: i32,
        credentials: &str,
    ) -> Result<AuthContext, ProviderError> {
        let dsn = self
            .dsn_service
            .validate_dsn(project_id, credentials)
            .await
            .map_err(|e| ProviderError::Authentication(e.to_string()))?;

        Ok(AuthContext {
            project_id: dsn.project_id,
            environment_id: dsn.environment_id,
            deployment_id: dsn.deployment_id,
            rate_limit_per_minute: dsn.rate_limit_per_minute,
        })
    }

    async fn parse_events(
        &self,
        payload: &[u8],
        auth: &AuthContext,
    ) -> Result<Vec<ParsedErrorEvent>, ProviderError> {
        Ok(self.parse_envelope(payload, auth)?.events)
    }

    async fn parse_json_event(
        &self,
        event_json: Value,
        auth: &AuthContext,
    ) -> Result<ParsedErrorEvent, ProviderError> {
        // Parse with Relay types
        let event = relay_event_schema::protocol::Event::from_value(event_json.clone().into());

        // Extract event ID
        let event_id = event
            .value()
            .and_then(|e| e.id.value().map(|id| id.to_string()))
            .ok_or_else(|| ProviderError::Validation("Event missing ID".to_string()))?;

        // Convert to internal format
        let error_data = mapper::convert_sentry_event_to_error_data(
            &event,
            event_json.clone(),
            auth.project_id,
            auth.environment_id,
            auth.deployment_id,
        )
        .map_err(|e| ProviderError::Parsing(e.to_string()))?;

        Ok(ParsedErrorEvent {
            event_id,
            raw_event: event_json,
            error_data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unsupported_mobile_video_does_not_block_native_telemetry() {
        let db = Arc::new(
            sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Postgres).into_connection(),
        );
        let provider = SentryProvider::new(Arc::new(DSNService::new(db)));
        let auth = AuthContext {
            project_id: 41,
            environment_id: None,
            deployment_id: None,
            rate_limit_per_minute: None,
        };
        let mut bytes=b"{}\n{\"type\":\"replay_event\"}\n{\"platform\":\"cocoa\"}\n{\"type\":\"replay_video\",\"length\":3}\n".to_vec();
        bytes.extend_from_slice(b"\0\xff\n\n");
        bytes.extend_from_slice(br#"{"type":"log","item_count":1,"content_type":"application/vnd.sentry.items.log+json"}
{"items":[{"timestamp":1790000000,"level":"info","body":"Synthetic mixed log","trace_id":"11111111111111111111111111111111"}]}
"#);
        let parsed = provider.parse_envelope(&bytes, &auth).unwrap();
        assert_eq!(parsed.logs.len(), 1);
        assert!(parsed.replay.is_none());
        assert_eq!(parsed.unsupported.get("replay_video"), Some(&1));
        assert_eq!(parsed.unsupported.get("replay_event"), Some(&1));
    }
}
