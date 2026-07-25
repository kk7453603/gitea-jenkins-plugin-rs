# Example: Custom Auth Scheme (OAuth Token Refresh)

**Use case:** Corporate Gitea requires OAuth bearer tokens that expire every hour. The plugin must refresh the token automatically using a refresh token + client credentials.

**Time:** 4-6 hours.

---

## Architecture

```
GiteaServer config (clientId + clientSecret + initialToken)
        │
        ▼
RustGiteaConnection constructor
        │ stores auth = Auth::CorpOAuth { ... }
        ▼
client.rs request loop
        │
        ├─ Check token expiry
        │     │ expired?
        │     ▼
        │   refresh_token() → POST /oauth2/token
        │     │
        │     ▼ new token
        ├─ Apply Authorization: Bearer <token> header
        ▼
Send request
```

---

## Implementation

### 1. Rust: extend Auth enum

In `rust/gitea-client/src/auth.rs`:

```rust
pub enum Auth {
    None,
    Token(String),
    Basic { user: String, pass: String },
    CorpOAuth {
        client_id: String,
        client_secret: String,
        token_url: String,
        // Mutable state — wrapped in Arc<Mutex> for thread-safety
        current: Arc<Mutex<OAuthToken>>,
    },
}

pub struct OAuthToken {
    pub access_token: String,
    pub expires_at: Instant,
}

impl Auth {
    pub fn apply_to_request(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Auth::None => req,
            Auth::Token(t) => req.header("Authorization", format!("token {}", t)),
            Auth::Basic { user, pass } => req.basic_auth(user, Some(pass)),
            Auth::CorpOAuth { current, .. } => {
                let token = current.lock().unwrap();
                // Check expiry handled by caller (see client.rs)
                req.header("Authorization", format!("Bearer {}", token.access_token))
            }
        }
    }
}

impl Auth {
    /// Refresh the OAuth token if expired. Returns true if refreshed.
    pub async fn maybe_refresh(&self, http: &reqwest::Client) -> Result<bool, crate::error::GiteaError> {
        match self {
            Auth::CorpOAuth { client_id, client_secret, token_url, current } => {
                let needs_refresh = {
                    let token = current.lock().unwrap();
                    token.expires_at < Instant::now() + Duration::from_secs(30)
                };
                if !needs_refresh {
                    return Ok(false);
                }

                // POST /oauth2/token with client_credentials grant
                let resp = http.post(token_url)
                    .form(&[
                        ("grant_type", "client_credentials"),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                    ])
                    .send().await
                    .map_err(crate::error::GiteaError::Network)?;

                if !resp.status().is_success() {
                    return Err(crate::error::GiteaError::HttpStatus {
                        status: resp.status().as_u16(),
                        message: "OAuth refresh failed".into(),
                        body: Some(resp.text().await.unwrap_or_default()),
                    });
                }

                let body: serde_json::Value = resp.json().await
                    .map_err(|e| crate::error::GiteaError::Json(e.to_string()))?;

                let new_token = OAuthToken {
                    access_token: body["access_token"].as_str()
                        .ok_or_else(|| crate::error::GiteaError::Json("missing access_token".into()))?
                        .to_string(),
                    expires_at: Instant::now() + Duration::from_secs(
                        body["expires_in"].as_u64().unwrap_or(3600)
                    ),
                };

                {
                    let mut token = current.lock().unwrap();
                    *token = new_token;
                }

                tracing::info!("OAuth token refreshed");
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
```

### 2. Rust: client.rs integration

In `GiteaClient::execute_request` (or wherever requests are sent):

```rust
pub async fn fetch_repository(&self, owner: &str, repo: &str) -> Result<String, GiteaError> {
    // Refresh OAuth token if needed (before every request)
    self.auth.maybe_refresh(&self.http).await?;

    let url = self.base_url.join(&format!("repos/{}/{}", owner, repo))?;
    let req = self.http.get(url);
    let req = self.auth.apply_to_request(req);

    let resp = req.send().await?;
    // ... existing error handling + JSON extraction
}
```

### 3. JNI bridge

In `rust/gitea-client/src/jni_corp.rs`:

```rust
use jni::objects::{JClass, JString};
use jni::JNIEnv;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeSetCorpOAuth(
    mut env: JNIEnv,
    _cls: JClass,
    client_id: JString,
    client_secret: JString,
    token_url: JString,
    initial_token: JString,
    expires_in_secs: jni::sys::jlong,
) {
    let cid: String = env.get_string(&client_id).map(|c| c.into()).unwrap_or_default();
    let csec: String = env.get_string(&client_secret).map(|c| c.into()).unwrap_or_default();
    let turl: String = env.get_string(&token_url).map(|c| c.into()).unwrap_or_default();
    let itok: String = env.get_string(&initial_token).map(|c| c.into()).unwrap_or_default();
    let exp = if expires_in_secs > 0 {
        Duration::from_secs(expires_in_secs as u64)
    } else {
        Duration::from_secs(3600)
    };

    let initial = crate::auth::OAuthToken {
        access_token: itok,
        expires_at: Instant::now() + exp,
    };

    let auth = crate::auth::Auth::CorpOAuth {
        client_id: cid,
        client_secret: csec,
        token_url: turl,
        current: Arc::new(Mutex::new(initial)),
    };

    crate::auth::set_corp_oauth(auth);
}
```

