//! Connection pool — reuse `reqwest::Client` across `GiteaClient` instances
//! (stage 9.A enhancement, issue #8).
//!
//! ## Why
//!
//! Every [`crate::client::GiteaClient::new`] call used to build a fresh
//! `reqwest::Client` via [`crate::tls::build_reqwest_client`]. Each build
//! assembles a brand-new TLS connector (rustls `ClientConfig` + proxy
//! resolver + connection pool) and pays for it on the first request via a
//! full TLS handshake. The upstream Java code is stateless at the
//! `GiteaConnection` level, so JNI calls reach `GiteaClient::new` on
//! essentially every method invocation — making the per-call rebuild cost
//! the dominant overhead in benchmark traces.
//!
//! ## Design
//!
//! A single process-wide `HashMap<String, PoolEntry>` is keyed by a stable
//! signature derived from `(base_url, auth)`:
//!
//! - `base_url` — so different Gitea instances get separate pools.
//! - `auth` discriminant + secret material — so different credentials do
//!   not accidentally share a client. The pool only caches the TCP/TLS
//!   connection and the proxy/TLS config; auth headers are still applied
//!   per-request by [`crate::auth::Auth::apply`], so two callers with the
//!   same `(base_url, auth)` signature can safely share the same
//!   `reqwest::Client`.
//!
//! The PEM trust material is NOT part of the key. It is process-global
//! (see [`crate::tls_store`]) and identical for every client in the pool,
//! so all entries share the same TLS roots implicitly.
//!
//! ## Eviction
//!
//! Two policies:
//!
//! 1. **TTL** — entries idle for longer than [`POOL_TTL`] (5 min) are
//!    eligible for eviction, swept either opportunistically on insert
//!    when the pool is full, or by a periodic background call to
//!    [`evict_stale`].
//! 2. **LRU on overflow** — when the pool reaches [`POOL_MAX`] (32
//!    entries) and no stale entries can be evicted, the oldest entry by
//!    `last_used` is dropped.
//!
//! Both policies are conservative: 32 distinct `(base_url, auth)` tuples
//! is well beyond any realistic Jenkins deployment (typical: 1-5 Gitea
//! servers, 1-3 token scopes each).

use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Idle TTL — entries not used for this long become eligible for eviction.
const POOL_TTL: Duration = Duration::from_secs(300); // 5 min

/// Hard cap on the number of cached clients. Bounding memory growth in
/// pathological cases (e.g. a misconfigured plugin iterating over many
/// distinct base URLs).
const POOL_MAX: usize = 32;

/// A single pooled client plus its last-use timestamp.
struct PoolEntry {
    /// The cached `reqwest::Client`. Cloning is cheap — internally it is
    /// an `Arc` over the connection pool and TLS config.
    client: reqwest::Client,
    /// Updated on every [`acquire`] hit and on insert.
    last_used: Instant,
}

/// Process-wide pool. `OnceCell` so we don't have to thread a reference
/// through every JNI export; the first call lazily initialises an empty
/// `HashMap`.
static POOL: OnceCell<Mutex<HashMap<String, PoolEntry>>> = OnceCell::new();

/// Borrow the global pool, initialising it on first use.
fn pool() -> &'static Mutex<HashMap<String, PoolEntry>> {
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Build a stable signature for `(base_url, auth)`.
///
/// We deliberately use [`std::collections::hash_map::DefaultHasher`]
/// (SipHash-1-3, seeded per-process by the std runtime) — collision
/// resistance is more than sufficient for a process-local cache, and we
/// avoid pulling in a `Sha2`/`blake3` dependency for a non-security
/// use-case. The raw secret is hashed into the signature so that
/// `tracing`/debug output of the key never leaks the token itself.
fn key_for(base_url: &str, auth: &crate::auth::Auth) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    base_url.hash(&mut h);
    std::mem::discriminant(auth).hash(&mut h);
    match auth {
        crate::auth::Auth::None => {}
        crate::auth::Auth::Token(t) => {
            t.hash(&mut h);
        }
        crate::auth::Auth::Basic { user, pass } => {
            user.hash(&mut h);
            pass.hash(&mut h);
        }
    }
    format!("{:x}", h.finish())
}

