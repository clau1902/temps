// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The pieces a chat has made: the projects, databases and services, and
//! repositories its confirmed write actions created, plus the sandbox linked to
//! it. The chat's right-hand canvas lists them so the user can see everything
//! the conversation put in place.
//!
//! Nothing new is recorded when a piece is created. Every write the assistant
//! performs already lands in `ai_pending_actions` with the operation that ran
//! and the response it returned, so the list is read back from the executed
//! rows and checked against the live tables. A piece that has since been
//! deleted drops out of the list, and one whose state changed shows its current
//! state rather than the state at creation time.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use temps_entities::{
    ai_conversations, ai_pending_actions, external_services, projects, sandboxes,
};
use thiserror::Error;
use utoipa::ToSchema;

use crate::sandbox_link::link_from_metadata;

/// Most creating actions read for one chat. A chat that made more than this
/// reports `truncated` and lists the newest ones.
pub const MAX_RESOURCES: u64 = 100;

const OP_CREATE_PROJECT: &str = "create_project";
const OP_CREATE_SERVICE: &str = "create_service";
const OP_CREATE_REPOSITORY: &str = "create_repository";

/// Write operations whose executed result names a piece the chat made.
const CREATING_OPERATIONS: [&str; 3] = [OP_CREATE_PROJECT, OP_CREATE_SERVICE, OP_CREATE_REPOSITORY];

#[derive(Debug, Error)]
pub enum ChatResourceError {
    #[error("Failed to list the resources created by conversation {conversation_id}: {reason}")]
    Database {
        conversation_id: String,
        reason: String,
    },
}

/// What kind of piece a chat made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatResourceKind {
    /// A Temps project.
    Project,
    /// An external service: a database, cache, object store or similar.
    Service,
    /// A Git repository created through a connected provider.
    Repository,
    /// The sandbox linked to the chat.
    Sandbox,
}

/// One piece the chat made, with its current state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ChatResource {
    pub kind: ChatResourceKind,
    /// Numeric id for projects and services, `owner/name` for repositories,
    /// public id (`sbx_…`) for sandboxes.
    pub id: String,
    pub name: String,
    /// Project slug, for opening the project.
    pub slug: Option<String>,
    /// What it is: the framework preset, the service type, the repository's
    /// visibility, the sandbox backend.
    pub detail: Option<String>,
    /// Current status, for services and sandboxes.
    pub status: Option<String>,
    /// Where to see it outside Temps: a repository's web page.
    pub url: Option<String>,
    /// ISO 8601 time the piece was made (or, for a sandbox, linked).
    pub created_at: DateTime<Utc>,
}

/// Everything the chat made, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ChatResourcesResponse {
    pub resources: Vec<ChatResource>,
    /// True when the chat made more pieces than this response covers.
    pub truncated: bool,
}

/// What an executed action created, as far as its own record says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Created {
    Project {
        id: i32,
    },
    Service {
        id: i32,
        service_type: Option<String>,
    },
    Repository {
        full_name: String,
        name: String,
        private: Option<bool>,
        web_url: Option<String>,
    },
}

fn id_from(value: &Value) -> Option<i32> {
    i32::try_from(value.get("id")?.as_i64()?).ok()
}

