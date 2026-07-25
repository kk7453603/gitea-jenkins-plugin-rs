# JNI Extensions — How to add new Rust↔Java bridges safely

This guide explains how to extend the plugin with **new** JNI exports without breaking the existing 41 exports. Read this if your corporate customization requires functionality that doesn't fit into the existing extension points (header pipeline, proxy routing, audit sinks).

---

## TL;DR decision tree

```
Need new functionality?
   │
   ├── Can it be done in Rust only (no new JNI)?
   │   └── YES → add new module in rust/gitea-client/src/, wire via existing entry points
   │
   ├── Can it be done in Java only (no new JNI)?
   │   └── YES → extend GiteaServers or add a filter class
   │
   ├── Needs NEW Java method calling NEW Rust function?
   │   └── YES → follow this guide
   │
   └── Needs to change EXISTING native method signature?
       └── STOP — this is a breaking change. Read ../AGENTS.md §6 first.
```

---

## 1. The 5-step safe pattern

Every new JNI bridge follows this exact pattern. Skipping any step breaks the contract.

### Step 1: Choose a JNI module

The plugin organizes JNI exports by responsibility:

| Module | Purpose | Add here if |
|---|---|---|
| `jni.rs` | Gitea HTTP API (35 exports) | You're adding a new Gitea API endpoint |
| `jni_webhook.rs` | Webhook server lifecycle (3 exports) | You're changing webhook start/stop |
| `jni_polling.rs` | Polling scheduler (2 exports) | You're changing polling behavior |
| `jni_log.rs` | Log bridge (1 export) | You're changing Rust→JUL forwarding |
| **NEW: `jni_corp.rs`** | Corporate extensions | Anything else — create this file |

**Rule:** never add corporate logic to `jni.rs`. Create `jni_corp.rs` instead.

### Step 2: Rust export

In `rust/gitea-client/src/jni_corp.rs`:

```rust
//! Corporate extension JNI exports.
//!
//! These exports are called from
//! `org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection` (or a new
//! Java class) and MUST keep their exact symbol names — renaming breaks
//! every caller.

use jni::objects::{JClass, JString};
use jni::sys::jint;
use jni::JNIEnv;

/// `Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeSetCorpToken`
///
/// Called from Java to install a corporate auth token. The token is stored
/// in a process-global `OnceLock` and consulted by the HTTP client on every
/// outbound request.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeSetCorpToken(
    mut env: JNIEnv,
    _cls: JClass,
    token: JString,
) {
    let token_str: String = match env.get_string(&token) {
        Ok(c) => c.into(),
        Err(e) => {
            tracing::warn!(error = %e, "nativeSetCorpToken: failed to decode token");
            return;
        }
    };
    // Store in OnceLock — same pattern as tls_store / proxy
    if let Err(_) = crate::corp_auth::set_token(token_str) {
        tracing::debug!("corp token already set — ignoring second call");
    }
}
```

### Step 3: Wire module in lib.rs

In `rust/gitea-client/src/lib.rs`:

```rust
pub mod corp_auth;  // new module with the actual logic
pub mod jni_corp;   // JNI exports
```

### Step 4: Java declaration

Add the native method to the appropriate Java class. Two choices:

**Choice A — extend `RustGiteaConnection`** (if the call affects HTTP API):

```java
// In RustGiteaConnection.java, near the other native declarations:
/**
 * Set the corporate auth token. Called from {@link GiteaServers#configure}
 * when the operator saves a corporate credentials configuration.
 */
public static native void nativeSetCorpToken(String token);
```

**Choice B — new Java class** (if the call is for webhook layer only):

```java
// In src/main/java/.../webhook/CorporateWebhookConfig.java
public class CorporateWebhookConfig {
    static {
        // No NativeLibraryLoader.load() here — RustGiteaConnection already
        // loaded the .so in its <clinit>. We just declare native methods.
    }
    public static native void nativeSetCorpHeader(String name, String value);
}
```

### Step 5: Caller

In `GiteaServers.configure()` (or wherever appropriate):

```java
try {
    RustGiteaConnection.nativeSetCorpToken(getCorpToken());
} catch (Throwable t) {
    LOGGER.log(Level.WARNING, "nativeSetCorpToken failed", t);
}
```

---

## 2. JNI naming convention

Symbol names follow this exact pattern:

```
Java_<package>_<Class>_<method>
```

