//! Native Rust HTTP client for the Gitea API.
//!
//! Native Rust HTTP client for the Gitea API.
//!
//! Stage 1 of the Jenkins Gitea plugin rewrite: a pure-Rust client crate
//! that can be built and tested independently of JNI / Jenkins. Stage 2
//! added `#[no_mangle]` JNI exports on top of [`client::GiteaClient`].
//! Stage 9.A added the webhook receiver layer (see [`server`] and
//! [`jni_webhook`]). Stage 12 added custom TLS trust material support
//! (see [`tls`] and [`tls_store`]). Stage 10 added the adaptive polling
//! scheduler (see [`polling`] and [`jni_polling`]).
//!
//! All public methods of [`client::GiteaClient`] return raw JSON strings
//! (not typed POJOs). The Java shim parses them with the existing Jackson
//! `ObjectMapper`. The webhook layer follows the same convention — the
//! raw event JSON is forwarded to Java untouched.

pub mod auth;
pub mod client;
pub mod error;
pub mod events;
pub mod jni;
pub mod jni_polling;
pub mod jni_webhook;
pub mod polling;
pub mod pool;
pub mod proxy;
pub mod rate_limiter;
pub mod runtime;
pub mod server;
pub mod tls;
pub mod tls_store;

pub use auth::Auth;
pub use client::GiteaClient;
pub use error::GiteaError;
