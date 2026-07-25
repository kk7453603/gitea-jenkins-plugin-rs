# Example: Custom Header Injection (Inbound)

**Use case:** Corporate Gitea is behind a gateway that adds `X-Corp-Token: <secret>`. The plugin must verify this token on every webhook delivery.

**Time:** 30 minutes.

---

## Files to change

| File | Change |
|---|---|
| `rust/gitea-client/src/server.rs` | Add corp token check after HMAC verification |
| `rust/gitea-client/src/jni_webhook.rs` | Extend `nativeStart` with corp token arg |
| `src/main/java/.../servers/GiteaServers.java` | Add `webhookCorpToken` field |
| `src/main/java/.../webhook/RustWebhookDispatcher.java` | Pass corp token to `configure()` |
| `src/main/java/.../webhook/WebhookServerStarter.java` | Pass corp token |
| `src/main/resources/.../GiteaServers/config.jelly` | UI field |
| `rust/gitea-client/tests/webhook.rs` | Add test |

---

## Implementation

### 1. Rust: add corp token to WebhookState

In `rust/gitea-client/src/server.rs`, extend `WebhookState`:

```rust
#[derive(Clone)]
pub struct WebhookState {
    // ... existing fields ...
    pub corp_token: Option<Arc<String>>,
}
```

In `WebhookServer::start`, add parameter:

```rust
pub async fn start(
    port: u16,
    hmac_secret: Option<String>,
    bearer_token: Option<String>,
    allowed_cidrs: Vec<String>,
    rate_limit_per_minute: u32,
    path_prefix: Option<String>,
    corp_token: Option<String>,  // NEW
) -> std::io::Result<Self> {
    // ... existing code ...
    let corp = corp_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(Arc::new);

    let state = WebhookState {
        // ... existing ...
        corp_token: corp,
    };
    // ...
}
```

### 2. Rust: check in handler

In `handle_webhook`, after HMAC check:

```rust
// === CORPORATE TOKEN CHECK ===
if let Some(ref expected) = state.corp_token {
    let provided = headers.get("x-corp-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided != expected.as_str() {
        WEBHOOK_REQUESTS.with_label_values(&[event_type, "unauthorized"]).inc();
        return (StatusCode::UNAUTHORIZED, "missing or invalid X-Corp-Token").into_response();
    }
}
// === END CORPORATE CHECK ===
```

### 3. JNI bridge

In `rust/gitea-client/src/jni_webhook.rs`, extend `nativeStart`:

```rust
#[no_mangle]
pub extern "system" fn Java_..._nativeStart(
    mut env: JNIEnv,
    _cls: JClass,
    port: jint,
    hmac_secret: JString,
    bearer_token: JString,
    allowed_cidrs: JString,
    rate_limit_per_minute: jint,
    path_prefix: JString,
    corp_token: JString,  // NEW
) {
    // ... existing decoding ...
    let corp: Option<String> = env
        .get_string(&corp_token)
        .ok()
        .map(|c| c.into())
        .filter(|s: &String| !s.is_empty());

    // ... pass `corp` to WebhookServer::start ...
}
```

### 4. Java side

In `GiteaServers.java`:

```java
private String webhookCorpToken = "";

@Restricted(NoExternalUse.class)
public String getWebhookCorpToken() {
    return webhookCorpToken == null ? "" : webhookCorpToken;
}

@Restricted(NoExternalUse.class)
public void setWebhookCorpToken(String token) {
    this.webhookCorpToken = token == null ? "" : token;
}
```

In `RustWebhookDispatcher.java`:

```java
public static synchronized void configure(
        int port, String hmacSecret, String bearerToken,
        String allowedCidrs, int rateLimitPerMinute,
        String pathPrefix, String corpToken) {  // NEW arg
    // ... existing ...
    nativeStart(port, secret, bearer, cidrs, rateLimitPerMinute, path, corpToken);
}

private static native void nativeStart(
    int port, String hmacSecret, String bearerToken,
    String allowedCidrs, int rateLimitPerMinute,
    String pathPrefix, String corpToken  // NEW arg
);
```

In `WebhookServerStarter.java` and `GiteaServers.configure()`:

```java
RustWebhookDispatcher.configure(
    servers.getWebhookPort(),
    servers.getWebhookSecret(),
    servers.getWebhookBearerToken(),
    servers.getWebhookAllowedCidrs(),
    servers.getWebhookRateLimitPerMinute(),
    servers.getWebhookPath(),
    servers.getWebhookCorpToken()  // NEW
);
```

### 5. UI field

In `config.jelly`:

```xml
<f:entry title="${%Corporate webhook token (optional)}" field="webhookCorpToken">
    <f:password/>
</f:entry>
```

### 6. Test

In `rust/gitea-client/tests/webhook.rs`:

```rust
#[tokio::test]
async fn corp_token_check_rejects_missing_header() {
    let server = WebhookServer::start(
        0, None, None, vec![], 60, None,
        Some("corp-secret".to_string()),  // corp_token
    ).await.unwrap();

    let resp = reqwest::Client::new()
        .post(&format!("http://{}/gitea-webhook/post", server.local_addr()))
        .header("x-gitea-event", "push")
        .body("{}")
        .send().await.unwrap();
    assert_eq!(resp.status(), 401);  // missing X-Corp-Token
}

#[tokio::test]
async fn corp_token_check_accepts_valid_header() {
    // ... same but with .header("x-corp-token", "corp-secret") → 200
}
```

### 7. Verify

```bash
cd rust/gitea-client && cargo test && cd ../..
mvn -B compile test-compile -DskipTests -Dban-junit4-imports.skip=true -Dexec.skip=true -o
docker compose build
./tools/smoke-test.sh http://localhost:8081
```

---

## Security notes

- **Constant-time comparison:** the example uses `==` which is vulnerable to timing attacks. For a production deployment with high security requirements, use `subtle::ConstantTimeEq`:

  ```rust
  use subtle::ConstantTimeEq;
  if provided.as_bytes().ct_eq(expected.as_bytes()).into() { /* ok */ }
  ```

- **Token storage:** stored plaintext in `config.xml`. Use Jenkins filesystem encryption or a credentials vault if required.

- **Logging:** never log the token value. The example only logs "missing or invalid X-Corp-Token" without the value.