Where:
- `<package>` = dots replaced with underscores: `org.jenkinsci.plugin.gitea.client.impl` → `org_jenkinsci_plugin_gitea_client_impl`
- `<Class>` = the Java class declaring the native method (without package prefix)
- `<method>` = the native method name

**Example:**
```java
package org.jenkinsci.plugin.gitea.client.impl;
public class RustGiteaConnection {
    private static native void nativeSetCorpToken(String token);
}
```
Maps to Rust symbol:
```rust
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeSetCorpToken(...)
```

**Gotchas:**
- Overloaded methods (same name, different args) get a `__1` suffix in some JNI versions — avoid overloading native methods
- Underscores in package/class/method names are escaped as `_1` — avoid underscores in native method names
- Native method names starting with `_` are reserved — start with `native` prefix or lowercase letter

---

## 3. Calling Java from Rust (JNI callback)

If your Rust code needs to call back into Java (e.g. corporate auth callback), follow the `GlobalRef` pattern from `jni_webhook.rs`:

```rust
use jni::objects::GlobalRef;
use std::sync::OnceLock;

static CORP_CALLBACK_CLASS: OnceLock<GlobalRef> = OnceLock::new();

#[no_mangle]
pub extern "system" fn Java_..._nativeRegisterCorpCallbackClass(
    mut env: JNIEnv, _cls: JClass, callback_class: JClass,
) {
    match env.new_global_ref(callback_class) {
        Ok(global) => {
            let _ = CORP_CALLBACK_CLASS.set(global);
        }
        Err(e) => tracing::error!(error = %e, "new_global_ref failed"),
    }
}

// Later, from a tokio worker thread:
fn invoke_corp_callback(event: &str) -> Result<(), String> {
    let Some(class) = CORP_CALLBACK_CLASS.get() else {
        return Err("CORP_CALLBACK_CLASS not registered".into());
    };
    let jvm = crate::runtime::get_jvm()?;  // You'll need to add this accessor
    let mut env = jvm.attach_current_thread().map_err(|e| e.to_string())?;
    let j_event = env.new_string(event).map_err(|e| e.to_string())?;
    env.call_static_method(
        class,
        "onCorpEvent",
        "(Ljava/lang/String;)V",
        &[(&j_event).into()],
    ).map_err(|e| e.to_string())?;
    Ok(())
}
```

**Critical:** Never use `env.find_class(...)` from tokio worker threads for plugin classes — the system ClassLoader can't see them. Always use `GlobalRef`.

---

## 4. Type mapping cheat sheet

| Java type | JNI type | Rust (`jni` crate) |
|---|---|---|
| `void` | `void` | return `()` |
| `boolean` | `jboolean` | `jni::sys::jboolean` (0 or 1) |
| `int` | `jint` | `i32` |
| `long` | `jlong` | `i64` |
| `String` | `jstring` | `JString` (convert via `env.get_string(&jstr)`) |
| `byte[]` | `jbyteArray` | `JByteArray` (convert via `env.convert_byte_array(&arr)`) |
| `Object` | `jobject` | `JObject` |
| `Class<?>` | `jclass` | `JClass` |
| return `String` | `jstring` | create via `env.new_string(s)?.into_raw()` |

**Signature string format:** `(args)return`
- `V` = void
- `I` = int
- `J` = long
- `Ljava/lang/String;` = String
- `[B` = byte[]

Example: `void handleEvent(String, String)` → `(Ljava/lang/String;Ljava/lang/String;)V`

---

## 5. Common pitfalls

### Pitfall 1: Mutability of `JNIEnv`

```rust
// WRONG:
pub extern "system" fn Java_...(env: JNIEnv, ...) {
    env.new_string("hi")?;  // fails to compile — env not mut
}

// RIGHT:
pub extern "system" fn Java_...(mut env: JNIEnv, ...) {
    env.new_string("hi")?;
}
```

### Pitfall 2: Lifetime of returned `JString`

```rust
// WRONG — jstring is dropped at end of inner scope
let jstr = env.new_string("hello")?;
let raw = jstr.into_raw();  // dangling after this function returns
raw  // returning raw ptr — caller will see garbage

// RIGHT — return owned jstring
let jstr = env.new_string("hello")?;
jstr.into_raw()  // converts to raw pointer, caller (JVM) owns it
```

### Pitfall 3: `find_class` from wrong thread

