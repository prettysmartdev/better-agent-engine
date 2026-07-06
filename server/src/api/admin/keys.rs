//! Admin client-key endpoints (`/admin/v1/keys`).
//!
//! Client keys are issued here (plaintext shown exactly once), listed without
//! ever exposing `key_hash`, and revoked. Revoking a key also invalidates every
//! session it opened. Served only on the loopback admin listener.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::error::ApiError;
use crate::api::pagination::{next_cursor, PageQuery};
use crate::api::AppState;
use crate::events::EventType;
use crate::store::keys::{self, KeyRecord};
use crate::store::{profiles, sessions};

/// `POST /admin/v1/keys` body.
#[derive(Debug, Deserialize)]
pub struct CreateKey {
    pub name: String,
    pub profile_id: String,
}

/// One row of `GET /admin/v1/keys`. Note: no `key_hash`, ever.
fn key_view(k: &KeyRecord) -> Value {
    json!({
        "id": k.id,
        "name": k.name,
        "prefix": k.prefix,
        "profile_id": k.profile_id,
        "created_at": k.created_at,
        "last_used_at": k.last_used_at,
    })
}

/// `POST /admin/v1/keys`
///
/// Validates the profile exists and is not deleted, then issues a `bae_<random>`
/// client key. The plaintext is returned exactly once in this response and never
/// again.
pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateKey>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    let generated = keys::generate_client_key();

    // Validate the profile and insert the key under one lock so the profile
    // cannot be deleted between the check and the insert.
    let result = state.store.with_conn(|c| {
        let profile = profiles::get(c, &body.profile_id).map_err(ApiError::from_db)?;
        if profile.is_none() {
            return Err(ApiError::unprocessable(
                "profile_unavailable",
                format!("no active profile {}", body.profile_id),
            ));
        }
        keys::insert_client_key(c, &body.name, &body.profile_id, &generated).map_err(|e| match e {
            keys::InsertError::Sqlite(e) => ApiError::from_db(e),
            keys::InsertError::Key(e) => {
                tracing::error!("key hashing failed: {e}");
                ApiError::internal("failed to hash key")
            }
        })
    })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": result.id,
            "name": result.name,
            "key": generated.plaintext,
            "prefix": result.prefix,
            "profile_id": result.profile_id,
            "created_at": result.created_at,
        })),
    ))
}

/// `GET /admin/v1/keys`
pub async fn list(
    State(state): State<AppState>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    let (after, limit) = page.resolve()?;
    let (rows, has_more) = state
        .store
        .with_conn(|c| keys::list_client_keys(c, after, limit))
        .map_err(ApiError::from_db)?;
    let last_rowid = rows.last().map(|(rid, _)| *rid);
    let items: Vec<Value> = rows.iter().map(|(_, k)| key_view(k)).collect();
    Ok(Json(json!({
        "items": items,
        "next_cursor": next_cursor(last_rowid, has_more),
    })))
}

/// `DELETE /admin/v1/keys/:id`
///
/// Revokes the key and invalidates its open sessions, logging a `session.close`
/// event on each.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let closed = state
        .store
        .with_conn(|c| keys::revoke_client_key(c, &id))
        .map_err(ApiError::from_db)?;
    let Some(closed_sessions) = closed else {
        return Err(ApiError::not_found(format!("no active client key {id}")));
    };
    for session_id in &closed_sessions {
        let payload = json!({ "reason": "client_key_revoked" });
        state
            .store
            .with_conn(|c| {
                sessions::insert_event(c, session_id, Some(&id), EventType::SessionClose, &payload)
            })
            .map_err(ApiError::from_db)?;
    }
    Ok(StatusCode::NO_CONTENT)
}