### 4. Wire in auth.rs

Add a global slot for the corp OAuth auth (separate from per-call `Auth` passed to `GiteaClient::new`):

```rust
// In auth.rs:
use once_cell::sync::OnceLock;

static CORP_OAUTH: OnceLock<Auth> = OnceLock::new();

pub fn set_corp_oauth(auth: Auth) {
    let _ = CORP_OAUTH.set(auth);
}

pub fn corp_oauth() -> Option<&'static Auth> {
    CORP_OAUTH.get()
}
```

### 5. Java side

In `GiteaServers.java`:

```java
private String corpOAuthClientId = "";
private String corpOAuthClientSecret = "";
private String corpOAuthTokenUrl = "";
private String corpOAuthInitialToken = "";
private long corpOAuthInitialExpiresInSecs = 3600;

// getters + setters with @Restricted(NoExternalUse.class)
```

In `RustGiteaConnection.java`:

```java
public static native void nativeSetCorpOAuth(
    String clientId, String clientSecret,
    String tokenUrl, String initialToken, long expiresInSecs
);
```

In `GiteaServers.configure()`:

```java
try {
    RustGiteaConnection.nativeSetCorpOAuth(
        getCorpOAuthClientId(),
        getCorpOAuthClientSecret(),
        getCorpOAuthTokenUrl(),
        getCorpOAuthInitialToken(),
        getCorpOAuthInitialExpiresInSecs()
    );
} catch (Throwable t) {
    LOGGER.log(Level.WARNING, "nativeSetCorpOAuth failed", t);
}
```

### 6. UI

In `config.jelly`:

```xml
<f:advanced>
    <f:section title="${%Corporate OAuth}">
        <f:entry title="${%Client ID}" field="corpOAuthClientId">
            <f:textbox/>
        </f:entry>
        <f:entry title="${%Client Secret}" field="corpOAuthClientSecret">
            <f:password/>
        </f:entry>
        <f:entry title="${%Token URL}" field="corpOAuthTokenUrl">
            <f:textbox placeholder="https://gitea.corp/oauth2/token"/>
        </f:entry>
        <f:entry title="${%Initial access token}" field="corpOAuthInitialToken">
            <f:password/>
        </f:entry>
        <f:entry title="${%Initial token expires in (seconds)}" field="corpOAuthInitialExpiresInSecs">
            <f:number default="3600" clazz="positive-number"/>
        </f:entry>
    </f:section>
</f:advanced>
```

---

## Concurrency notes

The `OAuthToken` is wrapped in `Arc<Mutex<OAuthToken>>`. Refresh logic:

1. Check expiry (cheap, no refresh needed)
2. If expired, refresh (acquires lock, may block other threads)
3. Release lock, return token

**Lock contention:** only blocks during the actual HTTP refresh (~100ms). After refresh, all threads use the new token concurrently.

**Race condition prevention:** the lock ensures only one thread refreshes at a time. If 100 threads hit expiry simultaneously, only one refresh happens.

---

## Testing

```rust
#[tokio::test]
async fn oauth_refresh_on_expiry() {
    use std::time::{Duration, Instant};
    use wiremock::{MockServer, Mock, ResponseTemplate};

    let server = MockServer::start().await;

    // Token endpoint mock
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .respond_with(ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({
                "access_token": "refreshed-token",
                "expires_in": 3600,
            })))
        .mount(&server).await;

    let auth = Auth::CorpOAuth {
        client_id: "cid".into(),
        client_secret: "csec".into(),
        token_url: format!("{}/oauth2/token", server.uri()),
        current: Arc::new(Mutex::new(OAuthToken {
            access_token: "expired".into(),
            expires_at: Instant::now() - Duration::from_secs(1),  // already expired
        })),
    };

    let client = reqwest::Client::new();
    let refreshed = auth.maybe_refresh(&client).await.unwrap();
    assert!(refreshed);

    let token = auth.current.lock().unwrap();
    assert_eq!(token.access_token, "refreshed-token");
}
```

---

## Failure modes

| Failure | Behavior |
|---|---|
| Refresh endpoint unreachable | Request fails with `GiteaError::Network` |
| Refresh returns 401 (bad client creds) | Request fails with `GiteaError::HttpStatus { status: 401 }` |
| Refresh returns malformed JSON | Request fails with `GiteaError::Json` |
| Refresh returns no `access_token` | Request fails with `GiteaError::Json` |
| Token expires during in-flight request | Race — request may use old token, fail with 401 from Gitea. Next request triggers refresh. |

For production: add a retry layer that catches 401 from Gitea, triggers refresh, retries once.

---

## Security notes

- **Client secret in config.xml:** plaintext. Use Jenkins Credentials plugin or filesystem encryption.
- **Token in transit:** HTTPS only. `token_url` should always be `https://`.
- **Logging:** never log access_token. The example code does not, but be careful if you add debug logging.
- **Token URL validation:** consider validating that `token_url` matches expected corp hostname to prevent config injection attacks.
