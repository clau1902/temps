// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Custom hostnames for public status pages (#1412).
//!
//! - `status_page_domains`: one row per hostname attached to a status page.
//!   `hostname` is stored normalized (lowercase ASCII/punycode, no trailing
//!   dot) and is unique across all pages.
//! - `domain_id` points at the `domains` row that carries the ACME order and,
//!   once ownership is proven, the certificate. Temps creates that row for the
//!   binding and owns it exclusively, so a trigger deletes it whenever the
//!   binding goes away or is re-pointed — on every path, including cascades
//!   from page or project deletion. That stops both serving and renewal of
//!   the certificate.
//! - Removing a hostname, or deleting its page or project, leaves a
//!   *tombstone*: `status_page_id` becomes NULL and `detached_at` is set. A
//!   tombstone keeps answering 404 for the hostname (instead of letting it fall
//!   through to the console) until the name is attached again or claimed by an
//!   application, which always takes precedence over a tombstone.
//! - Route-table NOTIFY triggers make every proxy (in-process or a separate
//!   `temps proxy`) reload when a binding, a page, or the certificate state of
//!   a bound `domains` row changes.

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
CREATE TABLE IF NOT EXISTS status_page_domains (
    id SERIAL PRIMARY KEY,
    status_page_id INTEGER REFERENCES status_pages(id) ON DELETE SET NULL,
    hostname VARCHAR(253) NOT NULL,
    verification_method VARCHAR(16) NOT NULL DEFAULT 'http-01',
    domain_id INTEGER REFERENCES domains(id) ON DELETE SET NULL,
    detached_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT status_page_domains_method_check
        CHECK (verification_method IN ('http-01', 'dns-01')),
    CONSTRAINT status_page_domains_hostname_normalized
        CHECK (hostname = lower(hostname) AND hostname NOT LIKE '%.' AND hostname NOT LIKE '%*%'),
    CONSTRAINT status_page_domains_tombstone_shape
        CHECK ((status_page_id IS NULL) = (detached_at IS NOT NULL)
               AND (status_page_id IS NOT NULL OR domain_id IS NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_status_page_domains_hostname
    ON status_page_domains (hostname);

CREATE INDEX IF NOT EXISTS idx_status_page_domains_page
    ON status_page_domains (status_page_id, id) WHERE status_page_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS uq_status_page_domains_domain
    ON status_page_domains (domain_id) WHERE domain_id IS NOT NULL;

-- Detaching a binding (directly, or through ON DELETE SET NULL when its page
-- is deleted) turns it into a tombstone and releases its certificate.
CREATE OR REPLACE FUNCTION status_page_domain_tombstone()
RETURNS trigger AS $$
BEGIN
    IF NEW.status_page_id IS NULL THEN
        NEW.detached_at := COALESCE(NEW.detached_at, now());
        NEW.domain_id := NULL;
    ELSE
        NEW.detached_at := NULL;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS status_page_domains_tombstone ON status_page_domains;
CREATE TRIGGER status_page_domains_tombstone
    BEFORE INSERT OR UPDATE ON status_page_domains
    FOR EACH ROW EXECUTE FUNCTION status_page_domain_tombstone();

CREATE OR REPLACE FUNCTION status_page_domain_release_certificate()
RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        IF OLD.domain_id IS NOT NULL THEN
            DELETE FROM domains WHERE id = OLD.domain_id;
        END IF;
        RETURN OLD;
    END IF;
    IF OLD.domain_id IS NOT NULL AND OLD.domain_id IS DISTINCT FROM NEW.domain_id THEN
        DELETE FROM domains WHERE id = OLD.domain_id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS status_page_domains_release_certificate ON status_page_domains;
CREATE TRIGGER status_page_domains_release_certificate
    AFTER UPDATE OR DELETE ON status_page_domains
    FOR EACH ROW EXECUTE FUNCTION status_page_domain_release_certificate();

DROP TRIGGER IF EXISTS status_page_domains_route_table_changes ON status_page_domains;
CREATE TRIGGER status_page_domains_route_table_changes
    AFTER INSERT OR UPDATE OR DELETE ON status_page_domains
    FOR EACH STATEMENT EXECUTE FUNCTION notify_route_table_change();

DROP TRIGGER IF EXISTS status_pages_route_table_changes ON status_pages;
CREATE TRIGGER status_pages_route_table_changes
    AFTER INSERT OR UPDATE OR DELETE ON status_pages
    FOR EACH STATEMENT EXECUTE FUNCTION notify_route_table_change();

CREATE OR REPLACE FUNCTION notify_status_page_certificate_change()
RETURNS trigger AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM status_page_domains WHERE domain_id = NEW.id) THEN
        PERFORM pg_notify('route_table_changes', '');
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS domains_status_page_route_changes ON domains;
CREATE TRIGGER domains_status_page_route_changes
    AFTER UPDATE OF status, certificate ON domains
    FOR EACH ROW EXECUTE FUNCTION notify_status_page_certificate_change();
"#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
DROP TRIGGER IF EXISTS domains_status_page_route_changes ON domains;
DROP FUNCTION IF EXISTS notify_status_page_certificate_change();
DROP TRIGGER IF EXISTS status_pages_route_table_changes ON status_pages;
DROP TABLE IF EXISTS status_page_domains;
DROP FUNCTION IF EXISTS status_page_domain_release_certificate();
DROP FUNCTION IF EXISTS status_page_domain_tombstone();
"#,
            )
            .await?;
        Ok(())
    }
}
