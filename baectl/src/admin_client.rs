//! A small `reqwest`-based wrapper over the admin API (`/admin/v1/*`).
//!
//! Typed request bodies mirror the JSON shapes documented in
//! `docs/reference/admin-api.md` and implemented in
//! `server/src/api/admin/{profiles,keys}.rs`. Responses are returned as
//! `serde_json::Value` so `--json` can echo the exact document the server sent
//! while human rendering ([`crate::output`]) reads the fields it needs — this
//! also keeps a forward-compatible response shape from panicking (see
//! `unexpected_response`).
//!
//! Uses the blocking `reqwest` client: baectl is a straight-line synchronous
//! program with no need for an async runtime.

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{ApiProblem, CliError};

/// `POST`/`PUT /admin/v1/profiles` body — field-for-field the documented shape.
///
/// `primary_provider`/`fallback_providers` are `bae-config.toml` `[providers]`
/// registry **name references**, not inline provider config objects — the
/// same opt-in-by-name model `mcp_servers` already uses. baectl does not
/// build or send provider config (URL, auth token, max tokens); that lives
/// entirely in the operator-managed `bae-config.toml` on the server.
#[derive(Debug, Serialize)]
pub struct ProfileBody {
    pub name: String,
    pub primary_provider: String,
    pub fallback_providers: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub allowed_tools: Vec<String>,
}

/// `POST /admin/v1/keys` body.
#[derive(Debug, Serialize)]
pub struct KeyBody {
    pub name: String,
    pub profile_id: String,
}

