//! Admin sandbox-status endpoint (`/admin/v1/sandbox-status`).
//!
//! Read-only, mirroring `/admin/v1/mcp-servers`' no-secrets posture: lets an
//! operator confirm sandbox image-pull state without grepping logs. The data
//! comes from the in-memory `AppState.sandbox_status` map the background
//! provisioning task maintains (rebuilt from `pending` at every restart).
//!
//! The response is **scoped per profile** — one item per profile, each
//! carrying only its own declared images — the same scoping the
//! `session.sandbox.available` notification enforces, so this endpoint cannot
//! be mistaken for a cross-profile image directory either.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::api::AppState;

/// `GET /admin/v1/sandbox-status`
///
/// `{"items": [{"profile_id", "images": [{"name", "status", "detail"?}]}]}`,
/// sorted by profile id then image name for stable output. `detail` is
/// present only on `error` entries. Empty when no profile declares
/// `available_sandboxes`.
pub async fn list(State(state): State<AppState>) -> Json<Value> {
    // Snapshot the JSON view under the lock (the map is never held across an
    // await), sorting inside each profile and across profiles for stability.
    let mut items: Vec<(String, Value)> = {
        let map = state
            .sandbox_status
            .lock()
            .expect("sandbox_status mutex poisoned");
        map.iter()
            .map(|(profile_id, images)| {
                let mut names: Vec<&String> = images.keys().collect();
                names.sort();
                let images: Vec<Value> = names
                    .into_iter()
                    .map(|name| {
                        let status = &images[name];
                        let mut entry = json!({ "name": name, "status": status.as_str() });
                        if let Some(d) = status.detail() {
                            entry["detail"] = json!(d);
                        }
                        entry
                    })
                    .collect();
                (
                    profile_id.clone(),
                    json!({ "profile_id": profile_id, "images": images }),
                )
            })
            .collect()
    };
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let items: Vec<Value> = items.into_iter().map(|(_, v)| v).collect();
    Json(json!({ "items": items }))
}
