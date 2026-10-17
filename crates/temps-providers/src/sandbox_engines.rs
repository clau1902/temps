// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The seam that lets a sandbox use a linked service without the
//! deployment's credentials (ADR-051).
//!
//! Each service type that can hand out a credential limited to a namespace of
//! its own implements [`SandboxAccessEngine`] and is listed in [`ENGINES`]:
//! a database and role for PostgreSQL, a bucket-scoped key for S3, a key
//! prefix ACL for Redis. Everything the engines share (the link check, the
//! data network, merging and redacting the variables, the audit trail) stays
//! out of them, so supporting another service type is one implementation and
//! one line in the list.
//!
//! A type with no engine is not an error to the sandbox: it is left out,
//! neither reachable nor given the deployment's credentials.

use std::collections::HashMap;

use async_trait::async_trait;
use temps_entities::external_services;

use crate::sandbox_access::PostgresSandboxAccess;
use crate::sandbox_mariadb::MariaDbSandboxAccess;
use crate::sandbox_mongodb::MongoDbSandboxAccess;
use crate::sandbox_redis::RedisSandboxAccess;
use crate::sandbox_s3::S3SandboxAccess;
use crate::services::{ExternalServiceError, ExternalServiceManager};

/// What an engine is given to provision a sandbox's access. The shared checks
/// (the service exists, is linked to the project) have already passed.
pub struct SandboxAccessRequest<'a> {
    pub service: &'a external_services::Model,
    /// The name the sandbox's database or bucket is derived from: the
    /// project's slug, or one made from the sandbox's id when the link was
    /// made to the service directly. A plain lowercase slug.
    pub namespace: &'a str,
    /// The project the link was made through, for the audit trail.
    pub project_id: Option<i32>,
    /// The service's decrypted parameters, including its administrator
    /// credentials. Never copy them into the returned variables.
    pub parameters: &'a HashMap<String, serde_json::Value>,
}

/// A service type that can give a sandbox a credential of its own.
#[async_trait]
pub trait SandboxAccessEngine: Send + Sync {
    /// The `external_services.service_type` values this engine serves.
    fn service_types(&self) -> &'static [&'static str];

    /// The containers a sandbox must dial to use `service`, by name, or none
    /// when this engine does not cover it (a backend it does not speak, a
    /// service on another node). Put on the sandbox's private network and
    /// nothing else of the service: it must agree with what
    /// [`Self::issue`] names as the host.
    async fn containers(
        &self,
        manager: &ExternalServiceManager,
        service: &external_services::Model,
    ) -> Result<Vec<String>, ExternalServiceError>;

    /// Provision the sandbox's namespace on the service and return the
    /// variables to run code with.
    ///
    /// Must be idempotent: it runs on every connect, so a credential or
    /// namespace that was changed or dropped by hand is repaired. The
    /// variables name the container to dial and its port, consistently, and
    /// never contain the service's administrator credentials. A service the
    /// sandbox cannot reach (for example one on another node) is an
    /// [`ExternalServiceError::SandboxAccessUnsupported`], not a fallback to
    /// the shared credentials. A service this engine does not cover at all
    /// answers [`ExternalServiceError::NoSandboxAccessEngine`], which callers
    /// read as "leave it out".
    async fn issue(
        &self,
        manager: &ExternalServiceManager,
        request: SandboxAccessRequest<'_>,
    ) -> Result<HashMap<String, String>, ExternalServiceError>;
}

/// Every service type with sandbox access. Add an engine here to support one.
static ENGINES: &[&dyn SandboxAccessEngine] = &[
    &PostgresSandboxAccess,
    &S3SandboxAccess,
    &RedisSandboxAccess,
    &MariaDbSandboxAccess,
    &MongoDbSandboxAccess,
];

/// A non-empty string (or number) parameter of a service, as text.
pub(crate) fn text_parameter(
    parameters: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    parameters.get(key).and_then(|value| {
        value
            .as_str()
            .map(str::to_string)
            .or_else(|| value.as_u64().map(|number| number.to_string()))
            .filter(|value| !value.is_empty())
    })
}

/// A password for a scoped user that survives restarts without being stored:
/// an HMAC of what it is for, keyed by the service's administrator secret. It
/// changes when that secret does, and differs per service, user and engine
/// (`tag`).
pub(crate) fn derive_password(
    admin_secret: &str,
    tag: &str,
    service_id: i32,
    name: &str,
) -> Result<String, String> {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(admin_secret.as_bytes())
        .map_err(|error| format!("could not derive the sandbox password: {error}"))?;
    mac.update(format!("{tag}:{service_id}:{name}").as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Refuse a service on another node: a sandbox can only reach the control
/// plane's Docker network, and falling back to the shared credentials is never
/// an option.
pub(crate) fn require_local(
    service: &external_services::Model,
) -> Result<(), ExternalServiceError> {
    if let crate::services::ServiceExecutionRoute::Remote(node_id) =
        crate::services::service_execution_route(service.node_id)
    {
        return Err(ExternalServiceError::SandboxAccessUnsupported {
            service_id: service.id,
            service_type: service.service_type.clone(),
            reason: format!(
                "the service runs on node {node_id}, and a sandbox can only reach services on \
                 the control plane"
            ),
        });
    }
    Ok(())
}

/// Whether `value` is a lowercase identifier safe to splice into a statement:
/// `[a-z0-9_]`, not starting with a digit, at most `max` bytes.
pub(crate) fn is_plain_identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !value.as_bytes()[0].is_ascii_digit()
}

/// The engine for `service_type`, if that type can give a sandbox credentials
/// of its own.
pub fn engine_for(service_type: &str) -> Option<&'static dyn SandboxAccessEngine> {
    ENGINES
        .iter()
        .copied()
        .find(|engine| engine.service_types().contains(&service_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_type_has_an_engine() {
        for service_type in ["postgres", "s3", "rustfs", "redis", "mariadb", "mongodb"] {
            let engine = engine_for(service_type).expect(service_type);
            assert!(engine.service_types().contains(&service_type));
        }
    }

    #[test]
    fn a_type_with_no_engine_is_left_out() {
        for service_type in ["kv", "blob", "", "Postgres", "mysql"] {
            assert!(engine_for(service_type).is_none(), "{service_type}");
        }
    }

    #[test]
    fn no_service_type_is_claimed_by_two_engines() {
        let mut types: Vec<_> = ENGINES
            .iter()
            .flat_map(|engine| engine.service_types().iter().copied())
            .collect();
        let count = types.len();
        types.sort();
        types.dedup();
        assert_eq!(
            types.len(),
            count,
            "two engines claim the same service type"
        );
    }

    #[test]
    fn derived_passwords_differ_by_secret_service_user_and_engine() {
        let base = derive_password("secret", "tag", 1, "user").expect("password");
        assert_eq!(
            base,
            derive_password("secret", "tag", 1, "user").expect("password")
        );
        for other in [
            derive_password("other", "tag", 1, "user"),
            derive_password("secret", "tag", 2, "user"),
            derive_password("secret", "tag", 1, "someone"),
            derive_password("secret", "other-tag", 1, "user"),
        ] {
            assert_ne!(base, other.expect("password"));
        }
    }

    #[test]
    fn identifiers_are_plain_lowercase_or_refused() {
        assert!(is_plain_identifier("shop_sandbox", 63));
        for bad in ["", "Shop", "1shop", "shop-x", "sh op", "a`b", "a'b", "a;b"] {
            assert!(!is_plain_identifier(bad, 63), "{bad:?}");
        }
        assert!(!is_plain_identifier(&"a".repeat(64), 63));
    }
}
