//! Admin profile endpoints (`/admin/v1/profiles`).
//!
//! Profiles are the admin-managed binding target for client keys: the primary
//! provider config, ordered fallbacks, MCP servers (stubbed), and the client
//! tool allowlist. This router is served only on the loopback admin listener,
//! so there is no auth here initially.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::error::ApiError;
use crate::api::pagination::{next_cursor, PageQuery};
use crate::api::AppState;
use crate::engine::provider::ProviderConfig;
use crate::store::profiles::{self, DeleteOutcome, ProfileInput, ProfileRecord};

/// `POST /admin/v1/profiles` body.
#[derive(Debug, Deserialize)]
pub struct CreateProfile {
    pub name: String,
    pub provider_config: Value,
    #[serde(default)]
    pub fallback_configs: Option<Value>,
    #[serde(default)]
    pub mcp_servers: Option<Value>,
    #[serde(default)]
    pub allowed_tools: Option<Value>,
}

impl CreateProfile {
    /// Validate and normalise into a [`ProfileInput`]. Rejects a blank name, a
    /// provider config that does not match the schema, and non-array
    /// `fallback_configs` / `mcp_servers` / `allowed_tools`.
    fn into_input(self) -> Result<ProfileInput, ApiError> {
        if self.name.trim().is_empty() {
            return Err(ApiError::bad_request("name must not be empty"));
        }
        validate_provider_config(&self.provider_config)?;

        let fallback_configs = self.fallback_configs.unwrap_or_else(|| json!([]));
        validate_fallbacks(&fallback_configs)?;
        let mcp_servers = self.mcp_servers.unwrap_or_else(|| json!([]));
        require_array(&mcp_servers, "mcp_servers")?;
        let allowed_tools = self.allowed_tools.unwrap_or_else(|| json!([]));
        require_array(&allowed_tools, "allowed_tools")?;

        Ok(ProfileInput {
            name: self.name,
            provider_config: self.provider_config,
            fallback_configs,
            mcp_servers,
            allowed_tools,
        })
    }
}

fn validate_provider_config(v: &Value) -> Result<(), ApiError> {
    serde_json::from_value::<ProviderConfig>(v.clone())
        .map_err(|e| ApiError::bad_request(format!("provider_config is not valid: {e}")))?;
    Ok(())
}

fn validate_fallbacks(v: &Value) -> Result<(), ApiError> {
    let arr = v
        .as_array()
        .ok_or_else(|| ApiError::bad_request("fallback_configs must be an array"))?;
    for (i, cfg) in arr.iter().enumerate() {
        serde_json::from_value::<ProviderConfig>(cfg.clone()).map_err(|e| {
            ApiError::bad_request(format!("fallback_configs[{i}] is not valid: {e}"))
        })?;
    }
    Ok(())
}

fn require_array(v: &Value, field: &str) -> Result<(), ApiError> {
    if v.is_array() {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!("{field} must be an array")))
    }
}

/// Full JSON view of a profile (all fields). Used by GET/PUT responses.
pub fn profile_view(p: &ProfileRecord) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "provider_config": p.provider_config,
        "fallback_configs": p.fallback_configs,
        "mcp_servers": p.mcp_servers,
        "allowed_tools": p.allowed_tools,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    })
}

/// `POST /admin/v1/profiles`
pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateProfile>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let input = body.into_input()?;
    let record = state
        .store
        .with_conn(|c| profiles::create(c, &input))
        .map_err(map_create_err)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": record.id,
            "name": record.name,
            "created_at": record.created_at,
        })),
    ))
}

/// `GET /admin/v1/profiles`
pub async fn list(
    State(state): State<AppState>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    let (after, limit) = page.resolve()?;
    let (rows, has_more) = state
        .store
        .with_conn(|c| profiles::list(c, after, limit))
        .map_err(ApiError::from_db)?;
    let last_rowid = rows.last().map(|(rid, _)| *rid);
    let items: Vec<Value> = rows.iter().map(|(_, p)| profile_view(p)).collect();
    Ok(Json(json!({
        "items": items,
        "next_cursor": next_cursor(last_rowid, has_more),
    })))
}

/// `GET /admin/v1/profiles/:id`
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = state
        .store
        .with_conn(|c| profiles::get(c, &id))
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found(format!("no profile {id}")))?;
    Ok(Json(profile_view(&record)))
}

/// `PUT /admin/v1/profiles/:id`
pub async fn replace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreateProfile>,
) -> Result<Json<Value>, ApiError> {
    let input = body.into_input()?;
    let record = state
        .store
        .with_conn(|c| profiles::replace(c, &id, &input))
        .map_err(map_create_err)?
        .ok_or_else(|| ApiError::not_found(format!("no profile {id}")))?;
    Ok(Json(profile_view(&record)))
}

/// `DELETE /admin/v1/profiles/:id`
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let outcome = state
        .store
        .with_conn(|c| profiles::soft_delete(c, &id))
        .map_err(ApiError::from_db)?;
    match outcome {
        DeleteOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        DeleteOutcome::NotFound => Err(ApiError::not_found(format!("no profile {id}"))),
        DeleteOutcome::HasActiveKeys(n) => Err(ApiError::conflict(
            "profile_in_use",
            format!("profile has {n} active client key(s); revoke them before deleting"),
        )),
    }
}

fn map_create_err(e: profiles::CreateError) -> ApiError {
    match e {
        profiles::CreateError::Duplicate => {
            ApiError::conflict("duplicate_name", "a profile with that name already exists")
        }
        profiles::CreateError::Sqlite(e) => ApiError::from_db(e),
    }
}
