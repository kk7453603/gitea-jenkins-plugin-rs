# Header Migration — Porting corporate header customizations

This guide covers how to migrate **HTTP header customizations** from a corporate fork to this plugin. The most common corporate customizations are:

1. **Inbound** — add custom header check on webhook requests (e.g. `X-Corp-Signature`, `X-API-Key`)
2. **Outbound** — inject custom headers on Gitea API requests (e.g. `X-Service-Name: jenkins-prod`, `Authorization` refresh)
3. **Header rewriting** — strip or rename headers in transit (rare, usually a reverse-proxy concern)

---

## 1. Where headers are processed

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          Inbound (webhook from Gitea)                   │
│                                                                         │
│  Gitea → axum :8081 → server.rs header pipeline:                       │
│    1. IP CIDR check          ← (IP-based, not header)                  │
│    2. Rate limit             ← (IP-based)                              │
│    3. Bearer token check     ← Authorization: Bearer <token>           │
│    4. HMAC-SHA256 verify     ← X-Gitea-Signature                       │
│    5. Idempotency dedup      ← X-Gitea-Delivery                        │
│    6. Event routing          ← X-Gitea-Event                           │
│    7. Body parse             ← (raw JSON)                              │
│                                                                         │
│  >>> INSERT CUSTOM HEADER CHECK HERE (between 5 and 6) <<<             │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│                         Outbound (Jenkins → Gitea API)                  │
│                                                                         │
│  RustGiteaConnection.nativeXxx → jni.rs → client.rs → reqwest:         │
│    1. Acquire pooled Client   ← pool.rs                                │
│    2. Build URL + path        ← client.rs                              │
│    3. Apply Auth header       ← auth.rs                                │
│       (None / Token / Basic)                                            │
│    4. Send request            ← reqwest                                │
│    5. Receive response         ← reqwest                               │
│                                                                         │
│  >>> INSERT CUSTOM HEADER INJECTION HERE (between 3 and 4) <<<          │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 2. Inbound custom header check

**Use case:** Corporate Gitea is behind a corporate gateway that adds `X-Corp-Signature: <ed25519 sig>`. The plugin must verify this signature on top of (or instead of) HMAC-SHA256.

### Step 1: Choose the insertion point

The corporate check should run **after HMAC verification** (so unauthenticated requests are rejected before expensive crypto) but **before the dedup cache** (so a duplicate is still a duplicate even if corp signature is valid):

```
... HMAC verify → CORP CHECK → dedup → parse → dispatch
```

### Step 2: Rust implementation

In `rust/gitea-client/src/server.rs`, extend `handle_webhook`:

```rust
async fn handle_webhook(
    State(state): State<Arc<WebhookState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // ... existing IP / rate / bearer / HMAC checks ...

    // === CORPORATE HEADER CHECK (insert here) ===
    if let Some(ref corp_verifier) = state.corp_signature_verifier {
        match corp_verifier.verify(&headers, &body) {
            Ok(()) => {}
            Err(e) => {
                WEBHOOK_REQUESTS.with_label_values(&[event_type, "unauthorized"]).inc();
                return (StatusCode::UNAUTHORIZED, "corp signature invalid").into_response();
            }
        }
    }
    // === END CORPORATE CHECK ===

    // ... existing dedup / parse / dispatch ...
}
```

### Step 3: Add config field

In `src/main/java/.../servers/GiteaServers.java`:

```java
/**
 * Corporate signature verification public key (PEM-encoded Ed25519 or RSA).
 * When non-empty, every webhook delivery must carry an `X-Corp-Signature`
 * header that verifies against this key.
 */
private String corpSignaturePublicKey = "";

@Restricted(NoExternalUse.class)
public String getCorpSignaturePublicKey() {
    return corpSignaturePublicKey == null ? "" : corpSignaturePublicKey;
}

@Restricted(NoExternalUse.class)
public void setCorpSignaturePublicKey(String key) {
    this.corpSignaturePublicKey = key == null ? "" : key;
}
```

### Step 4: Pass through JNI