```rust
// WRONG — system ClassLoader can't see plugin classes
let class = env.find_class("org/jenkinsci/plugin/gitea/webhook/RustWebhookDispatcher")?;
// → ClassNotFoundException

// RIGHT — use GlobalRef set in <clinit>
let class = DISPATCHER_CLASS.get().expect("not registered");
```

### Pitfall 4: Returning null on error

```rust
// Acceptable for jstring/jobject returns, but Java must check
match result {
    Ok(s) => env.new_string(s)?.into_raw(),
    Err(e) => {
        throw_gitea_exception(&mut env, &e);
        std::ptr::null_mut()
    }
}
```

Java side: always check for null after native calls that can fail.

### Pitfall 5: Forgetting `#[no_mangle]`

```rust
// WRONG — symbol gets mangled, JVM can't find it
pub extern "system" fn Java_..._nativeXxx(...) { }

// RIGHT
#[no_mangle]
pub extern "system" fn Java_..._nativeXxx(...) { }
```

---

## 6. Testing JNI extensions

### Unit test (Rust only)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corp_token_storage_round_trip() {
        crate::corp_auth::set_token("test123".to_string()).unwrap();
        assert_eq!(crate::corp_auth::get_token().map(|s| s.as_str()), Some("test123"));
    }
}
```

### Symbol presence test

In `rust/gitea-client/tests/jni_symbols.rs`, add your new symbol:

```rust
const EXPECTED_SYMBOLS: &[&str] = &[
    // ... existing ...
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeSetCorpToken",
];
```

### Java smoke test

```java
@Test
public void corpTokenRoundTrip() {
    Assume.assumeTrue(nativeAvailable);
    RustGiteaConnection.nativeSetCorpToken("test-token");
    // (If you have a getter, assert it here)
}
```

---

## 7. Custom auth scheme (example)

For a corporate OAuth/JWT auth scheme:

1. Add new `Auth` variant in `rust/gitea-client/src/auth.rs`:
   ```rust
   pub enum Auth {
       None,
       Token(String),
       Basic { user: String, pass: String },
       CorpOAuth { token: String, refresh_url: String },  // NEW
   }
   ```

2. Implement `apply_to_request` for the new variant in `auth.rs`.

3. Add JNI export in `jni_corp.rs`:
   ```rust
   #[no_mangle]
   pub extern "system" fn Java_..._nativeSetCorpOAuth(
       mut env: JNIEnv, _cls: JClass,
       token: JString, refresh_url: JString,
   ) {
       // ... store in OnceLock
   }
   ```

4. Add `authType = 3` encoding in `RustGiteaConnection` constructor and matching `decode_auth` arm in `jni.rs`.

5. Add UI fields in `GiteaServers` + Jelly.

**Effort:** 3-4 hours if OAuth token refresh is not needed, 6-8 hours with refresh logic.

---

## 8. mTLS client cert (example)

For mutual TLS (Jenkins presents client cert to Gitea):

1. Add fields to `rust/gitea-client/src/tls.rs`:
   ```rust
   pub fn build_reqwest_client_with_client_cert(
       extra_pem: Option<&[u8]>,
       client_cert_pem: Option<&[u8]>,
       client_key_pem: Option<&[u8]>,
   ) -> Result<reqwest::Client, reqwest::Error> { ... }
   ```

2. Add `rustls::sign::CertifiedKey` construction from PEM.

3. JNI export `nativeSetClientCertificate(byte[] cert, byte[] key)`.

4. UI fields for client cert PEM and key PEM (use `<f:password/>` for key).

**Effort:** 2-3 hours. This is the most-requested corporate feature.

---

## 9. When NOT to add a JNI bridge

Some corporate needs are better solved without JNI:

| Need | Better solution |
|---|---|
| Add a new Gitea API endpoint | Just call existing `client.rs` pattern (Rust only) |
| Filter webhooks by repo name | Java-side filter in `RustWebhookDispatcher.dispatch()` |
| Transform webhook payload before dispatch | Java-side transform before `SCMEvent.fireNow()` |
| Custom rate limit formula | Extend `rate_limiter.rs` (Rust only) |
| Audit log to separate file | Java `FileHandler` for `RustLogReceiver` logger |

JNI is for cases where Java can't do the work (e.g. crypto, native libraries, performance-critical loops). For most corporate customizations, Java filters are simpler.
