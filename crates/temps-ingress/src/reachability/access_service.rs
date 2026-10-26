// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Saved Cloudflare Access tokens (ADR 065 D9): checked before they are
//! saved, sealed with `EncryptionService`, never returned, and unsealed
//! only for the plan and the run that use them.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, Set,
};
use serde::{Deserialize, Serialize};
use temps_core::EncryptionService;
use temps_entities::{cloudflare_access_providers, environment_domains, project_custom_domains};
use temps_routes::access_keys::AccessKeyService;
use thiserror::Error;
use tracing::warn;
use utoipa::ToSchema;

use super::access::{
    self, check_access_token, AccessApp, AccessPolicyAnswer, AccessTokenCheckResult, CreateAppError,
};

#[derive(Debug, Error)]
pub enum AccessProviderError {
    #[error("Cloudflare Access token {provider_id} not found")]
    NotFound { provider_id: i32 },
    #[error("Validation error: {message}")]
    Validation { message: String },
    #[error("The Cloudflare Access token was not saved: {reason}")]
    TokenRefused {
        reason: String,
        check: Box<AccessTokenCheckResult>,
    },
    #[error(
        "Cloudflare Access token {provider_id} protects {addresses} address(es); switch them to \
         the Temps sign-in or remove them before deleting it"
    )]
    InUse { provider_id: i32, addresses: u64 },
    #[error("Failed to {operation} the Cloudflare Access token {provider_id:?}: {reason}")]
    Encryption {
        operation: &'static str,
        provider_id: Option<i32>,
        reason: String,
    },
    #[error("Failed to {operation} for Cloudflare Access tokens: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sea_orm::DbErr,
    },
}

fn db(operation: &'static str) -> impl FnOnce(sea_orm::DbErr) -> AccessProviderError {
    move |source| AccessProviderError::Database { operation, source }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateAccessProviderRequest {
    /// A name to tell tokens apart.
    pub name: String,
    pub account_id: String,
    /// Checked, then sealed; never returned.
    pub token: String,
}

/// A saved Access token. The token itself is never returned.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AccessProviderResponse {
    pub id: i32,
    pub name: String,
    pub account_id: String,
    /// `<team>.cloudflareaccess.com`.
    pub team_domain: String,
    /// Always `***`.
    pub api_token: String,
    /// When the team's signing keys were last fetched.
    #[schema(value_type = Option<String>, format = DateTime)]
    pub keys_fetched_at: Option<DateTime<Utc>>,
    /// How many signing keys listeners have for it now.
    pub key_count: usize,
    /// How many addresses it protects.
    pub addresses: u64,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: DateTime<Utc>,
}

/// What a plan or run needs from a saved token.
#[derive(Clone)]
pub struct AccessCredentials {
    pub provider_id: i32,
    pub account_id: String,
    pub team_domain: String,
    pub token: String,
}

impl std::fmt::Debug for AccessCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessCredentials")
            .field("provider_id", &self.provider_id)
            .field("account_id", &self.account_id)
            .field("team_domain", &self.team_domain)
            .field("token", &"***")
            .finish()
    }
}

pub struct AccessProviderService {
    db: Arc<DatabaseConnection>,
    encryption: Arc<EncryptionService>,
    keys: Arc<AccessKeyService>,
    http: reqwest::Client,
}