fn text_from(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Read what an executed creating action made out of its recorded result.
/// `None` for any other operation and for a result with no usable identity.
fn created_by(operation_id: &str, params: &Value, result: Option<&Value>) -> Option<Created> {
    let result = result?;
    match operation_id {
        OP_CREATE_PROJECT => Some(Created::Project {
            id: id_from(result)?,
        }),
        OP_CREATE_SERVICE => Some(Created::Service {
            id: id_from(result)?,
            service_type: params
                .get("__temps_display")
                .and_then(|display| text_from(display, "service_type")),
        }),
        OP_CREATE_REPOSITORY => {
            let name = text_from(result, "name")?;
            let full_name = text_from(result, "full_name")
                .or_else(|| Some(format!("{}/{}", text_from(result, "owner")?, name)))?;
            Some(Created::Repository {
                full_name,
                name,
                private: result.get("private").and_then(Value::as_bool),
                web_url: text_from(result, "web_url"),
            })
        }
        _ => None,
    }
}

/// Reads the pieces a conversation made.
pub struct ChatResourceService {
    db: Arc<DatabaseConnection>,
}

impl ChatResourceService {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }

    /// The pieces `conversation` made that still exist, newest first. The
    /// caller has already shown the conversation is theirs; `user_id` limits the
    /// linked sandbox to one they own.
    pub async fn list(
        &self,
        conversation: &ai_conversations::Model,
        user_id: i32,
    ) -> Result<ChatResourcesResponse, ChatResourceError> {
        let database = |reason: sea_orm::DbErr| ChatResourceError::Database {
            conversation_id: conversation.public_id.clone(),
            reason: reason.to_string(),
        };

        let mut actions = ai_pending_actions::Entity::find()
            .filter(ai_pending_actions::Column::ConversationId.eq(conversation.id))
            .filter(ai_pending_actions::Column::Status.eq("executed"))
            .filter(ai_pending_actions::Column::OperationId.is_in(CREATING_OPERATIONS))
            .order_by(ai_pending_actions::Column::CreatedAt, Order::Desc)
            .order_by(ai_pending_actions::Column::Id, Order::Desc)
            .limit(MAX_RESOURCES + 1)
            .all(self.db.as_ref())
            .await
            .map_err(database)?;
        let truncated = actions.len() as u64 > MAX_RESOURCES;
        actions.truncate(MAX_RESOURCES as usize);

        let created: Vec<(Created, DateTime<Utc>)> = actions
            .iter()
            .filter_map(|action| {
                let made =
                    created_by(&action.operation_id, &action.params, action.result.as_ref())?;
                Some((made, action.executed_at.unwrap_or(action.created_at)))
            })
            .collect();

        let project_ids: Vec<i32> = created
            .iter()
            .filter_map(|(made, _)| match made {
                Created::Project { id } => Some(*id),
                _ => None,
            })
            .collect();
        let service_ids: Vec<i32> = created
            .iter()
            .filter_map(|(made, _)| match made {
                Created::Service { id, .. } => Some(*id),
                _ => None,
            })
            .collect();

        let live_projects: HashMap<i32, projects::Model> = if project_ids.is_empty() {
            HashMap::new()
        } else {
            projects::Entity::find()
                .filter(projects::Column::Id.is_in(project_ids))
                .filter(projects::Column::IsDeleted.eq(false))
                .all(self.db.as_ref())
                .await
                .map_err(database)?
                .into_iter()
                .map(|project| (project.id, project))
                .collect()
        };
        let live_services: HashMap<i32, external_services::Model> = if service_ids.is_empty() {
            HashMap::new()
        } else {
            external_services::Entity::find()
                .filter(external_services::Column::Id.is_in(service_ids))
                .all(self.db.as_ref())
                .await
                .map_err(database)?
                .into_iter()
                .map(|service| (service.id, service))
                .collect()
        };

        let mut resources: Vec<ChatResource> = Vec::with_capacity(created.len() + 1);
        for (made, at) in created {
            let resource = match made {
                Created::Project { id } => live_projects.get(&id).map(|project| ChatResource {
                    kind: ChatResourceKind::Project,
                    id: project.id.to_string(),
                    name: project.name.clone(),
                    slug: Some(project.slug.clone()),
                    detail: Some(project.preset.display_name().to_string()),
                    status: None,
                    url: None,
                    created_at: at,
                }),
                Created::Service { id, service_type } => {
                    live_services.get(&id).map(|service| ChatResource {
                        kind: ChatResourceKind::Service,
                        id: service.id.to_string(),
                        name: service.name.clone(),
                        slug: None,
                        detail: Some(service_type.unwrap_or_else(|| service.service_type.clone())),
                        status: Some(service.status.clone()),
                        url: None,
                        created_at: at,
                    })
                }
                Created::Repository {
                    full_name,
                    name,
                    private,
                    web_url,
                } => Some(ChatResource {
                    kind: ChatResourceKind::Repository,
                    id: full_name,
                    name,
                    slug: None,
                    detail: private
                        .map(|private| if private { "Private" } else { "Public" }.to_string()),
                    status: None,
                    url: web_url,
                    created_at: at,
                }),
            };
            if let Some(resource) = resource {
                if !resources
                    .iter()
                    .any(|seen| seen.kind == resource.kind && seen.id == resource.id)
                {
                    resources.push(resource);
                }
            }
        }

        if let Some(link) = link_from_metadata(conversation.metadata.as_ref()) {
            let sandbox = sandboxes::Entity::find()
                .filter(sandboxes::Column::PublicId.eq(link.sandbox_id.as_str()))
                .filter(sandboxes::Column::UserId.eq(user_id))
                .filter(sandboxes::Column::Status.ne("destroyed"))
                .one(self.db.as_ref())
                .await
                .map_err(database)?;
            if let Some(sandbox) = sandbox {
                resources.push(ChatResource {
                    kind: ChatResourceKind::Sandbox,
                    id: sandbox.public_id,
                    name: sandbox.name,
                    slug: None,
                    detail: sandbox.backend,
                    status: Some(sandbox.status),
                    url: None,
                    created_at: link.linked_at,
                });
            }
        }
        resources.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        Ok(ChatResourcesResponse {
            resources,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_created_project_is_read_from_its_result() {
        let result = json!({"id": 36, "name": "demo-app", "slug": "demo-app"});
        assert_eq!(
            created_by(OP_CREATE_PROJECT, &json!({}), Some(&result)),
            Some(Created::Project { id: 36 })
        );
    }

    #[test]
    fn a_created_service_keeps_the_type_the_user_asked_for() {
        let params = json!({"__temps_display": {"name": "cache", "service_type": "redis"}});
        let result = json!({"id": 7, "name": "cache", "status": "starting"});
        assert_eq!(
            created_by(OP_CREATE_SERVICE, &params, Some(&result)),
            Some(Created::Service {
                id: 7,
                service_type: Some("redis".to_string())
            })
        );
    }

    #[test]
    fn a_service_without_a_display_type_is_still_found() {
        let result = json!({"id": 7});
        assert_eq!(
            created_by(OP_CREATE_SERVICE, &json!({"encrypted": "x"}), Some(&result)),
            Some(Created::Service {
                id: 7,
                service_type: None
            })
        );
    }

    #[test]
    fn a_created_repository_is_read_from_its_result() {
        let result = json!({
            "name": "demo",
            "owner": "acme",
            "private": true,
            "web_url": "https://git.example.com/acme/demo",
            "full_name": "acme/demo",
        });
        assert_eq!(
            created_by(OP_CREATE_REPOSITORY, &json!({}), Some(&result)),
            Some(Created::Repository {
                full_name: "acme/demo".to_string(),
                name: "demo".to_string(),
                private: Some(true),
                web_url: Some("https://git.example.com/acme/demo".to_string()),
            })
        );
    }

    #[test]
    fn a_repository_without_a_full_name_is_named_from_owner_and_name() {
        let result = json!({"name": "demo", "owner": "acme"});
        assert_eq!(
            created_by(OP_CREATE_REPOSITORY, &json!({}), Some(&result)),
            Some(Created::Repository {
                full_name: "acme/demo".to_string(),
                name: "demo".to_string(),
                private: None,
                web_url: None,
            })
        );
    }

    #[test]
    fn results_without_an_identity_name_nothing() {
        assert_eq!(created_by(OP_CREATE_PROJECT, &json!({}), None), None);
        assert_eq!(
            created_by(OP_CREATE_PROJECT, &json!({}), Some(&json!({"name": "x"}))),
            None
        );
        assert_eq!(
            created_by(
                OP_CREATE_PROJECT,
                &json!({}),
                Some(&json!({"id": "not-a-number"}))
            ),
            None
        );
        assert_eq!(
            created_by(
                OP_CREATE_PROJECT,
                &json!({}),
                Some(&json!({"id": 99999999999_i64}))
            ),
            None
        );
        assert_eq!(
            created_by(
                OP_CREATE_REPOSITORY,
                &json!({}),
                Some(&json!({"owner": "acme"}))
            ),
            None
        );
    }

    #[test]
    fn other_operations_create_nothing() {
        let result = json!({"id": 1, "name": "x"});
        assert_eq!(
            created_by("trigger_project_pipeline", &json!({}), Some(&result)),
            None
        );
        assert_eq!(
            created_by("link_service_to_project", &json!({}), Some(&result)),
            None
        );
    }

    #[test]
    fn every_creating_operation_is_read_back() {
        for operation in CREATING_OPERATIONS {
            let result = json!({"id": 1, "name": "x", "owner": "o"});
            assert!(
                created_by(operation, &json!({}), Some(&result)).is_some(),
                "{operation} is queried but never parsed"
            );
        }
    }

    #[test]
    fn kinds_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_value(ChatResourceKind::Repository).ok(),
            Some(json!("repository"))
        );
    }
}
