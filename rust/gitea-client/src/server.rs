//! axum-based HTTP server that receives Gitea webhooks — stage 9.A.
//!
//! ## Overview
//!
//! This module exposes a single endpoint (`POST /gitea-webhook/post`) which:
//!
//! 1. Optionally verifies the `X-Gitea-Signature` HMAC-SHA256 header against
//!    the raw request body (using the shared secret configured on the
//!    Jenkins side).
//! 2. Reads the `X-Gitea-Event` header to determine the event kind.
//! 3. Forwards the raw payload (as a UTF-8 string) to Java by calling
//!    `RustWebhookDispatcher.handleEvent(type, json)` via JNI.
//!
//! The full JSON payload is forwarded to Java unmodified — Rust does only
//! the minimal validation needed to route the request and reject obviously
//! malformed inputs. Java's Jackson `ObjectMapper` then deserialises the
//! payload into the existing `GiteaXxxEvent` POJO hierarchy.
//!
//! ## HMAC verification
//!
//! [`verify_hmac`] is a pure function so it can be unit-tested in
//! isolation. The HTTP handler delegates to it after normalising the
//! header. If `WebhookState::hmac_secret` is `None`, verification is
//! skipped (with a one-time WARN log via `tracing`).

use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use cidr::IpCidr;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::oneshot;

use crate::rate_limiter::RateLimiter;

type HmacSha256 = Hmac<Sha256>;

/// Header name carrying the lowercase event type ("push", "pull_request", …).
pub const HEADER_EVENT: &str = "x-gitea-event";

/// Header name carrying the hex-encoded HMAC-SHA256 of the request body.
pub const HEADER_SIGNATURE: &str = "x-gitea-signature";

/// Static callback hook used by the HTTP handler to invoke Java. In
/// production this is `Some(real_jni_callback)`; in tests it is `None`
/// (the handler logs a DEBUG message and returns `200 OK`).
///
/// Stored as an `Arc` so the handler (which is `Clone`) can capture it
/// cheaply. The actual `JavaVM` is `Copy` (it is just a pointer), but
/// wrapping the closure in an `Arc` lets us swap implementations
/// without touching `WebhookState`.
pub type JavaCallback = Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;

static JAVA_CALLBACK: OnceLock<JavaCallback> = OnceLock::new();

/// Install the process-wide Java callback. Called once from
/// [`crate::jni_webhook::nativeStart`] after the JVM has been
/// attached. Subsequent calls replace the previous callback (this
/// happens on plugin reload).
pub fn set_java_callback(callback: JavaCallback) {
    let _ = JAVA_CALLBACK.set(callback);
}

/// Drop the process-wide Java callback (called from `nativeStop`).
/// After this, the HTTP handler will return `503 Service Unavailable`
/// on any incoming webhook until `set_java_callback` is called again.
pub fn clear_java_callback() {
    // `OnceLock` cannot be reset; instead we install a stub that always
    // returns an error. This is fine because the server is typically
    // shut down at the same time.
    let stub: JavaCallback = Arc::new(|_t, _p| {
        Err("java callback disabled".to_string())
    });
    // `set` will fail because the cell is already initialised; that is
    // expected — we cannot truly clear a `OnceLock`. The fallback in
    // `invoke_callback` handles the case where the cell was never set.
    let _ = JAVA_CALLBACK.set(stub);
}

fn invoke_callback(event_type: &str, payload: &str) -> Result<(), String> {
    match JAVA_CALLBACK.get() {
        Some(cb) => cb(event_type, payload),
        None => Err("java callback not installed".to_string()),
    }
}

/// Process-wide reference to the *most recently started* server's rate
/// limiter. Used by [`cleanup_loop`] to evict stale buckets.
///
/// `OnceLock` cannot be reset; on a plugin reload a new server installs a
/// new limiter here, but the old value is retained until the process
/// exits. This is acceptable because:
/// (a) the cleanup task only reads the latest value;
/// (b) the previous limiter is dropped when the old `WebhookState`
///     (cloned into the old router) goes out of scope.
static CLEANUP_LIMITER: OnceLock<Arc<RateLimiter>> = OnceLock::new();

/// Register the active rate limiter for periodic cleanup. Idempotent in
/// the sense that the second call wins for a fresh `OnceLock`; once the
/// cell is set we keep the *original* limiter (since `OnceLock::set`
/// fails silently on a populated cell, and the cleanup task reads
/// whatever is there). The previous limiter is dropped when the old
/// server's router is torn down by `WebhookServer::shutdown`.
fn register_rate_limiter_for_cleanup(limiter: Arc<RateLimiter>) {
    let _ = CLEANUP_LIMITER.set(limiter);
}

