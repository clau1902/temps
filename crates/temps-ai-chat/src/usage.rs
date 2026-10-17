// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a chat has cost so far, read from the AI gateway's usage log.
//!
//! Every model request a chat makes through the gateway is logged once, with
//! its token counts and an estimated cost (`ai_usage_logs`), tagged with the
//! conversation's numeric id and a `chat.*` purpose. This module only reads
//! that log: it aggregates per model, bounded by the number of distinct models
//! one chat has used, and never returns request content.
//!
//! The figures are estimates. A request whose model has no published price is
//! logged at zero cost, and is counted in `unpriced_requests` so the total is
//! never presented as complete when it is not.

use sea_orm::{DatabaseBackend, DatabaseConnection, DbErr, FromQueryResult, Statement};
use serde::Serialize;
use utoipa::ToSchema;

/// Most distinct models reported for one chat. A chat switches models rarely;
/// the cap only bounds a pathological history.
const MAX_MODELS: i64 = 20;

/// Spend for one model within a chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ModelSpend {
    pub model: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Estimated cost in microcents (1/10000 of a cent), as logged.
    pub cost_microcents: i64,
}

/// What one conversation has cost so far.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ConversationUsage {
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Estimated cost in microcents (1/10000 of a cent), summed over the
    /// requests that have a known price.
    pub cost_microcents: i64,
    /// Requests that used tokens but were logged at zero cost because the
    /// model has no known price. When this is not zero, the cost is a floor.
    pub unpriced_requests: i64,
    /// Requests made with the user's own provider key rather than the
    /// platform's, which Temps does not bill.
    pub byok_requests: i64,
    pub models: Vec<ModelSpend>,
}

#[derive(Debug, FromQueryResult)]
struct ModelRow {
    model: String,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost_microcents: i64,
    unpriced_requests: i64,
    byok_requests: i64,
}

/// Aggregate the usage of conversation `conversation_id` for `user_id`.
///
/// Rows are matched on the owner as well as the conversation, and only those
/// logged by a chat turn (a `chat.*` purpose tag), so neither another user's
/// requests nor the owner's own gateway calls that happen to reuse the same
/// conversation string are counted.
pub async fn conversation_usage(
    db: &DatabaseConnection,
    conversation_id: i64,
    user_id: i32,
) -> Result<ConversationUsage, DbErr> {
    let rows = ModelRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        r#"SELECT
               model,
               COUNT(*)::INT8 AS requests,
               COALESCE(SUM(input_tokens), 0)::INT8 AS input_tokens,
               COALESCE(SUM(output_tokens), 0)::INT8 AS output_tokens,
               COALESCE(SUM(estimated_cost_microcents), 0)::INT8 AS cost_microcents,
               COUNT(*) FILTER (
                   WHERE estimated_cost_microcents = 0
                     AND input_tokens + output_tokens > 0
                     AND NOT is_byok
               )::INT8 AS unpriced_requests,
               COUNT(*) FILTER (WHERE is_byok)::INT8 AS byok_requests
           FROM ai_usage_logs
           WHERE conversation_id = $1
             AND user_id = $2
             AND EXISTS (SELECT 1 FROM unnest(tags) AS tag WHERE tag LIKE 'chat.%')
           GROUP BY model
           ORDER BY cost_microcents DESC, model ASC
           LIMIT $3"#,
        [
            conversation_id.to_string().into(),
            user_id.into(),
            MAX_MODELS.into(),
        ],
    ))
    .all(db)
    .await?;

    Ok(summarize(rows))
}

fn summarize(rows: Vec<ModelRow>) -> ConversationUsage {
    let mut usage = ConversationUsage {
        requests: 0,
        input_tokens: 0,
        output_tokens: 0,
        cost_microcents: 0,
        unpriced_requests: 0,
        byok_requests: 0,
        models: Vec::with_capacity(rows.len()),
    };
    for row in rows {
        usage.requests = usage.requests.saturating_add(row.requests);
        usage.input_tokens = usage.input_tokens.saturating_add(row.input_tokens);
        usage.output_tokens = usage.output_tokens.saturating_add(row.output_tokens);
        usage.cost_microcents = usage.cost_microcents.saturating_add(row.cost_microcents);
        usage.unpriced_requests = usage
            .unpriced_requests
            .saturating_add(row.unpriced_requests);
        usage.byok_requests = usage.byok_requests.saturating_add(row.byok_requests);
        usage.models.push(ModelSpend {
            model: row.model,
            requests: row.requests,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cost_microcents: row.cost_microcents,
        });
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{DatabaseBackend, MockDatabase};
    use std::collections::BTreeMap;

    fn row(
        model: &str,
        requests: i64,
        cost: i64,
        unpriced: i64,
        byok: i64,
    ) -> BTreeMap<String, sea_orm::Value> {
        BTreeMap::from([
            ("model".to_string(), model.into()),
            ("requests".to_string(), requests.into()),
            ("input_tokens".to_string(), (requests * 100).into()),
            ("output_tokens".to_string(), (requests * 10).into()),
            ("cost_microcents".to_string(), cost.into()),
            ("unpriced_requests".to_string(), unpriced.into()),
            ("byok_requests".to_string(), byok.into()),
        ])
    }

    #[tokio::test]
    async fn totals_add_up_across_models_and_keep_the_per_model_split() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![
                row("vendor/large", 3, 9_000, 0, 0),
                row("vendor/small", 5, 500, 1, 2),
            ]])
            .into_connection();

        let usage = conversation_usage(&db, 42, 7).await.expect("usage");

        assert_eq!(usage.requests, 8);
        assert_eq!(usage.input_tokens, 800);
        assert_eq!(usage.output_tokens, 80);
        assert_eq!(usage.cost_microcents, 9_500);
        assert_eq!(usage.unpriced_requests, 1);
        assert_eq!(usage.byok_requests, 2);
        assert_eq!(usage.models.len(), 2);
        assert_eq!(usage.models[0].model, "vendor/large");
        assert_eq!(usage.models[0].cost_microcents, 9_000);
    }

    #[tokio::test]
    async fn a_chat_with_no_logged_requests_costs_nothing() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([Vec::<BTreeMap<String, sea_orm::Value>>::new()])
            .into_connection();

        let usage = conversation_usage(&db, 42, 7).await.expect("usage");

        assert_eq!(usage.requests, 0);
        assert_eq!(usage.cost_microcents, 0);
        assert!(usage.models.is_empty());
    }

    #[tokio::test]
    async fn the_query_is_scoped_to_the_conversation_and_its_owner() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([Vec::<BTreeMap<String, sea_orm::Value>>::new()])
            .into_connection();

        conversation_usage(&db, 42, 7).await.expect("usage");

        let log = db.into_transaction_log();
        let sql = format!("{:?}", log);
        assert!(sql.contains("conversation_id = $1"));
        assert!(sql.contains("user_id = $2"));
        assert!(sql.contains("chat.%"));
        assert!(
            sql.contains("\"42\""),
            "the conversation id is matched as its decimal string"
        );
        assert!(
            sql.contains("Int(Some(7))"),
            "the owner id is bound, not interpolated"
        );
    }

    #[tokio::test]
    async fn a_database_failure_is_returned_not_swallowed() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("connection reset".to_string())])
            .into_connection();

        let error = conversation_usage(&db, 42, 7).await.expect_err("must fail");
        assert!(error.to_string().contains("connection reset"));
    }
}