Extend `nativeStart` signature with a 7th argument `corp_pubkey: JString`. Update:
- `jni_webhook.rs::nativeStart` (decode + pass to `WebhookServer::start`)
- `server.rs::WebhookServer::start` (parse PEM, build verifier, store in `WebhookState`)
- `RustWebhookDispatcher.configure` + `nativeStart` (add new arg)
- `WebhookServerStarter.java` + `GiteaServers.configure()` (pass field)

### Step 5: UI field

In `config.jelly`:

```xml
<f:entry title="${%Corporate signature public key (PEM)}" field="corpSignaturePublicKey">
    <f:textarea placeholder="-----BEGIN PUBLIC KEY-----&#10;...&#10;-----END PUBLIC KEY-----"/>
</f:entry>
```

### Effort estimate

- Simple header value comparison (e.g. `X-Corp-Token == "secret"`): 30 min
- HMAC-SHA256 of body with corp secret: 1 hour
- Ed25519 signature verification: 2 hours (needs `ed25519-dalek` crate)
- RSA-SHA256: 2-3 hours (needs `rsa` crate)

---

## 3. Outbound custom header injection

**Use case:** Corporate requires every outbound API call to Gitea to carry `X-Service-Name: jenkins-prod-1` and `X-Request-ID: <uuid>` for tracing.

### Step 1: Choose the layer

Headers can be added at two layers:

**Option A — Global client builder** (applies to every request):

```rust
// In tls.rs or a new corp_headers.rs:
pub fn apply_corp_headers(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .header("X-Service-Name", &*SERVICE_NAME)
        .header("X-Build-Version", env!("CARGO_PKG_VERSION"))
}
```

**Option B — Per-request** (for dynamic values like `X-Request-ID`):

```rust
// In client.rs, before each request:
let request = client.get(url)
    .header("X-Request-ID", &uuid::Uuid::new_v4().to_string())
    .header("Authorization", auth_header)
    .build()?;
```

### Step 2: Store corporate headers

```rust
// In corp_headers.rs:
use once_cell::sync::OnceLock;
use std::collections::HashMap;

static CORP_HEADERS: OnceLock<HashMap<String, String>> = OnceLock::new();

pub fn set_headers(headers: HashMap<String, String>) {
    let _ = CORP_HEADERS.set(headers);
}

pub fn apply_to_builder(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    if let Some(h) = CORP_HEADERS.get() {
        let mut b = builder;
        for (k, v) in h {
            b = b.header(k, v);
        }
        b
    } else {
        builder
    }
}
```

### Step 3: JNI bridge

In `jni_corp.rs`:

```rust
#[no_mangle]
pub extern "system" fn Java_..._nativeSetCorpHeaders(
    mut env: JNIEnv, _cls: JClass, json: JString,
) {
    let raw: String = env.get_string(&json).map(|c| c.into()).unwrap_or_default();
    let headers: HashMap<String, String> = serde_json::from_str(&raw).unwrap_or_default();
    crate::corp_headers::set_headers(headers);
}
```

### Step 4: Java UI

In `GiteaServers`:

```java
/**
 * Corporate outbound headers in JSON format: {"X-Service-Name":"jenkins","X-Region":"eu"}
 * Applied to every Gitea API call made by the Rust client.
 */
private String corpOutboundHeaders = "{}";

// In configure():
try {
    RustGiteaConnection.nativeSetCorpHeaders(getCorpOutboundHeaders());
} catch (Throwable t) {
    LOGGER.log(Level.WARNING, "nativeSetCorpHeaders failed", t);
}
```

In `config.jelly`:

```xml
<f:entry title="${%Corporate outbound headers (JSON)}" field="corpOutboundHeaders">
    <f:textarea placeholder='{"X-Service-Name":"jenkins-prod"}'/>
</f:entry>
```

### Effort estimate

- Static headers: 1 hour
- Per-request UUID: 1.5 hours (need to wire through `client.rs` everywhere)

---

## 4. Header rewriting (rare)

**Use case:** Gitea sends `X-Forwarded-For: 10.0.0.1, 10.0.0.2` and the plugin should use only the last value for rate limiting.