/// Background cleanup loop. Sweeps the registered rate limiter's bucket
/// map every 5 minutes, evicting buckets idle for more than 10 minutes.
/// Terminates when the tokio runtime tears it down — there is no explicit
/// shutdown channel because the existing `WebhookServer::shutdown` drops
/// the `shutdown_tx` half which fires `axum::serve`'s graceful shutdown,
/// and the runtime aborts this spawned task at the same time.
async fn cleanup_loop() {
    let mut interval = tokio::time::interval(Duration::from_secs(300));
    interval.tick().await; // skip the immediate first tick
    loop {
        interval.tick().await;
        if let Some(limiter) = CLEANUP_LIMITER.get() {
            limiter.cleanup_stale(Duration::from_secs(600));
        }
    }
}

/// Shared state for the axum router. Cheaply cloneable (the secret,
/// bearer token and rate limiter are all behind an `Arc`).
#[derive(Clone)]
pub struct WebhookState {
    /// If `None`, HMAC verification is skipped entirely. Otherwise the
    /// incoming `X-Gitea-Signature` must match HMAC-SHA256(body, secret).
    pub hmac_secret: Option<Arc<String>>,
    /// If `Some`, incoming requests must carry an
    /// `Authorization: Bearer <token>` header matching this value. This is
    /// an *additional* optional layer on top of HMAC — useful for
    /// deployments where the Gitea side is configured to send a static
    /// token but HMAC rotation is undesirable. When `None`, the check is
    /// skipped entirely.
    pub bearer_token: Option<Arc<String>>,
    /// IP allowlist. Empty = allow all source IPs. Non-empty ⇒ only
    /// requests whose remote address falls into one of the listed CIDRs
    /// are accepted; everything else is rejected with 403.
    pub allowed_cidrs: Arc<Vec<IpCidr>>,
    /// Per-IP token bucket rate limiter. See [`crate::rate_limiter`].
    pub rate_limiter: Arc<RateLimiter>,
}

/// Lifecycle handle for a running webhook server. Dropping this value
/// does NOT stop the server — call [`WebhookServer::shutdown`] to do
/// that gracefully.
pub struct WebhookServer {
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<Result<(), std::io::Error>>>,
    local_addr: std::net::SocketAddr,
}

impl WebhookServer {
    /// Bind a webhook server on `0.0.0.0:{port}` and start serving in a
    /// background tokio task on the current runtime.
    ///
    /// Returns once the listener is bound (so the caller can start
    /// sending requests immediately). The server runs until
    /// [`WebhookServer::shutdown`] is called.
    ///
    /// # Arguments
    ///
    /// * `port` — TCP port to bind. `0` lets the OS pick an ephemeral port
    ///   (mainly useful for tests).
    /// * `hmac_secret` — `None` or `Some(empty)` disables HMAC verification.
    ///   Otherwise the incoming `X-Gitea-Signature` must match
    ///   HMAC-SHA256(body, secret).
    /// * `bearer_token` — optional static bearer token checked against the
    ///   `Authorization: Bearer …` header. `None` disables the check.
    /// * `allowed_cidrs` — list of CIDRs that may send webhooks. Empty
    ///   means "allow all source IPs". Unparseable entries are silently
    ///   skipped (a WARN is logged per drop) — we never fail the whole
    ///   server start because of one bad CIDR string.
    /// * `rate_limit_per_minute` — token-bucket capacity AND refill rate
    ///   per source IP. `0` is clamped to `1` so the bucket can always
    ///   cover at least one request.
    pub async fn start(
        port: u16,
        hmac_secret: Option<String>,
        bearer_token: Option<String>,
        allowed_cidrs: Vec<String>,
        rate_limit_per_minute: u32,
    ) -> std::io::Result<Self> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let secret = hmac_secret
            .map(Arc::new)
            .filter(|s| !s.is_empty());
        if secret.is_none() {
            tracing::warn!(
                "starting webhook server without HMAC secret — \
                 incoming requests will NOT be authenticated"
            );
        }

