//! Error type and exit-code mapping.
//!
//! Per `aspec/uxui/cli.md`'s convention (shared with `baesrv`):
//! - exit `0` — success.
//! - exit `1` — runtime error (connection refused, an API error, an unexpected
//!   response shape).
//! - exit `2` — usage error (a malformed argument value we validate ourselves;
//!   clap already exits `2` for missing positionals / unknown flags).
//!
//! Diagnostics go to stderr; command results go to stdout. This type never
//! surfaces a raw `reqwest` error or a JSON backtrace to the user — transport
//! and decode failures are mapped to clean, actionable messages before they
//! reach here.

use std::fmt;

use serde::Deserialize;

/// A CLI error carrying its message and the process exit code it maps to.
#[derive(Debug)]
pub enum CliError {
    /// A runtime failure — exit `1`. The `String` is the full stderr message.
    Runtime(String),
    /// A usage/argument-validation failure — exit `2`.
    Usage(String),
}

impl CliError {
    /// Construct a runtime (exit `1`) error.
    pub fn runtime(msg: impl Into<String>) -> Self {
        CliError::Runtime(msg.into())
    }

    /// Construct a usage (exit `2`) error.
    pub fn usage(msg: impl Into<String>) -> Self {
        CliError::Usage(msg.into())
    }

    /// The process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Runtime(_) => 1,
            CliError::Usage(_) => 2,
        }
    }

    /// The stderr message for this error.
    pub fn message(&self) -> &str {
        match self {
            CliError::Runtime(m) | CliError::Usage(m) => m,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for CliError {}

/// An RFC 7807 problem document as returned by every non-2xx admin response.
///
/// `type` is a short, stable slug (`bad_request`, `not_found`, …) — we match on
/// it rather than on `status`/`title`, exactly as `docs/reference/02-admin-api.md`
/// instructs.
#[derive(Debug, Deserialize)]
pub struct ApiProblem {
    #[serde(default, rename = "type")]
    pub type_slug: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub status: u16,
}

impl ApiProblem {
    /// Map this problem document to a clean, actionable [`CliError`].
    ///
    /// Every admin API error is a runtime failure (exit `1`) — the request was
    /// well-formed enough to reach the server; it was the *server's* answer that
    /// failed. Each known `type` slug gets a tailored message per the work
    /// item's Edge Case Considerations; unknown slugs fall back to the raw
    /// `detail`.
    pub fn into_cli_error(self) -> CliError {
        let detail = if self.detail.trim().is_empty() {
            "the admin API returned an error with no detail".to_string()
        } else {
            self.detail.clone()
        };
        let msg = match self.type_slug.as_str() {
            "unauthorized" => UNAUTHORIZED_GUIDANCE.to_string(),
            "profile_in_use" => format!(
                "{detail}\n\
                 Revoke the profile's active keys first: run `baectl list keys` to \
                 find them, then `baectl delete key <id>` for each, and retry the delete."
            ),
            "profile_unavailable" => {
                format!("{detail} (the referenced profile does not exist or was deleted)")
            }
            // `bad_request`, `not_found`, `duplicate_name`, and any unknown slug
            // surface the API's own detail verbatim — it is already specific
            // (names the offending field / id / name).
            _ => detail,
        };
        CliError::runtime(msg)
    }
}

/// The message printed when the server enforces admin auth and baectl could not
/// resolve any usable token — lists all three ways to supply one.
pub const UNAUTHORIZED_GUIDANCE: &str = "\
admin API rejected the request: no valid admin token was supplied (401 unauthorized).
Provide an admin token in one of these ways (highest precedence first):
  1. --admin-token <token>   (or the BAE_ADMIN_TOKEN env var)
  2. --admin-key-file <path> (or the BAE_ADMIN_KEY_FILE env var)
  3. the default key file at /var/lib/bae/admin-key.pem, which baesrv writes on
     first boot — reachable automatically when baectl runs inside the same
     container as baesrv (e.g. `docker exec bae baectl …`).";

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an [`ApiProblem`] as the wire would (`type`/`detail`/`status`).
    fn problem(type_slug: &str, status: u16, detail: &str) -> ApiProblem {
        ApiProblem {
            type_slug: type_slug.to_string(),
            detail: detail.to_string(),
            status,
        }
    }

    #[test]
    fn each_error_slug_maps_to_message_and_exit_1() {
        // `bad_request` / `not_found` / `duplicate_name` surface the API detail
        // verbatim — it already names the offending field/id/name.
        for slug in ["bad_request", "not_found", "duplicate_name"] {
            let e = problem(slug, 400, "the specific server detail").into_cli_error();
            assert_eq!(e.exit_code(), 1, "{slug} is a runtime error");
            assert_eq!(e.message(), "the specific server detail", "{slug}");
        }

        // `profile_in_use` (409) → detail plus the revoke-keys suggestion.
        let e = problem("profile_in_use", 409, "profile has active client keys").into_cli_error();
        assert_eq!(e.exit_code(), 1);
        assert!(e.message().starts_with("profile has active client keys"));
        assert!(e.message().contains("baectl list keys"));
        assert!(e.message().contains("baectl delete key"));

        // `profile_unavailable` (422) → detail plus the parenthetical hint.
        let e = problem("profile_unavailable", 422, "no such profile").into_cli_error();
        assert_eq!(e.exit_code(), 1);
        assert!(e.message().starts_with("no such profile"));
        assert!(e.message().contains("does not exist or was deleted"));

        // `unauthorized` (401) → the full three-option guidance block.
        let e = problem("unauthorized", 401, "invalid admin key").into_cli_error();
        assert_eq!(e.exit_code(), 1);
        assert_eq!(e.message(), UNAUTHORIZED_GUIDANCE);

        // An unknown slug still surfaces the detail verbatim.
        let e = problem("internal", 500, "unexpected server error").into_cli_error();
        assert_eq!(e.message(), "unexpected server error");
    }

    #[test]
    fn parses_rfc7807_document_then_maps() {
        // The exact RFC 7807 shape from docs/reference/02-admin-api.md deserializes
        // into `ApiProblem` (matching on `type`, not `status`/`title`).
        let body = serde_json::json!({
            "type": "duplicate_name",
            "title": "Conflict",
            "status": 409,
            "detail": "profile name 'main' already taken",
        })
        .to_string();
        let parsed: ApiProblem = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.type_slug, "duplicate_name");
        assert_eq!(parsed.status, 409);
        let e = parsed.into_cli_error();
        assert_eq!(e.message(), "profile name 'main' already taken");
        assert_eq!(e.exit_code(), 1);
    }

    #[test]
    fn empty_detail_falls_back_to_a_generic_message() {
        let e = problem("bad_request", 400, "   ").into_cli_error();
        assert_eq!(
            e.message(),
            "the admin API returned an error with no detail"
        );
    }

    #[test]
    fn usage_and_runtime_exit_codes() {
        assert_eq!(CliError::usage("x").exit_code(), 2);
        assert_eq!(CliError::runtime("x").exit_code(), 1);
    }
}
