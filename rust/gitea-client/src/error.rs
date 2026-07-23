//! Error type for the Gitea HTTP client.
//!
//! Maps onto the Java exception hierarchy:
//! - [`GiteaError::HttpStatus`] → `org.jenkinsci.plugin.gitea.client.api.GiteaHttpStatusException`
//! - [`GiteaError::FileNotFound`] → `java.io.FileNotFoundException`
//! - [`GiteaError::Network`] / [`GiteaError::Io`] → `java.io.IOException`

use thiserror::Error;

/// All errors produced by [`crate::client::GiteaClient`].
#[derive(Debug, Error)]
pub enum GiteaError {
    /// A non-2xx HTTP response. Mirrors `GiteaHttpStatusException`.
    ///
    /// `status` is the HTTP status code; `message` is the reason phrase;
    /// `body` is the optional request body that triggered the failure
    /// (for POST/PATCH).
    #[error("HTTP {status}: {message}")]
    HttpStatus {
        /// HTTP status code.
        status: u16,
        /// HTTP reason phrase.
        message: String,
        /// Optional request body, for diagnostics on POST/PATCH failures.
        body: Option<String>,
    },

    /// A 404 from `fetch_file`. Mirrors the `FileNotFoundException` thrown by
    /// `DefaultGiteaConnection#fetchFile`. Carries the file path for diagnostics.
    #[error("file not found: {0}")]
    FileNotFound(String),

    /// Underlying transport / connection error from `reqwest`.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// URL construction / parsing failure.
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),

    /// JSON (de)serialization failure. Mostly used internally for pagination
    /// merging; public methods usually return raw strings.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// IO failure (e.g. reading a body stream into a buffer).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl GiteaError {
    /// Returns the HTTP status code if this is an `HttpStatus` or `FileNotFound`
    /// (404) variant. Used by the Java shim to populate
    /// `GiteaHttpStatusException#getStatusCode`.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            GiteaError::HttpStatus { status, .. } => Some(*status),
            GiteaError::FileNotFound(_) => Some(404),
            _ => None,
        }
    }
}
