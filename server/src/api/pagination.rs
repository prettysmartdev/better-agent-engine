//! Cursor pagination shared by every list endpoint.
//!
//! List endpoints accept `?cursor=&limit=` and return `{items, next_cursor}`
//! (per `aspec/architecture/apis.md`). The cursor is opaque to clients: it is
//! the rowid of the last row of the previous page. Ordering by rowid gives a
//! stable, insertion-order page sequence without needing a separate sort key.

use serde::Deserialize;

use super::error::ApiError;

/// Default page size when `limit` is omitted.
pub const DEFAULT_LIMIT: i64 = 50;
/// Hard cap on page size so a client cannot request an unbounded scan.
pub const MAX_LIMIT: i64 = 200;

/// Raw `?cursor=&limit=` query parameters.
#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

impl PageQuery {
    /// Resolve to `(after_rowid, limit)`. An absent cursor starts at rowid 0
    /// (exclusive), so the first page begins at the first row. A malformed
    /// cursor or a non-positive/oversized limit is a 400.
    pub fn resolve(&self) -> Result<(i64, i64), ApiError> {
        let after = match &self.cursor {
            None => 0,
            Some(c) if c.is_empty() => 0,
            Some(c) => c
                .parse::<i64>()
                .map_err(|_| ApiError::bad_request(format!("invalid cursor: {c:?}")))?,
        };
        let limit = match self.limit {
            None => DEFAULT_LIMIT,
            Some(n) if n <= 0 => {
                return Err(ApiError::bad_request("limit must be a positive integer"))
            }
            Some(n) => n.min(MAX_LIMIT),
        };
        Ok((after, limit))
    }
}

/// Build the `next_cursor` value: the last rowid of this page when more rows
/// remain, otherwise `None` (serialized as JSON `null`).
pub fn next_cursor(last_rowid: Option<i64>, has_more: bool) -> Option<String> {
    match (has_more, last_rowid) {
        (true, Some(id)) => Some(id.to_string()),
        _ => None,
    }
}
