// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::TransactionTrait;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let tx = manager.get_connection().begin().await?;
        // Wait for in-flight group writes before taking the count snapshot.
        // Subsequent transactional ingests increment from the reconciled value.
        tx.execute_unprepared("LOCK TABLE error_groups IN SHARE ROW EXCLUSIVE MODE")
            .await?;
        tx.execute_unprepared("UPDATE error_groups g SET total_count = counts.n FROM (SELECT g.id, count(e.id)::integer AS n FROM error_groups g LEFT JOIN error_events e ON e.error_group_id=g.id GROUP BY g.id) counts WHERE g.id=counts.id AND g.total_count IS DISTINCT FROM counts.n").await?;
        tx.execute_unprepared("CREATE TABLE IF NOT EXISTS error_group_fingerprints (project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE, fingerprint_hash VARCHAR(64) NOT NULL, error_group_id INTEGER NOT NULL REFERENCES error_groups(id) ON DELETE CASCADE, PRIMARY KEY(project_id,fingerprint_hash)); CREATE INDEX IF NOT EXISTS error_group_fingerprints_group ON error_group_fingerprints(error_group_id); INSERT INTO error_group_fingerprints(project_id,fingerprint_hash,error_group_id) SELECT project_id,fingerprint_hash,min(error_group_id) FROM error_events GROUP BY project_id,fingerprint_hash ON CONFLICT DO NOTHING").await?;
        tx.commit().await?;
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE error_group_fingerprints")
            .await?;
        // Accurate counts remain accurate on rollback; no records are merged.
        Ok(())
    }
}