impl AccessProviderService {
    pub fn new(
        db: Arc<DatabaseConnection>,
        encryption: Arc<EncryptionService>,
        keys: Arc<AccessKeyService>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            db,
            encryption,
            keys,
            http,
        }
    }

    /// Check a token without saving it.
    pub async fn check(&self, token: &str, account_id: &str) -> AccessTokenCheckResult {
        check_access_token(&self.http, token, account_id).await
    }

    async fn addresses_using(&self, provider_id: i32) -> Result<u64, AccessProviderError> {
        let custom = project_custom_domains::Entity::find()
            .filter(project_custom_domains::Column::GateAccessProviderId.eq(provider_id))
            .count(self.db.as_ref())
            .await
            .map_err(db("count the custom domains using a token"))?;
        let environment = environment_domains::Entity::find()
            .filter(environment_domains::Column::GateAccessProviderId.eq(provider_id))
            .count(self.db.as_ref())
            .await
            .map_err(db("count the environment domains using a token"))?;
        Ok(custom + environment)
    }

    async fn respond(
        &self,
        model: cloudflare_access_providers::Model,
    ) -> Result<AccessProviderResponse, AccessProviderError> {
        Ok(AccessProviderResponse {
            addresses: self.addresses_using(model.id).await?,
            key_count: temps_routes::access_keys::stored_keys(model.jwks.as_ref()).len(),
            id: model.id,
            name: model.name,
            account_id: model.account_id,
            team_domain: model.team_domain,
            api_token: "***".to_string(),
            keys_fetched_at: model.jwks_fetched_at,
            created_at: model.created_at,
        })
    }

    /// Every saved token, newest first. There are a handful at most, so
    /// this is not paginated.
    pub async fn list(&self) -> Result<Vec<AccessProviderResponse>, AccessProviderError> {
        let models = cloudflare_access_providers::Entity::find()
            .order_by_desc(cloudflare_access_providers::Column::CreatedAt)
            .all(self.db.as_ref())
            .await
            .map_err(db("list the tokens"))?;
        let mut out = Vec::with_capacity(models.len());
        for model in models {
            out.push(self.respond(model).await?);
        }
        Ok(out)
    }

    /// The saved tokens as `(id, name, team domain)`, for plans.
    pub async fn names(&self) -> Result<Vec<(i32, String, String)>, AccessProviderError> {
        Ok(cloudflare_access_providers::Entity::find()
            .order_by_asc(cloudflare_access_providers::Column::Id)
            .all(self.db.as_ref())
            .await
            .map_err(db("list the tokens"))?
            .into_iter()
            .map(|m| (m.id, m.name, m.team_domain))
            .collect())
    }

    /// Check the token, then save it sealed, then fetch the team's signing
    /// keys (a failure there is logged; the hourly refresh retries).
    pub async fn create(
        &self,
        request: &CreateAccessProviderRequest,
        user_id: Option<i32>,
    ) -> Result<AccessProviderResponse, AccessProviderError> {
        let name = request.name.trim();
        if name.is_empty() || name.len() > 255 {
            return Err(AccessProviderError::Validation {
                message: "Give the token a name of 1 to 255 characters.".into(),
            });
        }
        let account_id = request.account_id.trim().to_ascii_lowercase();
        let check = self.check(&request.token, &account_id).await;
        if let Some(reason) = check.refusal() {
            return Err(AccessProviderError::TokenRefused {
                reason,
                check: Box::new(check),
            });
        }
        let Some(team_domain) = check.team_domain.clone() else {
            return Err(AccessProviderError::Validation {
                message: "Cloudflare did not say which Access team the account uses.".into(),
            });
        };
        let sealed = self
            .encryption
            .encrypt_string(request.token.trim())
            .map_err(|e| AccessProviderError::Encryption {
                operation: "seal",
                provider_id: None,
                reason: e.to_string(),
            })?;
        let now = Utc::now();
        let model = cloudflare_access_providers::ActiveModel {
            name: Set(name.to_string()),
            account_id: Set(account_id),
            team_domain: Set(team_domain),
            api_token_encrypted: Set(sealed),
            jwks: Set(None),
            jwks_fetched_at: Set(None),
            created_by: Set(user_id),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(self.db.as_ref())
        .await
        .map_err(db("save the token"))?;
        let model = match self.keys.refresh(model.id, true).await {
            Ok(_) => cloudflare_access_providers::Entity::find_by_id(model.id)
                .one(self.db.as_ref())
                .await
                .map_err(db("read the saved token"))?
                .unwrap_or(model),
            Err(error) => {
                warn!(provider_id = model.id, %error, "could not fetch the Access signing keys of a new token");
                model
            }
        };
        self.respond(model).await
    }

    /// Delete a token no address uses.
    pub async fn delete(
        &self,
        provider_id: i32,
    ) -> Result<cloudflare_access_providers::Model, AccessProviderError> {
        let model = cloudflare_access_providers::Entity::find_by_id(provider_id)
            .one(self.db.as_ref())
            .await
            .map_err(db("load the token"))?
            .ok_or(AccessProviderError::NotFound { provider_id })?;
        let addresses = self.addresses_using(provider_id).await?;
        if addresses > 0 {
            return Err(AccessProviderError::InUse {
                provider_id,
                addresses,
            });
        }
        cloudflare_access_providers::Entity::delete_by_id(provider_id)
            .exec(self.db.as_ref())
            .await
            .map_err(db("delete the token"))?;
        Ok(model)
    }

    /// The token, unsealed, for a plan or a run.
    pub async fn credentials(
        &self,
        provider_id: i32,
    ) -> Result<AccessCredentials, AccessProviderError> {
        let model = cloudflare_access_providers::Entity::find_by_id(provider_id)
            .one(self.db.as_ref())
            .await
            .map_err(db("load the token"))?
            .ok_or(AccessProviderError::NotFound { provider_id })?;
        let token = self
            .encryption
            .decrypt_string(&model.api_token_encrypted)
            .map_err(|e| AccessProviderError::Encryption {
                operation: "unseal",
                provider_id: Some(provider_id),
                reason: e.to_string(),
            })?;
        Ok(AccessCredentials {
            provider_id,
            account_id: model.account_id,
            team_domain: model.team_domain,
            token,
        })
    }

    /// The apps in `provider_id`'s account that protect every path of
    /// `host`, for a plan (ADR 065 D9).
    pub async fn covering_apps(
        &self,
        provider_id: i32,
        host: &str,
    ) -> Result<Vec<AccessApp>, String> {
        let credentials = self
            .credentials(provider_id)
            .await
            .map_err(|e| e.to_string())?;
        let apps =
            access::list_apps(&self.http, &credentials.token, &credentials.account_id).await?;
        Ok(apps
            .into_iter()
            .filter(|app| access::covers(app, host))
            .collect())
    }

    /// The app named `name` protecting `host`: how a run finds the app it
    /// set out to create.
    pub async fn app_named(
        &self,
        provider_id: i32,
        name: &str,
        host: &str,
    ) -> Result<Option<AccessApp>, String> {
        let credentials = self
            .credentials(provider_id)
            .await
            .map_err(|e| e.to_string())?;
        let apps =
            access::list_apps(&self.http, &credentials.token, &credentials.account_id).await?;
        Ok(apps
            .into_iter()
            .find(|app| app.name == name && access::covers(app, host)))
    }

    pub async fn app(&self, provider_id: i32, id: &str) -> Result<Option<AccessApp>, String> {
        let credentials = self
            .credentials(provider_id)
            .await
            .map_err(|e| e.to_string())?;
        access::get_app(&self.http, &credentials.token, &credentials.account_id, id).await
    }

    pub async fn create_app(
        &self,
        provider_id: i32,
        name: &str,
        host: &str,
        policy: &AccessPolicyAnswer,
    ) -> Result<AccessApp, CreateAppError> {
        let credentials = self
            .credentials(provider_id)
            .await
            .map_err(|e| CreateAppError {
                message: e.to_string(),
                maybe_applied: false,
            })?;
        access::create_app(
            &self.http,
            &credentials.token,
            &credentials.account_id,
            access::app_body(name, host, policy),
        )
        .await
    }

    pub async fn delete_app(&self, provider_id: i32, id: &str) -> Result<(), String> {
        let credentials = self
            .credentials(provider_id)
            .await
            .map_err(|e| e.to_string())?;
        access::delete_app(&self.http, &credentials.token, &credentials.account_id, id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(db: DatabaseConnection) -> AccessProviderService {
        let db = Arc::new(db);
        AccessProviderService::new(
            db.clone(),
            Arc::new(EncryptionService::new_from_password("access-test")),
            Arc::new(AccessKeyService::new(db)),
            reqwest::Client::new(),
        )
    }

    #[test]
    fn credentials_never_print_the_token() {
        let credentials = AccessCredentials {
            provider_id: 1,
            account_id: "a".into(),
            team_domain: "team.cloudflareaccess.com".into(),
            token: "secret-token-value".into(),
        };
        assert!(!format!("{credentials:?}").contains("secret-token-value"));
    }

    #[tokio::test]
    async fn a_nameless_token_is_refused_before_cloudflare_is_called() {
        let db = sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Postgres).into_connection();
        let result = service(db)
            .create(
                &CreateAccessProviderRequest {
                    name: "  ".into(),
                    account_id: "01a7362d577a6c3019a474fd6f485823".into(),
                    token: "t".into(),
                },
                Some(1),
            )
            .await;
        assert!(matches!(
            result,
            Err(AccessProviderError::Validation { .. })
        ));
    }

    #[tokio::test]
    async fn a_token_that_protects_an_address_is_not_deleted() {
        let now = Utc::now();
        let db = sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Postgres)
            .append_query_results([vec![cloudflare_access_providers::Model {
                id: 4,
                name: "access".into(),
                account_id: "a".into(),
                team_domain: "team.cloudflareaccess.com".into(),
                api_token_encrypted: "sealed".into(),
                jwks: None,
                jwks_fetched_at: None,
                created_by: None,
                created_at: now,
                updated_at: now,
            }]])
            .append_query_results([vec![std::collections::BTreeMap::from([(
                "num_items".to_string(),
                sea_orm::Value::BigInt(Some(1)),
            )])]])
            .append_query_results([vec![std::collections::BTreeMap::from([(
                "num_items".to_string(),
                sea_orm::Value::BigInt(Some(0)),
            )])]])
            .into_connection();
        let result = service(db).delete(4).await;
        assert!(matches!(
            result,
            Err(AccessProviderError::InUse {
                provider_id: 4,
                addresses: 1
            })
        ));
    }

    #[tokio::test]
    async fn an_unknown_token_is_not_found() {
        let db = sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Postgres)
            .append_query_results([Vec::<cloudflare_access_providers::Model>::new()])
            .into_connection();
        assert!(matches!(
            service(db).credentials(9).await,
            Err(AccessProviderError::NotFound { provider_id: 9 })
        ));
    }
}
