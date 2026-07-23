//! Global lazy statics shared by all [`crate::client::GiteaClient`] instances.
//!
//! The upstream Java code creates a fresh `HttpURLConnection` per request and
//! relies on Jenkins' proxy configuration. In Rust we use a single `reqwest`
//! client per server (connection-pooled) and a shared `tokio` runtime that
//! the JNI layer will `block_on` from synchronous Java methods.

use once_cell::sync::Lazy;
use tokio::runtime::Runtime;

/// Shared multi-threaded Tokio runtime.
///
/// Constructed lazily on first access (via `once_cell`) and lives for the
/// lifetime of the JVM. The JNI shim uses `RT.block_on(...)` to drive async
/// methods from synchronous Java entry points.
///
/// Note: this design does NOT support Jenkins plugin hot-reload — see
/// `IMPLEMENTATION_PLAN.md` §"Риски".
pub static RT: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create Tokio runtime for gitea-client")
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_is_usable() {
        // Smoke test: ensure the lazy initializes and can run a future.
        let result = RT.block_on(async { 42 });
        assert_eq!(result, 42);
    }
}
