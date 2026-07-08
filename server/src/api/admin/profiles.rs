//! Admin profile endpoints (`/admin/v1/profiles`).
//!
//! Profiles are the admin-managed binding target for client keys: the primary
//! provider *name* plus ordered fallback provider *names* (each matching a
//! `bae-config.toml` `[providers]` registry entry), the opt-in list of MCP
//! server *names* (likewise registry entries), and the client tool allowlist.
//! This router is served only on the loopback admin listener, so there is no
//! auth here initially.
//!
//! Provider references are names, not inline config objects (the WI 0005
//! breaking change, mirroring the WI 0003 `mcp_servers` blob → name-array
//! change): the request/response fields are `primary_provider: string` and
//! `fallback_providers: string[]`. Registry resolution happens at
//! session-creation and message time, never at admin-write time.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::error::ApiError;
use crate::api::pagination::{next_cursor, PageQuery};
use crate::api::AppState;
use crate::store::profiles::{self, DeleteOutcome, ProfileInput, ProfileRecord};

/// `POST /admin/v1/profiles` body.
#[derive(Debug, Deserialize)]
pub struct CreateProfile {
    pub name: String,
    /// The primary provider's `bae-config.toml` registry name.
    pub primary_provider: String,
    /// Ordered fallback provider registry names.
    #[serde(default)]
    pub fallback_providers: Option<Value>,
    #[serde(default)]
    pub mcp_servers: Option<Value>,
    #[serde(default)]
    pub allowed_tools: Option<Value>,
}

impl CreateProfile {
    /// Validate and normalise into a [`ProfileInput`]. Rejects a blank name, a
    /// blank `primary_provider`, and non-string-array `fallback_providers` /
    /// `mcp_servers`, plus a non-array `allowed_tools`.
    fn into_input(self) -> Result<ProfileInput, ApiError> {
        if self.name.trim().is_empty() {
            return Err(ApiError::bad_request("name must not be empty"));
        }
        // `primary_provider` is a provider *name* (a `bae-config.toml`
        // `[providers]` registry entry), not an inline config object. Registry
        // resolution happens at session-creation/message time, exactly like
        // `mcp_servers`; here we only enforce the shape.
        if self.primary_provider.trim().is_empty() {
            return Err(ApiError::bad_request("primary_provider must not be empty"));
        }

        let fallback_providers = self.fallback_providers.unwrap_or_else(|| json!([]));
        require_string_array(&fallback_providers, "fallback_providers")?;
        let mcp_servers = self.mcp_servers.unwrap_or_else(|| json!([]));
        // `mcp_servers` is an array of MCP server *names* (strings) that must
        // match `bae-config.toml` registry entries, not an opaque JSON blob.
        // Registry resolution happens at session-creation time; here we only
        // enforce the shape. Non-string elements are rejected at admin-write
        // time rather than silently ignored later.
        require_string_array(&mcp_servers, "mcp_servers")?;
        let allowed_tools = self.allowed_tools.unwrap_or_else(|| json!([]));
        require_array(&allowed_tools, "allowed_tools")?;

        Ok(ProfileInput {
            name: self.name,
            // Stored in the unchanged TEXT columns as a JSON string / JSON
            // string-array (an application-layer shape change, not a schema
            // change).
            provider_config: json!(self.primary_provider),
            fallback_configs: fallback_providers,
            mcp_servers,
            allowed_tools,
        })
    }
}

fn require_array(v: &Value, field: &str) -> Result<(), ApiError> {
    if v.is_array() {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!("{field} must be an array")))
    }
}

/// Like [`require_array`], but additionally requires every element to be a
/// string. Used for `mcp_servers` and `fallback_providers`, both arrays of
/// `bae-config.toml` registry names.
fn require_string_array(v: &Value, field: &str) -> Result<(), ApiError> {
    let arr = v
        .as_array()
        .ok_or_else(|| ApiError::bad_request(format!("{field} must be an array")))?;
    for (i, el) in arr.iter().enumerate() {
        if !el.is_string() {
            return Err(ApiError::bad_request(format!(
                "{field}[{i}] must be a string (a bae-config.toml registry name)"
            )));
        }
    }
    Ok(())
}

/// Full JSON view of a profile (all fields). Used by GET/PUT responses. The
/// provider fields surface under their name-reference API names
/// (`primary_provider` / `fallback_providers`), not the storage column names.
pub fn profile_view(p: &ProfileRecord) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "primary_provider": p.provider_config,
        "fallback_providers": p.fallback_configs,
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