**Strong recommendation:** do this in the reverse proxy (nginx/envoy), not in the plugin. Header rewriting in the plugin creates audit ambiguity and makes debugging harder.

If you MUST do it in the plugin:

```rust
// In server.rs, at the start of handle_webhook:
let real_ip = headers.get("x-forwarded-for")
    .and_then(|v| v.to_str().ok())
    .and_then(|s| s.split(',').last())
    .and_then(|s| s.trim().parse().ok())
    .unwrap_or(addr.ip());
```

This is a 10-min change but creates a security decision: which `X-Forwarded-For` value to trust. Document this clearly.

---

## 5. Audit log enrichment

**Use case:** Corporate requires every webhook event to be logged with `delivery_id`, `repo_full_name`, `event_type`, `source_ip`, and `latency_ms` to a separate JSON file for SIEM ingestion.

### Approach

This is a Java-side concern — add a `java.util.logging.Handler` (or custom audit sink) attached to the `org.jenkinsci.plugin.gitea.webhook.RustWebhookDispatcher` logger.

See [`examples/audit-sink.md`](./examples/audit-sink.md) for a complete template.

### Effort

1-2 hours.

---

## 6. Common pitfalls

### Pitfall 1: Header name case sensitivity

HTTP headers are case-insensitive, but `http::HeaderMap` (used by axum/reqwest) normalizes to lowercase. Always use lowercase when looking up:

```rust
// WRONG
headers.get("X-Corp-Signature")

// RIGHT
headers.get("x-corp-signature")
```

### Pitfall 2: Header value encoding

Header values must be ASCII. If your corp uses non-ASCII (rare), base64-encode it:

```rust
let decoded = base64::decode(value).map_err(|_| "invalid base64")?;
```

### Pitfall 3: Trusted header source

Never trust `X-Real-IP` or `X-Forwarded-For` without explicit configuration. If the corp reverse proxy adds these, document that the plugin trusts them ONLY when `Allowed CIDRs` includes the proxy IP.

### Pitfall 4: Header mutation breaks HMAC

If you read the body, then mutate a header, then verify HMAC — the HMAC will fail because it's computed over the body, not headers. Headers are separate from body in HMAC computation.

---

## 7. Testing header customizations

### Rust unit test

```rust
#[tokio::test]
async fn corp_signature_check_rejects_missing_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gitea-webhook/post"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    // ... build WebhookState with corp verifier, send request without X-Corp-Signature,
    // assert 401 response
}
```

### Java smoke test

```bash
# After deploy:
curl -X POST http://jenkins:8081/gitea-webhook/post \
    -H "X-Gitea-Event: push" \
    -H "X-Gitea-Signature: <valid-hmac>" \
    -H "X-Corp-Signature: <invalid-corp-sig>" \
    -d '{}'
# Should return 401 (corp signature invalid)
```

### Tools/smoke-test.sh

Extend `tools/smoke-test.sh` with a corp-header test case.

---

## 8. Reference: all inbound headers used by this plugin

| Header | Used for | Required? | Mutated? |
|---|---|---|---|
| `X-Gitea-Event` | Event routing | yes | no |
| `X-Gitea-Signature` | HMAC verification | if secret set | no |
| `X-Gitea-Delivery` | Idempotency dedup | recommended | no |
| `Authorization: Bearer <token>` | Optional bearer check | if bearer set | no |
| `Content-Type` | ignored (body read as Bytes) | no | no |
| `User-Agent` | logged at DEBUG for audit | no | no |
| Source IP (`ConnectInfo`) | IP CIDR + rate limit | always | no |

**None of these are mutated.** The pipeline is strictly read-only.

## 9. Reference: all outbound headers added by this plugin

| Header | Added by | When |
|---|---|---|
| `Authorization: token <T>` | `auth.rs` (Token variant) | Every API call with token auth |
| `Authorization: Basic <base64>` | `auth.rs` (Basic variant) | Every API call with basic auth |
| `User-Agent: gitea-client/0.1.0` | reqwest default | Every call |
| `Accept: application/json` | reqwest default | Every call |

Corporate headers (via `corp_headers.rs`) are added on top of these.
