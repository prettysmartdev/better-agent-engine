//! Connection configuration for the harness.

/// Everything the harness needs to reach a BAE server and authenticate the
/// initial session-open call.
///
/// The `client_key` is the plaintext `bae_…` bearer token minted by an
/// operator via the admin API. It is treated as an opaque secret — the SDK
/// never parses or validates its shape.
#[derive(Clone, Debug)]
pub struct Config {
    /// Base URL of the BAE **client** port, e.g. `http://localhost:8080`.
    /// Trailing slashes are trimmed when building request URLs.
    pub server_url: String,
    /// Plaintext client key (`bae_…`) used as the bearer token when opening a
    /// session. Opaque — passed through verbatim.
    pub client_key: String,
    /// Version string this client advertises at session open. Recorded on the
    /// `session.open` event server-side; purely informational.
    pub client_version: String,
}

impl Config {
    /// Construct a config from the required fields, defaulting `client_version`
    /// to this crate's version.
    pub fn new(server_url: impl Into<String>, client_key: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            client_key: client_key.into(),
            client_version: crate::VERSION.to_string(),
        }
    }

    /// Override the advertised client version.
    pub fn with_client_version(mut self, version: impl Into<String>) -> Self {
        self.client_version = version.into();
        self
    }

    /// The server URL with any trailing slashes removed, ready for path joins.
    pub(crate) fn base(&self) -> &str {
        self.server_url.trim_end_matches('/')
    }
}
