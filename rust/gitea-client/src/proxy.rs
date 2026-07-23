//! HTTP proxy configuration for outbound Gitea requests — stage 13.
//!
//! By default [`reqwest`] honours the `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`
//! / `NO_PROXY` environment variables. That is the right behaviour for
//! Jenkins controllers whose operators have configured proxy via the standard
//! env vars (e.g. in the systemd unit or container image).
//!
//! However, the Jenkins **global config UI** also exposes proxy settings,
//! and enterprise deployments often prefer that path because:
//!
//! 1. It survives container image rebuilds (the value lives in
//!    `$JENKINS_HOME/org.jenkinsci.plugin.gitea.servers.GiteaServers.xml`).
//! 2. It is the only way to inject credentials when the env vars would be
//!    visible in `/proc/<pid>/environ` to anyone with shell access.
//! 3. It lets admins pick a per-plugin proxy (different from Jenkins core's
//!    own `ProxyConfiguration`).
//!
//! So we mirror the design of [`crate::tls_store`]: a process-global
//! [`OnceLock`] holds an `Option<Arc<ProxyConfig>>`. When Java calls
//! `nativeSetProxy(String)` during `GiteaServers.configure()`, the JSON is
//! parsed and the resulting [`ProxyConfig`] is stashed. When a client is
//! built, [`apply_to_builder`] reads the slot and, if set, attaches a
//! [`reqwest::Proxy`] to the builder. If the slot is empty (no explicit
//! proxy), reqwest's default env-var lookup kicks in.
//!
//! Like the TLS slot, this is **write-once**: the first non-empty value
//! wins. Hot-reload is documented as unsupported (see `AGENTS.md`).

use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Explicit HTTP/HTTPS/SOCKS5 proxy configuration for outbound Gitea
/// requests.
///
/// Serialized as JSON across the JNI boundary. Field names are camelCase
/// to match the Java side's `buildProxyJson()` helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    /// Proxy URL, e.g. `"http://proxy.corp:3128"`, `"https://..."` or
    /// `"socks5://proxy:1080"`. Empty string means "no explicit proxy"
    /// (fall back to env vars).
    #[serde(default)]
    pub url: String,
    /// Optional Basic-auth username.
    #[serde(default)]
    pub username: String,
    /// Optional Basic-auth password.
    #[serde(default)]
    pub password: String,
    /// Comma-separated host patterns that bypass the proxy, e.g.
    /// `"localhost,127.0.0.1,.internal.corp.com"`. The leading-dot form
    /// matches the whole subdomain (mirrors cURL / Jenkins semantics).
    #[serde(default)]
    pub no_proxy_hosts: String,
}

impl ProxyConfig {
    /// A proxy config is "empty" when it has no URL. Used to distinguish
    /// "explicitly disabled" (`None`) from "explicitly empty" (some JSON
    /// with empty fields) — both are normalised to env-var fallback.
    pub fn is_empty(&self) -> bool {
        self.url.is_empty()
    }
}

/// Process-global proxy slot. `None` (or unset) means "let reqwest fall
/// back to env vars". `Some(Arc<ProxyConfig>)` is the explicit config that
/// will be applied to every outbound client built via
/// [`crate::tls::build_reqwest_client`].
static PROXY_CONFIG: OnceCell<Option<Arc<ProxyConfig>>> = OnceCell::new();

/// Install the proxy configuration.
///
/// Should be called exactly once from
/// `Java_…_RustGiteaConnection_nativeSetProxy` during plugin
/// initialisation. Subsequent calls are **no-ops** — `OnceLock`
/// semantics — so changing the proxy in `GiteaServers.configure()` and
/// saving Jenkins config will NOT take effect until the controller is
/// restarted. This matches the existing hot-reload limitation documented
/// in `AGENTS.md`.
///
/// Passing `None` (or a config whose [`ProxyConfig::is_empty`] is true)
/// keeps the env-var fallback behaviour — i.e. reqwest reads
/// `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` at request time.
pub fn set_proxy(config: Option<ProxyConfig>) {
    // Strip empty configs so the consumer can branch on `Some(non-empty)`.
    let normalized = config.filter(|c| !c.is_empty());
    // `set` returns `Err` if already initialised; idempotent re-install
    // during plugin reload is fine, so we ignore the result.
    let _ = PROXY_CONFIG.set(normalized.map(Arc::new));
}

/// Read the currently-installed explicit proxy configuration, if any.
///
/// Does NOT include the env-var fallback — callers that want "effective"
/// proxy should rely on [`apply_to_builder`] instead.
pub fn proxy_config() -> Option<Arc<ProxyConfig>> {
    PROXY_CONFIG.get().and_then(|opt| opt.clone())
}

/// Attach the proxy (if any) to a [`reqwest::ClientBuilder`].
///
/// * If no explicit [`ProxyConfig`] is set, the builder is returned
///   unchanged — reqwest then reads `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`
///   / `NO_PROXY` from the environment (its default behaviour).
/// * If an explicit config exists, [`reqwest::Proxy::all`] is attached.
///   Basic auth is added when `username` is non-empty. Each entry of
///   `no_proxy_hosts` is registered via [`reqwest::Proxy::no_proxy`].
/// * If the URL cannot be parsed (e.g. typo), the builder is returned
///   unchanged and a WARN is logged — never propagate the error, because
///   failing the client build would take down every API call.
pub fn apply_to_builder(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let Some(cfg) = proxy_config() else {
        return builder; // env-var fallback
    };
    if cfg.is_empty() {
        return builder;
    }
    match reqwest::Proxy::all(&cfg.url) {
        Ok(mut proxy) => {
            if !cfg.username.is_empty() {
                proxy = proxy.basic_auth(&cfg.username, &cfg.password);
            }
            // `reqwest::Proxy::no_proxy` takes a single `Option<NoProxy>`
            // built from a comma-separated host list. The `NoProxy::from_string`
            // helper accepts the same `"host1,host2,.domain"` syntax that
            // cURL / Jenkins use.
            if !cfg.no_proxy_hosts.is_empty() {
                proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&cfg.no_proxy_hosts));
            }
            builder.proxy(proxy)
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %cfg.url, "Invalid proxy URL, ignoring");
            builder
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_proxy_config_is_empty() {
        assert!(ProxyConfig {
            url: String::new(),
            username: String::new(),
            password: String::new(),
            no_proxy_hosts: String::new(),
        }
        .is_empty());
    }

    #[test]
    fn non_empty_url_is_not_empty() {
        assert!(!ProxyConfig {
            url: "http://proxy.corp:3128".into(),
            username: String::new(),
            password: String::new(),
            no_proxy_hosts: String::new(),
        }
        .is_empty());
    }

    #[test]
    fn set_proxy_none_does_not_panic() {
        // Idempotent: subsequent calls (including the no-op None case)
        // must not panic regardless of prior state.
        set_proxy(None);
    }

    #[test]
    fn apply_to_builder_without_explicit_proxy_returns_builder() {
        // When no explicit config is set (the common case in tests), the
        // builder is returned as-is and reqwest falls back to env vars.
        let builder = reqwest::Client::builder().use_rustls_tls();
        // Just exercise the function — building the client proves the
        // builder is still usable.
        let _client = apply_to_builder(builder).build().expect("client must build");
    }
}