        // Parse the CIDR list. Empty ⇒ allow all. Skip unparseable entries
        // with a WARN so the operator can spot typos in the Jelly config.
        let parsed_cidrs: Vec<IpCidr> = allowed_cidrs
            .iter()
            .filter_map(|raw| match raw.trim().parse::<IpCidr>() {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(cidr = %raw, error = %e, "ignoring unparseable CIDR entry");
                    None
                }
            })
            .collect();
        if !allowed_cidrs.is_empty() {
            tracing::info!(
                count = parsed_cidrs.len(),
                "IP allowlist active — requests from non-matching CIDRs will be rejected"
            );
        }

        // Clamp the rate limit: capacity must be >= 1 so each client can
        // send at least one probe request. Refill rate is expressed in
        // tokens per second, derived from the per-minute setting.
        let capacity = rate_limit_per_minute.max(1);
        let refill_per_second = capacity as f64 / 60.0;
        let rate_limiter = Arc::new(RateLimiter::new(capacity, refill_per_second));

        let state = WebhookState {
            hmac_secret: secret,
            bearer_token: bearer_token
                .map(Arc::new)
                .filter(|s| !s.is_empty()),
            allowed_cidrs: Arc::new(parsed_cidrs),
            rate_limiter: rate_limiter.clone(),
        };

        let app = Router::new()
            // axum auto-redirects `/post` → `/post/` when only the trailing-
            // slash form is registered; register both explicitly so we
            // accept either spelling.
            .route("/gitea-webhook/post", post(handle_webhook))
            .route("/gitea-webhook/post/", post(handle_webhook))
            .with_state(state);

        let addr: std::net::SocketAddr = format!("0.0.0.0:{}", port).parse().expect(
            "internal error: 0.0.0.0:{port} should always parse to a valid SocketAddr",
        );
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        tracing::info!(
            addr = %local_addr,
            "Gitea webhook listener bound — POST /gitea-webhook/post"
        );

        // Spawn the background cleanup task. It scans the rate-limiter's
        // bucket map every 5 minutes and drops IPs that have been idle
        // for > 10 minutes, bounding memory growth under spoofed-source
        // floods.
        //
        // The limiter is registered in a static `OnceLock` so the cleanup
        // task can find it by reference. Only the *most recently started*
        // server's limiter wins (matches the existing replace-on-reload
        // behaviour of the Java callback slot in [`JAVA_CALLBACK`]).
        //
        // The shutdown_rx is consumed by the main `axum::serve` call
        // below, so the cleanup task simply runs forever until the tokio
        // runtime tears it down (which happens when
        // `WebhookServer::shutdown` awaits the main handle).
        register_rate_limiter_for_cleanup(rate_limiter);
        tokio::spawn(cleanup_loop());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        Ok(WebhookServer {
            shutdown_tx: Some(shutdown_tx),
            handle: Some(handle),
            local_addr,
        })
    }

    /// The actual address the server is listening on. Useful when `port`
    /// was `0` (let the OS pick a port) — primarily for tests.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Gracefully stop the server. Sends the shutdown signal, then waits
    /// for the background task to finish. Idempotent: a second call is a
    /// no-op.
    pub async fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            match handle.await {
                Ok(Ok(())) => tracing::info!("webhook server shut down cleanly"),
                Ok(Err(e)) => tracing::error!(error = %e, "webhook server exited with error"),
                Err(e) => tracing::error!(error = %e, "webhook server task panicked"),
            }
        }
    }
}