/// One page of a cursor-paginated list endpoint.
#[derive(Debug, serde::Deserialize)]
pub struct Page {
    #[serde(default)]
    pub items: Vec<Value>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// HTTP wrapper over the admin API, carrying the resolved base URL and token.
pub struct AdminClient {
    /// Base URL, e.g. `http://127.0.0.1:8081`.
    base: String,
    /// The `host:port` (or URL) as the user supplied it, for error messages.
    addr_display: String,
    /// Bearer token, if one was resolved (see [`crate::cli`] auth precedence).
    token: Option<String>,
    http: reqwest::blocking::Client,
}

impl AdminClient {
    /// Build a client for `addr` (a `host:port` or a full URL) and optional token.
    pub fn new(addr: &str, token: Option<String>) -> Self {
        // Accept a bare `host:port` (the common case) or a full URL (for an SSH
        // tunnel that terminates TLS). Plain `host:port` gets `http://` — the
        // admin port speaks plain HTTP on loopback.
        let base = if addr.contains("://") {
            addr.trim_end_matches('/').to_string()
        } else {
            format!("http://{addr}")
        };
        AdminClient {
            base,
            addr_display: addr.to_string(),
            token,
            http: reqwest::blocking::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Attach the bearer token (if any) to a request builder.
    fn authed(&self, rb: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    /// Send a request and decode a JSON body on success, or map the error.
    ///
    /// - Connection failures → a clean "could not connect …" runtime error.
    /// - Non-2xx → the RFC 7807 body mapped by [`ApiProblem::into_cli_error`].
    /// - 2xx with a body that will not parse → an "unexpected response" error
    ///   (version skew), never a raw parse panic.
    fn send(&self, rb: reqwest::blocking::RequestBuilder) -> Result<Option<Value>, CliError> {
        let resp = self.authed(rb).send().map_err(|e| self.transport_err(e))?;
        let status = resp.status();
        if status.is_success() {
            if status == reqwest::StatusCode::NO_CONTENT {
                return Ok(None);
            }
            let body = resp.text().map_err(|e| self.transport_err(e))?;
            if body.trim().is_empty() {
                return Ok(None);
            }
            let value: Value =
                serde_json::from_str(&body).map_err(|_| Self::unexpected_response())?;
            Ok(Some(value))
        } else {
            // Try to parse the RFC 7807 problem document; if the error body is
            // not the expected shape, still surface a clean runtime error.
            let body = resp.text().unwrap_or_default();
            match serde_json::from_str::<ApiProblem>(&body) {
                Ok(problem) => Err(problem.into_cli_error()),
                Err(_) => Err(CliError::runtime(format!(
                    "admin API returned HTTP {} with an unrecognized body",
                    status.as_u16()
                ))),
            }
        }
    }

    /// Map a transport error (connection refused, DNS, timeout) to a clean
    /// message — never a raw `reqwest` error/backtrace.
    fn transport_err(&self, _e: reqwest::Error) -> CliError {
        CliError::runtime(format!(
            "could not connect to admin API at {} — is baesrv running and is \
             --admin-addr correct?",
            self.addr_display
        ))
    }

    fn unexpected_response() -> CliError {
        CliError::runtime(
            "unexpected response from admin API — check that baectl and the server \
             are the same version",
        )
    }

    // --- Profiles ---------------------------------------------------------

    /// `POST /admin/v1/profiles` → the created profile summary.
    pub fn create_profile(&self, body: &ProfileBody) -> Result<Value, CliError> {
        let rb = self.http.post(self.url("/admin/v1/profiles")).json(body);
        self.send(rb)?.ok_or_else(Self::unexpected_response)
    }

    /// `GET /admin/v1/profiles/{id}` → the full profile.
    pub fn get_profile(&self, id: &str) -> Result<Value, CliError> {
        let rb = self.http.get(self.url(&format!("/admin/v1/profiles/{id}")));
        self.send(rb)?.ok_or_else(Self::unexpected_response)
    }

    /// `PUT /admin/v1/profiles/{id}` → the replaced profile.
    pub fn replace_profile(&self, id: &str, body: &ProfileBody) -> Result<Value, CliError> {
        let rb = self
            .http
            .put(self.url(&format!("/admin/v1/profiles/{id}")))
            .json(body);
        self.send(rb)?.ok_or_else(Self::unexpected_response)
    }

    /// `DELETE /admin/v1/profiles/{id}` (204).
    pub fn delete_profile(&self, id: &str) -> Result<(), CliError> {
        let rb = self
            .http
            .delete(self.url(&format!("/admin/v1/profiles/{id}")));
        self.send(rb)?;
        Ok(())
    }

    // --- Keys -------------------------------------------------------------

    /// `POST /admin/v1/keys` → the created key (plaintext shown once).
    pub fn create_key(&self, body: &KeyBody) -> Result<Value, CliError> {
        let rb = self.http.post(self.url("/admin/v1/keys")).json(body);
        self.send(rb)?.ok_or_else(Self::unexpected_response)
    }

    /// `DELETE /admin/v1/keys/{id}` (204).
    pub fn delete_key(&self, id: &str) -> Result<(), CliError> {
        let rb = self.http.delete(self.url(&format!("/admin/v1/keys/{id}")));
        self.send(rb)?;
        Ok(())
    }

    // --- Pagination -------------------------------------------------------

    /// Fetch one page from a list endpoint (`/admin/v1/profiles` or `/keys`).
    pub fn list_page(
        &self,
        path: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Page, CliError> {
        let mut rb = self.http.get(self.url(path));
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(c) = cursor {
            query.push(("cursor", c.to_string()));
        }
        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }
        if !query.is_empty() {
            rb = rb.query(&query);
        }
        let value = self.send(rb)?.ok_or_else(Self::unexpected_response)?;
        serde_json::from_value(value).map_err(|_| Self::unexpected_response())
    }

    /// Auto-paginate a list endpoint: follow `next_cursor` until it is `null`
    /// and return every item. A human running `baectl list …` should not need
    /// to know the API is cursor-paginated.
    pub fn list_all(&self, path: &str) -> Result<Vec<Value>, CliError> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.list_page(path, cursor.as_deref(), None)?;
            items.extend(page.items);
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(items)
    }
}

/// Build the raw single-page JSON document for `--json` list output.
pub fn page_document(page: &Page) -> Value {
    json!({
        "items": page.items,
        "next_cursor": page.next_cursor,
    })
}
