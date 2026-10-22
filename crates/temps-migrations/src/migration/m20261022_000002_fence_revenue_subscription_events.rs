// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Order revenue subscription state by event time.
//!
//! `revenue_subscriptions_state` was projected from every webhook event in
//! arrival order, so a provider's out-of-order delivery (or a replay by a
//! signing-secret holder) of an older "active" event revived a subscription
//! a newer event had canceled. `last_event_at` records the `occurred_at` of
//! the newest event applied to the row, and ingestion refuses to project an
//! older one.
//!
//! Existing rows are backfilled from the event log, the newest
//! `occurred_at` per subscription, so the fence is correct from the first
//! event after the upgrade. A row with no logged event (CSV import only)
//! stays NULL, which means "no fence yet": the next event is applied and
//! sets it, exactly as before this migration. The backfill is one grouped
//! pass over `revenue_events`, not a per-row subquery.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                ALTER TABLE revenue_subscriptions_state
                    ADD COLUMN IF NOT EXISTS last_event_at TIMESTAMPTZ NULL;

                UPDATE revenue_subscriptions_state AS s
                SET last_event_at = e.newest
                FROM (
                    SELECT integration_id, subscription_ref, MAX(occurred_at) AS newest
                    FROM revenue_events
                    WHERE subscription_ref IS NOT NULL
                    GROUP BY integration_id, subscription_ref
                ) AS e
                WHERE e.integration_id = s.integration_id
                  AND e.subscription_ref = s.provider_subscription_id
                  AND s.last_event_at IS NULL;
                "#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE revenue_subscriptions_state DROP COLUMN IF EXISTS last_event_at",
            )
            .await?;
        Ok(())
    }
}