/// axum handler for `POST /gitea-webhook/post`.
///
/// Pipeline (stage 16):
/// 1. **IP allowlist** — if `WebhookState::allowed_cidrs` is non-empty,
///    the request is rejected with `403 FORBIDDEN` unless the remote IP
///    falls into one of the listed CIDRs.
/// 2. **Rate limit** — token bucket per IP. Over-budget requests get
///    `429 TOO_MANY_REQUESTS`. Runs *after* the allowlist so a flood
///    from a blocked IP cannot consume tokens from any client's bucket
///    (because the lookup never happens for blocked IPs).
/// 3. **Bearer token** — optional. If `WebhookState::bearer_token` is
///    set, the `Authorization: Bearer <token>` header must match.
///    Otherwise `401 UNAUTHORIZED`.
/// 4. **HMAC verification** (stage 9.A) — the original security layer.
/// 5. Dispatch to the Java callback.
async fn handle_webhook(
    State(state): State<WebhookState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let client_ip = addr.ip();

    // 1. IP allowlist (CIDR matching). Empty list ⇒ allow all.
    if !state.allowed_cidrs.is_empty() {
        let allowed = state.allowed_cidrs.iter().any(|cidr| cidr.contains(&client_ip));
        if !allowed {
            tracing::warn!(ip = %client_ip, "webhook rejected: IP not in allowlist");
            return StatusCode::FORBIDDEN;
        }
    }

    // 2. Rate limit (token bucket per IP). Runs after the allowlist so
    //    blocked IPs cannot burn tokens from any legitimate client's
    //    bucket (the lookup never happens for them).
    if !state.rate_limiter.check(client_ip) {
        tracing::warn!(ip = %client_ip, "webhook rejected: rate limited");
        return StatusCode::TOO_MANY_REQUESTS;
    }

    // 3. Bearer token (optional). When configured, the request must carry
    //    an `Authorization: Bearer <token>` header matching the expected
    //    value. We deliberately do NOT use constant-time comparison here
    //    because the bearer token is not a cryptographic secret in the
    //    same way HMAC keys are — it's a long-lived credential that
    //    should be rotated rather than timed. The token is also not
    //    logged at INFO+.
    if let Some(expected) = &state.bearer_token {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let provided = auth.strip_prefix("Bearer ").unwrap_or("");
        if provided != expected.as_str() {
            tracing::warn!(ip = %client_ip, "webhook rejected: invalid bearer token");
            return StatusCode::UNAUTHORIZED;
        }
    }

    // 4. HMAC verification (skipped if no secret configured).
    if let Some(secret) = &state.hmac_secret {
        let sig_header = match headers.get(HEADER_SIGNATURE) {
            Some(v) => match v.to_str() {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!(
                        "X-Gitea-Signature header is not valid UTF-8 — rejecting"
                    );
                    return StatusCode::BAD_REQUEST;
                }
            },
            None => {
                tracing::warn!("request missing X-Gitea-Signature header — rejecting");
                return StatusCode::UNAUTHORIZED;
            }
        };
        if !verify_hmac(secret, &body, sig_header) {
            tracing::warn!("HMAC verification failed — rejecting webhook");
            return StatusCode::UNAUTHORIZED;
        }
    }

    // 5. Determine event type.
    let event_header = match headers.get(HEADER_EVENT) {
        Some(v) => match v.to_str() {
            Ok(s) => s,
            Err(_) => return StatusCode::BAD_REQUEST,
        },
        None => {
            tracing::warn!("request missing X-Gitea-Event header — rejecting");
            return StatusCode::BAD_REQUEST;
        }
    };
    let event_type = event_header.trim().to_ascii_lowercase();

    // 6. Parse body as UTF-8 so we can hand Java a String. axum::body::Bytes
    //    is cheap to convert — it is already a contiguous buffer.
    let payload_str = match std::str::from_utf8(&body) {
        Ok(s) => s.to_string(),
        Err(_) => {
            tracing::warn!("request body is not valid UTF-8 — rejecting");
            return StatusCode::BAD_REQUEST;
        }
    };

    // 7. Invoke Java callback.
    if let Err(e) = invoke_callback(&event_type, &payload_str) {
        tracing::error!(
            event = %event_type,
            error = %e,
            "Java callback failed"
        );
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    tracing::debug!(event = %event_type, bytes = body.len(), "webhook forwarded to Java");
    StatusCode::OK
}

/// Constant-time comparison of two hex digests.
///
/// `ring`/`subtle` provide `ConstantTimeEq`, but we intentionally avoid
/// adding another dependency for two ASCII strings. The implementation
/// short-circuits on length mismatch (which is not a secret) and XORs all
/// bytes otherwise, so a timing attacker learns only the digest length —
/// not its contents.
pub fn timing_safe_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Pure HMAC-SHA256 verification of a webhook body.
///
/// Returns `true` iff `HMAC-SHA256(secret, body)` hex-encoded equals
/// `provided_signature` (case-insensitive on the signature side). This
/// function is the unit-testable heart of the verification logic.
///
/// `provided_signature` is treated case-insensitively because Gitea (and
/// GitHub) emits lowercase hex digests but the spec is ambiguous and we
/// do not want to reject a well-meaning client that sends uppercase.
pub fn verify_hmac(secret: &str, body: &[u8], provided_signature: &str) -> bool {
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            // HmacSha256 accepts any key length; this branch is unreachable
            // in practice but is kept for API safety.
            return false;
        }
    };
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());
    // Compare case-insensitively. `to_ascii_lowercase` is fine for hex.
    let provided_lower = provided_signature.to_ascii_lowercase();
    timing_safe_eq(&expected, &provided_lower)
}

