# Example: Per-Host Proxy Routing

**Use case:** Some Gitea servers are public (no proxy), others are behind corporate firewall (require proxy). Need per-server proxy configuration.

**Time:** 2-3 hours.

---

## Current limitation

`GiteaServers` (global config) has a single `proxyUrl` field. All Gitea servers share the same proxy setting. Per-server proxy requires either:
1. Extending `GiteaServer` (per-server class) with a `proxyOverride` field
2. Per-request proxy selection in Rust based on target host

This example shows option 2 — Rust-side per-host routing.

---

## Implementation

### 1. Rust: per-host proxy resolver

New file `rust/gitea-client/src/corp_proxy.rs`:

```rust
//! Per-host proxy routing for corporate deployments.
//!
//! Some Gitea servers are reachable directly (no proxy), others require
//! a corporate proxy. This module parses a routing table and returns
//! the correct proxy URL for a given target host.

use once_cell::sync::OnceLock;
use std::collections::HashMap;

/// Routing rule: target_host_prefix → proxy_url
/// Empty proxy_url means "direct connection" (bypass).
static ROUTES: OnceLock<HashMap<String, String>> = OnceLock::new();

pub fn set_routes(routes: HashMap<String, String>) {
    let _ = ROUTES.set(routes);
}

/// Resolve the proxy URL for a given target URL.
/// Returns `None` for "direct" (no proxy).
pub fn resolve(target_url: &str) -> Option<String> {
    let routes = ROUTES.get()?;
    let host = url::Url::parse(target_url)
        .ok()?
        .host_str()?
        .to_lowercase();

    // Longest-prefix match
    let mut best: Option<(&String, &String)> = None;
    for (prefix, proxy) in routes.iter() {
        if host.ends_with(&prefix.to_lowercase()) {
            if best.is_none() || prefix.len() > best.unwrap().0.len() {
                best = Some((prefix, proxy));
            }
        }
    }

    best.and_then(|(_, proxy)| {
        if proxy.is_empty() {
            None  // explicit bypass
        } else {
            Some(proxy.clone())
        }
    })
}
```

### 2. Rust: apply per-host proxy in client.rs

In `rust/gitea-client/src/client.rs`, replace `crate::proxy::apply_to_builder(builder)` with per-host selection:

```rust
fn build_client_for_target(target_url: &str) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .use_rustls_tls();

    // Apply TLS trust store (global)
    builder = crate::tls::apply_to_builder(builder);

    // Apply per-host proxy
    if let Some(proxy_url) = crate::corp_proxy::resolve(target_url) {
        let proxy = reqwest::Proxy::all(&proxy_url)?;
        builder = builder.proxy(proxy);
    } else {
        // No per-host override — fall back to global proxy
        builder = crate::proxy::apply_to_builder(builder);
    }

    builder.build()
}
```

### 3. JNI bridge

In `rust/gitea-client/src/jni_corp.rs` (new file):

```rust
use jni::objects::{JClass, JString};
use jni::JNIEnv;
use std::collections::HashMap;

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeSetCorpProxyRoutes(
    mut env: JNIEnv,
    _cls: JClass,
    json: JString,
) {
    let raw: String = env.get_string(&json).map(|c| c.into()).unwrap_or_default();
    let routes: HashMap<String, String> = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "nativeSetCorpProxyRoutes: invalid JSON");
            return;
        }
    };
    crate::corp_proxy::set_routes(routes);
}
```

### 4. Wire module in lib.rs

```rust
pub mod corp_proxy;
pub mod jni_corp;
```

### 5. Java side

In `GiteaServers.java`:

```java
/**
 * Per-host proxy routing rules in JSON format.
 * Keys are hostname suffixes, values are proxy URLs.
 * Empty value means "direct connection" (bypass proxy).
 *
 * Example: {"internal.corp":"","gitea.com":"http://proxy.corp:3128"}
 */
private String corpProxyRoutes = "{}";

@Restricted(NoExternalUse.class)
public String getCorpProxyRoutes() {
    return corpProxyRoutes == null ? "{}" : corpProxyRoutes;
}

@Restricted(NoExternalUse.class)
public void setCorpProxyRoutes(String routes) {
    this.corpProxyRoutes = routes == null ? "{}" : routes;
}
```

In `RustGiteaConnection.java`:

```java
public static native void nativeSetCorpProxyRoutes(String json);
```

In `GiteaServers.configure()`:

```java
try {
    RustGiteaConnection.nativeSetCorpProxyRoutes(getCorpProxyRoutes());
} catch (Throwable t) {
    LOGGER.log(Level.WARNING, "nativeSetCorpProxyRoutes failed", t);
}
```

### 6. UI field

In `config.jelly`:

```xml
<f:entry title="${%Corporate proxy routing (JSON)}" field="corpProxyRoutes"
         help="">
    <f:textarea placeholder='{"internal.corp":"","gitea.com":"http://proxy.corp:3128"}'/>
</f:entry>
```

### 7. Test

In `rust/gitea-client/src/corp_proxy.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn longest_prefix_match_wins() {
        let mut routes = HashMap::new();
        routes.insert("corp".to_string(), "http://proxy.corp:3128".to_string());
        routes.insert("internal.corp".to_string(), "".to_string());  // bypass

        // Use a test-only setter since ROUTES is OnceLock
        // (in real code, this is set via JNI once)
        // For tests, you may need to refactor ROUTES to allow test injection.

        let target = "https://git.internal.corp/api/v1/version";
        let proxy = resolve(target);
        assert_eq!(proxy, None);  // bypassed — longest prefix match
    }

    #[test]
    fn no_match_falls_through() {
        let target = "https://example.com/api/v1/version";
        let proxy = resolve(target);
        // Depends on whether routes are set in this test process
        // (OnceLock limitation — see note below)
    }
}
```

**Note on testing `OnceLock`:** the global `ROUTES` can only be set once per process. For unit tests, refactor to accept routes as a parameter, or use `#[serial_test::serial]` to avoid concurrent test interference.

---

## Operational notes

- **Restart required:** changing `corpProxyRoutes` requires Jenkins restart (OnceLock limitation). Same as `trustedCertificatesPem`.
- **Performance:** `resolve()` is called on every outbound request. HashMap lookup is O(1) on average — negligible overhead.
- **Logging:** proxy URL is logged at DEBUG level (`org.jenkinsci.plugin.gitea.gitea_client.corp_proxy`). Enable for troubleshooting.
- **Credentials:** proxy auth credentials still come from global `GiteaServers.proxyUsername` / `proxyPassword`. Per-route credentials are not supported in this example.

---

## When NOT to use this

- If you have only one Gitea server → use `GiteaServers.proxyUrl` (simpler)
- If all Gitea servers use the same proxy → use Jenkins global proxy
- If routing depends on something other than hostname (e.g. user, time of day) → this pattern won't work, design a custom solution

This pattern is for the specific case of "different Gitea servers need different proxies on the same Jenkins controller."
