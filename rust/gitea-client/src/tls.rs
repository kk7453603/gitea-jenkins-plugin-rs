//! Custom TLS root certificate loading — stage 12.
//!
//! By default the Rust client (via `reqwest`'s `rustls-tls` feature) trusts
//! only the Mozilla CA bundle shipped as `webpki-roots`. That works for any
//! Gitea instance with a public-CA certificate, but production deployments
//! behind a corporate CA or with self-signed certs need a way to add extra
//! trust material.
//!
//! This module provides two complementary entry points:
//!
//! * [`build_reqwest_client`] — the primary API. `reqwest` already knows how
//!   to ingest PEM via [`reqwest::Certificate::from_pem`], so we keep the
//!   custom rustls plumbing out of the hot path. The Mozilla bundle is
//!   always trusted; the optional `extra_pem` adds to it (it does not
//!   replace it).
//!
//! * [`build_client_config`] — a lower-level helper that returns a raw
//!   [`rustls::ClientConfig`]. Useful for callers (or future code) that
//!   want to build the reqwest client from a pre-assembled rustls config.
//!
//! PEM storage for the global trust material lives in [`tls_store`]; see its
//! docs for why the design uses an `OnceLock<Option<Arc<Vec<u8>>>>`.

use rustls::{ClientConfig, RootCertStore};
use std::io::Cursor;
use std::sync::Arc;

/// Errors that can occur while loading custom trust material.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// I/O failure while reading PEM bytes (e.g. from a `Cursor`).
    #[error("failed to read PEM: {0}")]
    Io(#[from] std::io::Error),
    /// PEM input contained no parseable certificates.
    #[error("no certificates found in PEM input")]
    NoCertificates,
    /// Underlying rustls configuration error.
    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Build a [`rustls::ClientConfig`] that trusts:
/// 1. `webpki-roots` (Mozilla CA bundle) — always, the default.
/// 2. additional PEM bytes (if provided) — for self-signed Gitea or
///    corporate CA. The PEM may contain any number of certificates.
///
/// The resulting config uses no client auth (Gitea token auth is handled at
/// the HTTP layer, not via mutual TLS).
///
/// This helper is currently unused on the main request path (we go through
/// `reqwest::Certificate::from_pem` in [`build_reqwest_client`] instead),
/// but is kept public so that future code that needs a raw rustls config
/// (e.g. an `axum` HTTPS client) can reuse the same root store assembly.
pub fn build_client_config(extra_pem: Option<&[u8]>) -> Result<ClientConfig, TlsError> {
    let mut root_store = RootCertStore::empty();

    // 1. Mozilla CA bundle.
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // 2. Extra PEM (optional).
    if let Some(pem) = extra_pem {
        add_pem_to_store(&mut root_store, pem)?;
    }

    // rustls 0.23 takes the root store as `impl Into<Arc<RootCertStore>>`.
    // We wrap it in `Arc` ourselves so the resulting `ClientConfig` shares
    // one allocation for the roots across all clones.
    let config = ClientConfig::builder()
        .with_root_certificates(Arc::new(root_store))
        .with_no_client_auth();
    Ok(config)
}

/// Parse a PEM blob and append every certificate it contains to `store`.
/// Certificates that cannot be parsed are counted and logged at WARN level;
/// the function succeeds as long as at least one certificate was extracted
/// (matches the lenient behaviour of `openssl s_client` and most Java
/// trust-store loaders).
fn add_pem_to_store(store: &mut RootCertStore, pem: &[u8]) -> Result<(), TlsError> {
    let mut reader = Cursor::new(pem);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader).collect::<Result<_, _>>()?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificates);
    }
    let (_, ignored) = store.add_parsable_certificates(certs);
    if ignored > 0 {
        tracing::warn!(ignored, "some PEM certificates could not be parsed");
    }
    Ok(())
}

/// Build a [`reqwest::Client`] with custom TLS config.
///
/// `extra_pem` = additional CA certificates in PEM format (optional). When
/// `None` (or empty), the client trusts only the Mozilla CA bundle baked
/// into `webpki-roots` — i.e. identical to the pre-stage-12 behaviour.
///
/// The timeout is set to [`crate::client::DEFAULT_TIMEOUT`] via the
/// builder so that callers don't have to repeat it.
pub fn build_reqwest_client(extra_pem: Option<&[u8]>) -> Result<reqwest::Client, reqwest::Error> {
    use std::time::Duration;

    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

    let mut builder = reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .use_rustls_tls();

    if let Some(pem) = extra_pem {
        if !pem.is_empty() {
            // reqwest's built-in support: parse PEM and register every
            // certificate inside as an additional root. Simpler and more
            // battle-tested than re-assembling a rustls ClientConfig by
            // hand.
            let cert = reqwest::Certificate::from_pem(pem).map_err(|e| {
                tracing::error!(error = %e, "build_reqwest_client: failed to parse PEM");
                e
            })?;
            builder = builder.add_root_certificate(cert);
        }
    }

    // Stage 13 — attach the process-global proxy if one has been set via
    // JNI. If no explicit proxy is configured, `apply_to_builder` is a
    // no-op and reqwest falls back to HTTP_PROXY/HTTPS_PROXY/NO_PROXY env
    // vars (its default behaviour).
    builder = crate::proxy::apply_to_builder(builder);

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_reqwest_client_with_no_pem_works() {
        // Smoke test: a None PEM must produce a usable client (this is the
        // pre-stage-12 code path, used by 99% of deployments). We only
        // assert that the builder returns Ok — reqwest::Client has no
        // public inspector for the configured trust store.
        let _client = build_reqwest_client(None).expect("client without PEM must build");
    }

    #[test]
    fn empty_pem_is_ignored() {
        // Defensive: an empty byte slice must behave like `None`. We don't
        // want a misconfigured plugin to bring down every API call with an
        // "InvalidData" PEM error.
        let _client = build_reqwest_client(Some(b"")).expect("empty PEM must be ignored");
    }

    #[test]
    fn garbage_pem_is_rejected() {
        // reqwest::Certificate::from_pem returns Err on non-PEM input. We
        // propagate it so the operator sees a clear failure rather than a
        // silently-reduced trust store.
        let result = build_reqwest_client(Some(b"not a certificate"));
        assert!(result.is_err(), "garbage PEM must error");
    }

    #[test]
    fn build_client_config_with_no_pem_succeeds() {
        // Lower-level helper must also accept None.
        let config = build_client_config(None).expect("client config without PEM must build");
        // A ClientConfig has no public inspector for "number of roots", so
        // we just confirm the type was assembled without panicking by
        // touching a stable Debug field.
        let _ = format!("{:?}", config.max_fragment_size);
    }
}