/// Acquire a pooled `reqwest::Client`.
///
/// On a cache hit the entry's `last_used` timestamp is refreshed and the
/// client is cloned out (cheap). On a miss a new client is built via
/// [`crate::tls::build_reqwest_client`] (which picks up the global extra
/// PEM and proxy settings) and inserted into the pool.
///
/// The caller must still apply auth headers per-request — the pool only
/// caches the TCP/TLS connection and the proxy/TLS config, not the auth
/// state.
pub fn acquire(
    base_url: &str,
    auth: &crate::auth::Auth,
) -> Result<reqwest::Client, crate::error::GiteaError> {
    let key = key_for(base_url, auth);
    let mut guard = pool().lock().unwrap();

    // 1. Cache hit — refresh timestamp and return a cheap clone.
    if let Some(entry) = guard.get_mut(&key) {
        entry.last_used = Instant::now();
        return Ok(entry.client.clone());
    }

    // 2. Cache miss. If the pool is full, evict stale entries first
    //    (TTL); if still full, drop the oldest by last_used (LRU).
    if guard.len() >= POOL_MAX {
        guard.retain(|_, e| e.last_used.elapsed() < POOL_TTL);
        if guard.len() >= POOL_MAX {
            if let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&oldest_key);
            }
        }
    }

    // 3. Build a fresh client using the process-global PEM trust
    //    material. We intentionally do NOT pass `extra_pem` from the
    //    caller — see the module docs for why the PEM is not part of the
    //    cache key.
    let pem = crate::tls_store::extra_pem();
    let client = crate::tls::build_reqwest_client(pem.as_deref())
        .map_err(crate::error::GiteaError::Network)?;
    guard.insert(
        key,
        PoolEntry {
            client: client.clone(),
            last_used: Instant::now(),
        },
    );
    Ok(client)
}

/// Sweep the pool for stale entries. Intended to be called from a tokio
/// task on a fixed cadence (e.g. once per minute) alongside the existing
/// rate-limiter cleanup loop in [`crate::server`]).
///
/// Failures to lock the pool (e.g. a poisoned mutex) are logged at WARN
/// and otherwise ignored — a transient lock failure must not crash the
/// cleanup task.
pub fn evict_stale() {
    match pool().lock() {
        Ok(mut guard) => {
            guard.retain(|_, e| e.last_used.elapsed() < POOL_TTL);
        }
        Err(_) => {
            tracing::warn!("connection pool mutex poisoned — skipping stale eviction");
        }
    }
}

/// Current number of cached clients. Exposed for diagnostics/tests.
#[cfg(test)]
pub fn len() -> usize {
    pool().lock().map(|g| g.len()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Unit tests — the pool is exercised via `GiteaClient::new` in the wider
// integration suite; here we only verify the cache-key stability and the
// eviction/LRU behaviour.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Auth;

    #[test]
    fn key_for_is_stable_for_same_inputs() {
        // Same inputs must hash to the same signature.
        let k1 = key_for("https://gitea.example.com", &Auth::None);
        let k2 = key_for("https://gitea.example.com", &Auth::None);
        assert_eq!(k1, k2);
    }

    #[test]
    fn key_for_differs_on_base_url() {
        let k1 = key_for("https://gitea.example.com", &Auth::None);
        let k2 = key_for("https://gitea.other.com", &Auth::None);
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_for_differs_on_auth() {
        let k_none = key_for("https://g.example", &Auth::None);
        let k_tok = key_for("https://g.example", &Auth::Token("abc".to_string()));
        let k_tok2 = key_for("https://g.example", &Auth::Token("xyz".to_string()));
        let k_basic = key_for(
            "https://g.example",
            &Auth::Basic {
                user: "u".to_string(),
                pass: "p".to_string(),
            },
        );
        assert_ne!(k_none, k_tok);
        assert_ne!(k_none, k_basic);
        assert_ne!(k_tok, k_tok2);
        assert_ne!(k_tok, k_basic);
    }

    #[test]
    fn acquire_returns_a_client() {
        // Smoke test: acquire must hand back a usable client. The pool
        // may already contain entries from other tests (shared global),
        // so we only assert that the result is Ok.
        let client = acquire("https://gitea-pool-test.example.com", &Auth::None);
        assert!(client.is_ok(), "acquire must succeed for None auth");
    }

    #[test]
    fn acquire_does_not_panic_under_high_concurrency_keys() {
        // Defensive: hammering the pool with many distinct keys must not
        // deadlock or panic, even if the LRU branch kicks in.
        for i in 0..(POOL_MAX * 2) {
            let url = format!("https://gitea-{}.example.com", i);
            let _ = acquire(&url, &Auth::None);
        }
        // After churning through 2x POOL_MAX distinct keys, the pool
        // size must remain bounded by POOL_MAX.
        assert!(
            len() <= POOL_MAX,
            "pool grew past POOL_MAX: len={}",
            len()
        );
    }

    #[test]
    fn evict_stale_is_safe_on_empty_pool() {
        // Must not panic whether or not the pool has been initialised.
        evict_stale();
    }
}
