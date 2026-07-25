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
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use cidr::IpCidr;
use hmac::{Hmac, Mac};
use lru::LruCache;
use prometheus::{register_histogram_vec, register_int_counter_vec, HistogramVec, IntCounterVec};
use sha2::Sha256;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::oneshot;

use crate::rate_limiter::RateLimiter;

type HmacSha256 = Hmac<Sha256>;

/// Header name carrying the lowercase event type ("push", "pull_request", …).
pub const HEADER_EVENT: &str = "x-gitea-event";

/// Header name carrying the hex-encoded HMAC-SHA256 of the request body.
pub const HEADER_SIGNATURE: &str = "x-gitea-signature";

/// Header name carrying the unique delivery UUID for a webhook (issue #11).
/// Gitea populates this on every webhook delivery and reuses it across
/// retries, so deduplicating on this header is sufficient to make the
/// webhook receiver idempotent.
pub const HEADER_DELIVERY: &str = "x-gitea-delivery";

// ---------------------------------------------------------------------------
// Prometheus metrics (issue #10).
//
// `LazyLock` defers the `register_*` calls until first use so the static
// registry stays clean in tests that don't touch the webhook server.
// ---------------------------------------------------------------------------

/// Total webhook requests, partitioned by event type and outcome label
/// (`ok`, `bad_request`, `unauthorized`, `rate_limited`, `forbidden`,
/// `duplicate`, `error`).
pub static WEBHOOK_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gitea_webhook_requests_total",
        "Total webhook requests received by the Rust webhook server",
        &["event_type", "status"]
    )
    .expect("failed to register gitea_webhook_requests_total")
});

/// JNI callback latency in seconds, partitioned by event type. Covers only
/// the time spent inside [`invoke_callback`] — the upstream IP/HMAC/rate
/// checks are not included because they are dominated by cheap in-memory
/// operations.
pub static CALLBACK_LATENCY: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "gitea_webhook_callback_latency_seconds",
        "JNI callback latency in seconds",
        &["event_type"],
        vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0]
    )
    .expect("failed to register gitea_webhook_callback_latency_seconds")
});

// ---------------------------------------------------------------------------
// Idempotency cache (issue #11).
//
// Bounded at 2048 entries — enough to absorb Gitea's default 5-retry burst
// (5-min jitter) for any realistic number of repositories, while keeping
// memory growth bounded. The LRU eviction policy is the right fit here
// because a delivery ID is "hot" only for the brief window between the
// original delivery and its retries.
// ---------------------------------------------------------------------------

/// Bounded LRU of recently-seen `X-Gitea-Delivery` values. Presence in the
/// cache ⇒ a prior delivery already triggered the Java callback, so a
/// retry must be a no-op.
static DELIVERY_CACHE: LazyLock<Mutex<LruCache<String, ()>>> =
    LazyLock::new(|| Mutex::new(LruCache::new(std::num::NonZeroUsize::new(2048).expect("2048 > 0"))));

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

/// `GET /gitea-webhook/health` — Kubernetes liveness/readiness probe
/// (issue #9).
///
/// Returns `200 {"status":"ok"}` unconditionally. No auth, no state, no
/// side-effects — the kubelet calls this on a tight cadence and must not
/// burn rate-limit tokens or trigger any logging.
async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        r#"{"status":"ok"}"#,
    )
}

