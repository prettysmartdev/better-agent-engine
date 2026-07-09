//! Admin session-visibility endpoints (`/admin/v1/sessions`).
//!
//! Read-only, mirroring the `mcp-servers`/`providers` "state visibility without
//! secrets" pattern — but DB-driven like `profiles`/`keys`. These are the
//! **only** admin-port windows onto session data, which otherwise lives entirely
//! behind session-key-authenticated client-port routes: they let an operator (or
//! MAX) list sessions and read any session's full event history *without ever
//! holding a session key*, including for `closed`/`error` sessions that no
//! client-port `join` could ever reach. Served on the loopback admin listener,
//! plain HTTP/REST like every other admin endpoint; admin auth already gates the
//! whole port, so there is no per-session key check here.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::client::sessions::event_view;
use crate::api::error::ApiError;
use crate::api::pagination::{next_cursor, PageQuery};
use crate::api::AppState;
use crate::store::sessions::{self, SessionRecord, STATE_CLOSED, STATE_ERROR, STATE_OPEN};

/// `?cursor=&limit=&state=` query for the session list: the shared cursor
/// pagination fields plus an optional lifecycle-state filter.
///
/// The `PageQuery` fields are declared inline rather than via `#[serde(flatten)]`
/// because `serde_urlencoded` (axum's query deserializer) cannot coerce a
/// numeric field like `limit=2` through a flattened struct — flatten forces the
/// self-describing map path, where every value stays a string and `limit`'s
/// `i64` deserialize then fails with a 400. Declaring the fields directly keeps
/// the same wire shape while letting `limit` parse as an integer, and
/// [`page`](Self::page) rebuilds a [`PageQuery`] so cursor/limit resolution stays
/// identical to `profiles`/`keys`.
#[derive(Debug, Default, Deserialize)]
pub struct SessionListQuery {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    /// Optional `open`/`closed`/`error` filter; any other value is a 400.
    pub state: Option<String>,
}

impl SessionListQuery {
    /// The pagination view of this query, resolved exactly as every other list
    /// endpoint's [`PageQuery`].
    fn page(&self) -> PageQuery {
        PageQuery {
            cursor: self.cursor.clone(),
            limit: self.limit,
        }
    }
}

/// List view of one session. Deliberately omits `client_key_id` and
/// `client_tools` — the latter is noisy per-client JSON not needed for a list
/// view (the full event history via `.../events` carries everything).
fn session_view(s: &SessionRecord) -> Value {
    json!({
        "id": s.id,
        "profile_id": s.profile_id,
        "state": s.state,
        "client_version": s.client_version,
        "created_at": s.created_at,
        "closed_at": s.closed_at,
    })
}

/// `GET /admin/v1/sessions`
///
/// Cursor-paginated (`?cursor=&limit=`) list of sessions, newest-appended last
/// (rowid order), with an optional `?state=open|closed|error` filter. Returns
/// `{items, next_cursor}` exactly like `profiles`/`keys`.
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<Value>, ApiError> {
    let (after, limit) = query.page().resolve()?;
    let filter = match query.state.as_deref() {
        None | Some("") => None,
        Some(s) if s == STATE_OPEN || s == STATE_CLOSED || s == STATE_ERROR => Some(s),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "invalid state filter {other:?}; expected one of open, closed, error"
            )))
        }
    };
    let (rows, has_more) = state
        .store
        .with_conn(|c| sessions::list_sessions(c, after, limit, filter))
        .map_err(ApiError::from_db)?;
    let last_rowid = rows.last().map(|(rid, _)| *rid);
    let items: Vec<Value> = rows.iter().map(|(_, s)| session_view(s)).collect();
    Ok(Json(json!({
        "items": items,
        "next_cursor": next_cursor(last_rowid, has_more),
    })))
}

/// `GET /admin/v1/sessions/{id}/events`
///
/// The same event history — byte-for-byte the same per-row shape (reusing
/// [`event_view`]) — that the client-port `GET /api/v1/sessions/{id}/events`
/// serves, but admin-authenticated instead of session-key-authenticated. This
/// is the only way to read a `closed`/`error` session's full history without
/// ever having held a session key. `404` if the session id does not exist.
pub async fn get_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    // Existence check first, so a nonexistent id is a 404 rather than an empty
    // event page (a closed/errored session is fully readable here — no session
    // key is involved; admin auth already gated the port).
    let session = state
        .store
        .with_conn(|c| sessions::get_session(c, &id))
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found(format!("no session {id}")))?;
    let (after, limit) = page.resolve()?;
    let (rows, has_more) = state
        .store
        .with_conn(|c| sessions::list_events(c, &session.id, after, limit))
        .map_err(ApiError::from_db)?;
    let last_rowid = rows.last().map(|(rid, _)| *rid);
    let items: Vec<Value> = rows.iter().map(|(_, e)| event_view(e)).collect();
    Ok(Json(json!({
        "items": items,
        "next_cursor": next_cursor(last_rowid, has_more),
    })))
}
