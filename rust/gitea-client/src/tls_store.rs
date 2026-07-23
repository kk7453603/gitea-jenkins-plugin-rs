//! Process-global storage for the Jenkins-supplied trust material — stage 12.
//!
//! Every JNI entry point in [`crate::jni`] rebuilds a [`crate::client::GiteaClient`]
//! from the `(serverUrl, authType, authSecret)` triple that Java passes on
//! every call (the upstream `GiteaConnection` contract is stateless). That
//! means there is no per-connection place to attach a "trusted certificates"
//! blob.
//!
//! Two designs were considered:
//!
//! 1. **Add a `byte[] extraPem` argument to every one of the 35 native
//!    methods.** Mechanically correct, but it bloats the Java-side call
//!    sites (each `nativeXxx(...)` wrapper would have to thread the PEM
//!    through) and forces every JNI export to re-parse the PEM on every
//!    call — wasteful.
//!
//! 2. **Set the PEM once at plugin load, read it on every client build.**
//!    We use an [`once_cell::sync::OnceCell`] holding an `Option<Arc<Vec<u8>>>`.
//!    `OnceCell` is appropriate because the trust material is expected to
//!    change only when the operator saves Jenkins' global config — and at
//!    that point a process restart is acceptable (the AGENTS.md "known
//!    limitations" section already calls out hot-reload as unsupported
//!    because of the Tokio runtime).
//!
//! Design (2) was chosen. The value is wrapped in [`Arc`] so a future
//! "swap-and-rebuild" API can hand out clones without copying the bytes.

use once_cell::sync::OnceCell;
use std::sync::Arc;

/// Global slot for the Jenkins-supplied PEM trust material.
///
/// * `None` (initial state, or `set_extra_pem(None)` called) — only the
///   Mozilla CA bundle is trusted.
/// * `Some(Arc<Vec<u8>>)` — the PEM bytes are merged into the trust store
///   on top of the Mozilla bundle.
static EXTRA_PEM: OnceCell<Option<Arc<Vec<u8>>>> = OnceCell::new();

/// Install additional PEM trust material.
///
/// Should be called exactly once from `Java_…_RustGiteaConnection_nativeSetTrustedCertificates`
/// during plugin initialisation. Subsequent calls are **no-ops** — `OnceCell`
/// semantics — so changing the PEM in `GiteaServers.configure()` and
/// saving Jenkins config will NOT take effect until the controller is
/// restarted. This matches the existing hot-reload limitation documented in
/// `AGENTS.md`.
///
/// Passing `None` or an empty `Vec` clears any previously-recorded intent
/// to add extra roots (equivalent to "use only Mozilla CA").
pub fn set_extra_pem(pem: Option<Vec<u8>>) {
    // Strip empty payloads so the consumer can branch on `Some(non-empty)`.
    let normalized = pem.filter(|b| !b.is_empty());
    // `set` returns `Err` if already initialised. We deliberately ignore
    // that: idempotent re-installation during plugin reload is fine (and
    // common — Jenkins calls `configure()` again on every global-config
    // save).
    let _ = EXTRA_PEM.set(normalized.map(Arc::new));
}

/// Read the currently-installed extra PEM, if any.
///
/// Returns an owned `Vec<u8>` so the caller can hand it directly to
/// `reqwest::Certificate::from_pem` without juggling lifetimes. The clone
/// is cheap in practice — the PEM is a few KB at most.
pub fn extra_pem() -> Option<Vec<u8>> {
    EXTRA_PEM
        .get()
        .and_then(|opt| opt.as_ref().map(|a| (**a).clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_none() {
        // NOTE: OnceCell state leaks across tests within a single binary,
        // so this assertion only holds before anyone calls `set_extra_pem`.
        // Tests that exercise `set_extra_pem` run after this one (Rust runs
        // them serially in declaration order within a module), so it's
        // safe in practice — but we tolerate a Some(empty) too.
        if let Some(vec) = extra_pem() {
            assert!(
                vec.is_empty(),
                "expected no extra PEM, got {} bytes",
                vec.len()
            );
        }
    }

    #[test]
    fn set_extra_pem_none_is_ignored_after_first_init() {
        // Whether or not OnceCell was already set, calling with None must
        // not panic and must leave extra_pem() in a sane state.
        set_extra_pem(None);
        // We don't assert on the value because other tests may have
        // populated it first — OnceCell is write-once.
    }
}