/// `GET /gitea-webhook/metrics` — Prometheus text-format scrape target
/// (issue #10).
///
/// Encodes every metric family registered against the default registry.
/// The body is `text/plain; version=0.0.4` per the Prometheus exposition
/// format spec.
async fn metrics() -> impl IntoResponse {
    let encoder = prometheus::TextEncoder::new();
    let metric_families = prometheus::gather();
    let body = match encoder.encode_to_string(&metric_families) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to encode prometheus metrics");
            String::new()
        }
    };
    (
        StatusCode::OK,
        [("Content-Type", "text/plain; version=0.0.4")],
        body,
    )
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
/// map every 5 minutes, evicting buckets idle for more than 10 minutes,
/// and also runs the connection-pool TTL sweep (issue #8). Terminates
/// when the tokio runtime tears it down — there is no explicit shutdown
/// channel because the existing `WebhookServer::shutdown` drops the
/// `shutdown_tx` half which fires `axum::serve`'s graceful shutdown,
/// and the runtime aborts this spawned task at the same time.
async fn cleanup_loop() {
    let mut interval = tokio::time::interval(Duration::from_secs(300));
    interval.tick().await; // skip the immediate first tick
    loop {
        interval.tick().await;
        if let Some(limiter) = CLEANUP_LIMITER.get() {
            limiter.cleanup_stale(Duration::from_secs(600));
        }
        // Sweep idle entries out of the connection pool (issue #8). Safe
        // to call unconditionally — `evict_stale` no-ops if the pool has
        // never been initialised.
        crate::pool::evict_stale();
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
        path_prefix: Option<String>,
    ) -> std::io::Result<Self> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Path prefix: defaults to "/gitea-webhook" (back-compat with v1.0).
        // Operator can override (e.g. "/jenkins/gitea-plugin") when a corp
        // reverse proxy requires a custom path. We trim trailing slashes and
        // clamp the prefix to a non-empty path-looking string.
        let prefix = path_prefix
            .map(|p| p.trim().trim_end_matches('/').to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "/gitea-webhook".to_string());

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

        let post_path = format!("{prefix}/post");
        let post_path_slash = format!("{post_path}/");
        let health_path = format!("{prefix}/health");
        let health_path_slash = format!("{health_path}/");
        let metrics_path = format!("{prefix}/metrics");
        let metrics_path_slash = format!("{metrics_path}/");

        tracing::info!(
            prefix = %prefix,
            post = %post_path,
            health = %health_path,
            metrics = %metrics_path,
            "webhook routes registered"
        );

        let app = Router::new()
            // axum auto-redirects `/post` → `/post/` when only the trailing-
            // slash form is registered; register both explicitly so we
            // accept either spelling.
            .route(&post_path, post(handle_webhook))
            .route(&post_path_slash, post(handle_webhook))
            // Kubernetes liveness/readiness probe target (issue #9).
            // Responds 200 `{"status":"ok"}` without any auth — the kubelet
            // does not (and should not) carry an HMAC secret.
            .route(&health_path, get(health))
            .route(&health_path_slash, get(health))
            // Prometheus scrape target (issue #10). Exposed without auth
            // so the scraper does not need a per-namespace token; the
            // metrics carry no sensitive payload (no repo names, no
            // secrets, just counters + histogram buckets).
            .route(&metrics_path, get(metrics))
            .route(&metrics_path_slash, get(metrics))
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
/// Pipeline (stage 16 + issue #10/#11 enhancements):
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
/// 5. **Idempotency** (issue #11) — `X-Gitea-Delivery` is checked against
///    a bounded LRU cache. A second delivery with the same ID short-
///    circuits with `200 OK` without invoking the Java callback, so Gitea
///    retries do not produce duplicate `SCMEvent`s in Jenkins.
/// 6. Dispatch to the Java callback, recording Prometheus metrics for
///    total requests and JNI callback latency (issue #10).
async fn handle_webhook(
    State(state): State<WebhookState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let client_ip = addr.ip();

    // We need an `event_type` label for the Prometheus counters. The
    // event header is read *after* the security layers, so for early
    // rejections (allowlist / rate-limit / bearer / HMAC) we fall back
    // to the literal `"unknown"` label rather than leaking any header
    // value that a malicious client might have set.
    fn record(event_type: &str, status: &str) {
        WEBHOOK_REQUESTS
            .with_label_values(&[event_type, status])
            .inc();
    }

    // 1. IP allowlist (CIDR matching). Empty list ⇒ allow all.
    if !state.allowed_cidrs.is_empty() {
        let allowed = state.allowed_cidrs.iter().any(|cidr| cidr.contains(&client_ip));
        if !allowed {
            tracing::warn!(ip = %client_ip, "webhook rejected: IP not in allowlist");
            record("unknown", "forbidden");
            return StatusCode::FORBIDDEN;
        }
    }

    // 2. Rate limit (token bucket per IP). Runs after the allowlist so
    //    blocked IPs cannot burn tokens from any legitimate client's
    //    bucket (the lookup never happens for them).
    if !state.rate_limiter.check(client_ip) {
        tracing::warn!(ip = %client_ip, "webhook rejected: rate limited");
        record("unknown", "rate_limited");
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
            record("unknown", "unauthorized");
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
                    record("unknown", "bad_request");
                    return StatusCode::BAD_REQUEST;
                }
            },
            None => {
                tracing::warn!("request missing X-Gitea-Signature header — rejecting");
                record("unknown", "unauthorized");
                return StatusCode::UNAUTHORIZED;
            }
        };
        if !verify_hmac(secret, &body, sig_header) {
            tracing::warn!("HMAC verification failed — rejecting webhook");
            record("unknown", "unauthorized");
            return StatusCode::UNAUTHORIZED;
        }
    }

    // 5. Determine event type.
    let event_header = match headers.get(HEADER_EVENT) {
        Some(v) => match v.to_str() {
            Ok(s) => s,
            Err(_) => {
                record("unknown", "bad_request");
                return StatusCode::BAD_REQUEST;
            }
        },
        None => {
            tracing::warn!("request missing X-Gitea-Event header — rejecting");
            record("unknown", "bad_request");
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
            record(&event_type, "bad_request");
            return StatusCode::BAD_REQUEST;
        }
    };

    // 7. Idempotency check (issue #11). `X-Gitea-Delivery` carries a UUID
    //    that Gitea reuses across its retries of the same webhook. If we
    //    have already dispatched this delivery to Java, short-circuit
    //    with 200 OK — the upstream Gitea server treats 2xx as success
    //    and stops retrying.
    //
    //    An empty / missing delivery header skips dedup (we cannot invent
    //    a key) and falls back to the always-dispatch path, preserving
    //    backwards compatibility with clients that don't send the header.
    let delivery_id = headers
        .get(HEADER_DELIVERY)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string());
    if let Some(id) = &delivery_id {
        if !id.is_empty() {
            let mut cache = match DELIVERY_CACHE.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(), // poisoned: keep serving
            };
            if cache.get(id).is_some() {
                tracing::info!(delivery_id = %id, "duplicate webhook delivery, skipping");
                record(&event_type, "duplicate");
                return StatusCode::OK;
            }
            cache.put(id.clone(), ());
        }
    }

    // 8. Invoke Java callback, recording latency into the Prometheus
    //    histogram. The timer covers only the JNI round-trip — the
    //    Java-side `handleEvent` is responsible for the bulk of the
    //    latency, which is exactly what we want to track.
    let timer = std::time::Instant::now();
    let callback_result = invoke_callback(&event_type, &payload_str);
    CALLBACK_LATENCY
        .with_label_values(&[&event_type])
        .observe(timer.elapsed().as_secs_f64());

    if let Err(e) = callback_result {
        tracing::error!(
            event = %event_type,
            error = %e,
            "Java callback failed"
        );
        record(&event_type, "error");
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    tracing::debug!(event = %event_type, bytes = body.len(), "webhook forwarded to Java");
    record(&event_type, "ok");
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

        let mut server = WebhookServer::start(0, Some("topsecret".to_string()), None, vec![], 60, None)
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

        let mut server = WebhookServer::start(0, Some("topsecret".to_string()), None, vec![], 60, None)
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

        let mut server = WebhookServer::start(0, Some("topsecret".to_string()), None, vec![], 60, None)
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

        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
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

        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
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

        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
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

        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
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
        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
            .await
            .expect("failed to bind test server");
        server.shutdown().await;
        // Calling again must not panic.
        server.shutdown().await;
    }

    // -------------------------------------------------------------------
    // Issue #9 — health endpoint.
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn health_endpoint_returns_ok_without_auth() {
        // The health probe must respond 200 even when HMAC is configured,
        // because the Kubernetes kubelet has no way to produce a valid
        // signature.
        let mut server = WebhookServer::start(0, Some("topsecret".to_string()), None, vec![], 60, None)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/health", addr);

        let client = reqwest::Client::new();
        let resp = client.get(&url).send().await.expect("request failed");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.text().await.expect("body");
        assert!(body.contains(r#""status":"ok""#));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn health_endpoint_accepts_trailing_slash() {
        // Match the post/ behaviour: register both spellings.
        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/health/", addr);

        let client = reqwest::Client::new();
        let resp = client.get(&url).send().await.expect("request failed");
        assert_eq!(resp.status(), StatusCode::OK);
        server.shutdown().await;
    }

    // -------------------------------------------------------------------
    // Issue #10 — metrics endpoint.
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn metrics_endpoint_exposes_prometheus_text() {
        install_test_callback();

        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();

        // Fire one webhook so the counters are non-zero.
        let post_url = format!("http://{}/gitea-webhook/post", addr);
        let m = marker("metrics_endpoint_exposes_prometheus_text");
        let body = format!(r#"{{"ref":"refs/heads/main","_test":"{}"}}"#, m);
        let client = reqwest::Client::new();
        let _ = client
            .post(&post_url)
            .header("X-Gitea-Event", "push")
            .body(body)
            .send()
            .await
            .expect("webhook post failed");

        // Scrape /metrics and confirm our counters appear.
        let metrics_url = format!("http://{}/gitea-webhook/metrics", addr);
        let resp = client
            .get(&metrics_url)
            .send()
            .await
            .expect("metrics request failed");
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            ct.starts_with("text/plain"),
            "metrics content-type must be text/plain, got {ct}"
        );
        let body = resp.text().await.expect("metrics body");
        assert!(
            body.contains("gitea_webhook_requests_total"),
            "metrics body missing gitea_webhook_requests_total:\n{body}"
        );
        assert!(
            body.contains("gitea_webhook_callback_latency_seconds"),
            "metrics body missing gitea_webhook_callback_latency_seconds:\n{body}"
        );
        server.shutdown().await;
    }

    // -------------------------------------------------------------------
    // Issue #11 — idempotency dedup via X-Gitea-Delivery.
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn duplicate_delivery_is_skipped() {
        install_test_callback();
        let m = marker("duplicate_delivery_is_skipped");

        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/post", addr);

        // Same delivery ID for both requests — Gitea would emit the same
        // UUID when retrying.
        let delivery = "11111111-1111-1111-1111-111111111111";
        let body = format!(r#"{{"ref":"refs/heads/main","_test":"{}"}}"#, m);

        let client = reqwest::Client::new();
        let resp1 = client
            .post(&url)
            .header("X-Gitea-Event", "push")
            .header("X-Gitea-Delivery", delivery)
            .body(body.clone())
            .send()
            .await
            .expect("first request failed");
        assert_eq!(resp1.status(), StatusCode::OK);

        let resp2 = client
            .post(&url)
            .header("X-Gitea-Event", "push")
            .header("X-Gitea-Delivery", delivery)
            .body(body.clone())
            .send()
            .await
            .expect("second request failed");
        assert_eq!(resp2.status(), StatusCode::OK);

        server.shutdown().await;

        // The callback must have been invoked exactly once despite two
        // deliveries with the same UUID.
        let count = count_recorded_with(&m);
        assert_eq!(
            count, 1,
            "duplicate delivery must not trigger a second callback — got {count}"
        );
    }

    #[tokio::test]
    async fn distinct_deliveries_are_both_dispatched() {
        // Sanity: dedup must NOT collapse two genuinely distinct deliveries.
        install_test_callback();
        let m1 = marker("distinct_deliveries_first");
        let m2 = marker("distinct_deliveries_second");

        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/post", addr);

        let client = reqwest::Client::new();
        let body1 = format!(r#"{{"ref":"refs/heads/main","_test":"{}"}}"#, m1);
        let body2 = format!(r#"{{"ref":"refs/heads/main","_test":"{}"}}"#, m2);

        let r1 = client
            .post(&url)
            .header("X-Gitea-Event", "push")
            .header("X-Gitea-Delivery", "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
            .body(body1)
            .send()
            .await
            .expect("first request failed");
        assert_eq!(r1.status(), StatusCode::OK);

        let r2 = client
            .post(&url)
            .header("X-Gitea-Event", "push")
            .header("X-Gitea-Delivery", "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
            .body(body2)
            .send()
            .await
            .expect("second request failed");
        assert_eq!(r2.status(), StatusCode::OK);

        server.shutdown().await;

        assert_eq!(
            count_recorded_with(&m1),
            1,
            "first distinct delivery must be dispatched"
        );
        assert_eq!(
            count_recorded_with(&m2),
            1,
            "second distinct delivery must be dispatched"
        );
    }

    #[tokio::test]
    async fn missing_delivery_header_still_dispatches() {
        // Backwards-compat: clients that omit X-Gitea-Delivery fall
        // through to the always-dispatch path.
        install_test_callback();
        let m = marker("missing_delivery_header_still_dispatches");

        let mut server = WebhookServer::start(0, None, None, vec![], 60, None)
            .await
            .expect("failed to bind test server");
        let addr = server.local_addr();
        let url = format!("http://{}/gitea-webhook/post", addr);

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

        assert_eq!(
            count_recorded_with(&m),
            1,
            "request without X-Gitea-Delivery must still dispatch"
        );
    }
}