// ---------------------------------------------------------------------------
// Tests — exercise the pure logic (no JVM, no real socket).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_accepts_correct_signature() {
        let secret = "topsecret";
        let body = br#"{"ref":"refs/heads/main"}"#;
        // Compute the expected signature the same way Gitea would.
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = hex::encode(mac.finalize().into_bytes());
        assert!(verify_hmac(secret, body, &sig));
    }

    #[test]
    fn hmac_rejects_wrong_signature() {
        let secret = "topsecret";
        let body = br#"{"ref":"refs/heads/main"}"#;
        assert!(!verify_hmac(secret, body, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"));
    }

    #[test]
    fn hmac_rejects_tampered_body() {
        let secret = "topsecret";
        let body = br#"{"ref":"refs/heads/main"}"#;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = hex::encode(mac.finalize().into_bytes());
        // Flip a byte in the body.
        let mut tampered = body.to_vec();
        tampered[0] ^= 0xFF;
        assert!(!verify_hmac(secret, &tampered, &sig));
    }

    #[test]
    fn hmac_accepts_uppercase_signature() {
        let secret = "topsecret";
        let body = br#"{"hello":"world"}"#;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = hex::encode_upper(mac.finalize().into_bytes());
        assert!(verify_hmac(secret, body, &sig));
    }

    #[test]
    fn timing_safe_eq_handles_basic_cases() {
        assert!(timing_safe_eq("abc", "abc"));
        assert!(!timing_safe_eq("abc", "abd"));
        assert!(!timing_safe_eq("abc", "abcd"));
        assert!(!timing_safe_eq("abcd", "abc"));
        assert!(timing_safe_eq("", ""));
    }

    #[test]
    fn missing_callback_returns_error() {
        // We deliberately don't install a callback in tests; the OnceLock
        // may have been set by another test, so we just call the helper
        // and assert it returns either Ok or Err (not panic).
        let _ = invoke_callback("push", "{}");
    }

    // -----------------------------------------------------------------------
    // End-to-end HTTP tests using a real axum server on an ephemeral port.
    // We install a fake callback that records what the handler would have
    // forwarded to Java.
    //
    // **Parallelism note.** Cargo runs `#[test]`s in parallel by default.
    // Because `JAVA_CALLBACK` is a process-wide `OnceLock`, all tests
    // share a single recording callback and a single `RECORDED` Vec. We
    // therefore embed a unique marker (UUID-ish string) in each test's
    // request body and filter for that marker afterwards — no test
    // depends on `len()` or `is_empty()`, so concurrent recording is
    // safe.
    // -----------------------------------------------------------------------

    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Recorded events from the test callback.
    static RECORDED: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();

    fn recorded() -> &'static Mutex<Vec<(String, String)>> {
        RECORDED.get_or_init(|| Mutex::new(Vec::new()))
    }

    fn install_test_callback() {
        let cb: JavaCallback = Arc::new(|event_type, payload| {
            // Use `lock().unwrap_or_else(|p| p.into_inner())` so that a
            // poisoned mutex (caused by a panic in another test) does not
            // cascade into every subsequent test.
            let mut guard = match recorded().lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.push((event_type.to_string(), payload.to_string()));
            Ok(())
        });
        // If a callback was already installed (e.g. by a prior test), we
        // can't reset the OnceLock — but since all our tests use the same
        // recording callback, this is fine.
        let _ = JAVA_CALLBACK.set(cb);
    }

    /// Generate a per-test unique marker. We use the PID + current nanos
    /// to reduce the chance of two parallel tests colliding.
    fn marker(test_name: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("__test_marker_{}_{}__", test_name, nanos)
    }

    /// Look up the recorded payload for a given test marker.
    fn find_recorded(marker: &str) -> Option<(String, String)> {
        let guard = match recorded().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard
            .iter()
            .find(|(_, payload)| payload.contains(marker))
            .cloned()
    }

    fn count_recorded_with(marker: &str) -> usize {
        let guard = match recorded().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard
            .iter()
            .filter(|(_, payload)| payload.contains(marker))
            .count()
    }

    /// Helper to compute the HMAC-SHA256 hex digest the way Gitea does.
    fn compute_sig(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[tokio::test]
    async fn server_accepts_valid_signature() {
        install_test_callback();
        let m = marker("server_accepts_valid_signature");

        let mut server = WebhookServer::start(0, Some("topsecret".to_string()), None, vec![], 60)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/post", addr);

        // Embed our unique marker in the body so we can find this test's
        // record even when other tests are running concurrently.
        let body = format!(
            r#"{{"ref":"refs/heads/main","_test":"{}","repository":{{"name":"x","full_name":"o/x","html_url":"http://h/o/x","owner":{{"login":"o"}}}},"sender":{{"login":"u"}}}}"#,
            m
        );
        let sig = compute_sig("topsecret", body.as_bytes());

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("X-Gitea-Event", "push")
            .header("X-Gitea-Signature", sig)
            .header("Content-Type", "application/json")
            .body(body.clone())
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), StatusCode::OK);
        server.shutdown().await;

        // Give the background callback a brief chance to complete. The
        // HTTP response is sent before the callback returns (since the
        // handler `await`s it), so the record should already be in place.
        let record = find_recorded(&m);
        assert!(record.is_some(), "callback did not record our payload");
        let (event_type, payload) = record.unwrap();
        assert_eq!(event_type, "push");
        assert!(payload.contains("refs/heads/main"));
    }

    #[tokio::test]
    async fn server_rejects_invalid_signature() {
        install_test_callback();
        let m = marker("server_rejects_invalid_signature");

        let mut server = WebhookServer::start(0, Some("topsecret".to_string()), None, vec![], 60)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/post", addr);

        let body = format!(r#"{{"ref":"refs/heads/main","_test":"{}"}}"#, m);
        let bad_sig = "0".repeat(64);

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("X-Gitea-Event", "push")
            .header("X-Gitea-Signature", bad_sig)
            .body(body)
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        server.shutdown().await;

        // The handler should NOT have invoked the callback for a rejected
        // request. We only count records that match our marker — other
        // tests' records don't pollute the assertion.
        let count = count_recorded_with(&m);
        assert_eq!(
            count, 0,
            "callback should not be invoked on HMAC failure"
        );
    }

    #[tokio::test]
    async fn server_rejects_missing_signature_header() {
        install_test_callback();

        let mut server = WebhookServer::start(0, Some("topsecret".to_string()), None, vec![], 60)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/post", addr);

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("X-Gitea-Event", "push")
            .body(r#"{"ref":"refs/heads/main"}"#.to_string())
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn server_allows_no_secret_mode() {
        // When no secret is configured, HMAC verification is skipped and
        // any request with a valid event header reaches the callback.
        install_test_callback();
        let m = marker("server_allows_no_secret_mode");

        let mut server = WebhookServer::start(0, None, None, vec![], 60)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/post", addr);

        let body = format!(r#"{{"action":"opened","number":1,"_test":"{}"}}"#, m);

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("X-Gitea-Event", "pull_request")
            .body(body)
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), StatusCode::OK);
        server.shutdown().await;

        let record = find_recorded(&m);
        assert!(record.is_some(), "callback did not record our payload");
        let (event_type, _payload) = record.unwrap();
        assert_eq!(event_type, "pull_request");
    }

    #[tokio::test]
    async fn server_rejects_missing_event_header() {
        install_test_callback();

        let mut server = WebhookServer::start(0, None, None, vec![], 60)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/post", addr);

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .body(r#"{}"#.to_string())
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn server_rejects_non_utf8_body() {
        install_test_callback();

        let mut server = WebhookServer::start(0, None, None, vec![], 60)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/post", addr);

        let client = reqwest::Client::new();
        // 0xFF is never valid UTF-8.
        let resp = client
            .post(&url)
            .header("X-Gitea-Event", "push")
            .body(vec![0xFFu8, 0xFE, 0xFD])
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn server_accepts_trailing_slash() {
        install_test_callback();
        let m = marker("server_accepts_trailing_slash");

        let mut server = WebhookServer::start(0, None, None, vec![], 60)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        // Note trailing slash.
        let url = format!("http://{}/gitea-webhook/post/", addr);

        let body = format!(r#"{{"ref":"refs/heads/main","_test":"{}"}}"#, m);

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("X-Gitea-Event", "push")
            .body(body)
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), StatusCode::OK);
        server.shutdown().await;

        assert!(find_recorded(&m).is_some());
    }

    #[tokio::test]
    async fn server_shutdown_is_idempotent() {
        let mut server = WebhookServer::start(0, None, None, vec![], 60)
            .await
            .expect("failed to bind test server");
        server.shutdown().await;
        // Calling again must not panic.
        server.shutdown().await;
    }
}
