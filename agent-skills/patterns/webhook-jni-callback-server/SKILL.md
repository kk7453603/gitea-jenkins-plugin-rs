---
name: webhook-jni-callback-server
description: HTTP server (axum) в Rust-ядре плагина, который принимает webhooks и колбэчит в JVM через JNI. GlobalRef на JClass вместо find_class (system ClassLoader не видит plugin classes), attach_current_thread на каждый tokio worker, OnceLock для dispatcher class. Применять когда нужен Rust-side HTTP listener с Java callback.
origin: gitea-jenkins-plugin-rs v1.1.0
tags: [rust, jni, axum, webhook, http-server, globlref, classloader, tokio]
---

# Webhook server с JNI callback в Jenkins

## Когда применять

- Нужен HTTP listener внутри Rust-ядра Jenkins-плагина (отдельный порт)
- Webhook payload должен дойти до Jenkins `SCMHeadEvent.fireNow()` через JNI callback
- `env.find_class(...)` падает с `ClassNotFoundException` из tokio worker thread
- Нужен pipeline: IP allowlist → rate limit → bearer → HMAC → dispatch

## Паттерн

### Архитектура

```
Gitea server
   │
   │ POST /gitea-webhook/post с X-Gitea-Signature + X-Gitea-Event + body
   ▼
Rust axum server (:8081)
   │
   │ Pipeline:
   │   1. IP allowlist check (CIDR)
   │   2. Rate limit (token bucket per IP)
   │   3. Bearer token check (опционально)
   │   4. HMAC-SHA256 verify body
   │   5. Read X-Gitea-Event header
   │
   ▼  JNI callback
RustWebhookDispatcher.handleEvent(String type, String json)
   │ (Java-side, plugin ClassLoader)
   ▼
parseObject(json, GiteaXxxEvent.class)
   │
   ▼
SCMHeadEvent.fireNow(...)  →  Jenkins SCM bus
```

### `GlobalRef` вместо `find_class` — критично

System ClassLoader в JVM не видит plugin classes. Если из tokio worker вызвать `env.find_class("org/jenkinsci/plugin/gitea/webhook/RustWebhookDispatcher")` — получите `ClassNotFoundException`.

Решение: Java-side `<clinit>` регистрирует `Class<?>` через отдельный native method, Rust хранит `GlobalRef` в `OnceLock`:

```rust
// jni_webhook.rs

use jni::objects::{GlobalRef, JClass};
use once_cell::sync::OnceCell;

static DISPATCHER_CLASS: OnceCell<GlobalRef> = OnceCell::new();

/// Вызывается из RustWebhookDispatcher.<clinit> с RustWebhookDispatcher.class
/// как аргументом. Мы берём global ref и храним его process-wide.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_webhook_RustWebhookDispatcher_nativeRegisterDispatcherClass(
    mut env: JNIEnv,
    _cls: JClass,
    dispatcher_class: JClass,
) {
    match env.new_global_ref(dispatcher_class) {
        Ok(global) => {
            if DISPATCHER_CLASS.set(global).is_err() {
                tracing::debug!("DISPATCHER_CLASS already set (plugin reload) — keeping original");
            }
        }
        Err(e) => tracing::error!(error = %e, "new_global_ref failed"),
    }
}

/// Доступ к классу из любого tokio worker thread.
pub fn dispatcher_class() -> Option<&'static GlobalRef> {
    DISPATCHER_CLASS.get()
}
```

Java-side `<clinit>`:

```java
// RustWebhookDispatcher.java
static {
    NativeLibraryLoader.load("gitea_rust");
    nativeRegisterDispatcherClass(RustWebhookDispatcher.class);  // ← передаём Class
    nativeInstallLogBridge();
}

private static native void nativeRegisterDispatcherClass(Class<?> cls);
```

### `attach_current_thread` на каждый запрос

Каждый tokio worker thread — новый для JVM. Дешёвый `attach_current_thread` (если уже attached — просто refcount bump):

```rust
// JNI callback闭ture, вызывается из axum handler на tokio worker
fn make_jni_callback(jvm: jni::JavaVM) -> Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync> {
    Arc::new(move |event_type: &str, payload: &str| {
        // 1. Прикрепляем текущий tokio worker thread к JVM.
        let mut env = jvm
            .attach_current_thread()
            .map_err(|e| format!("jni attach: {}", e))?;

        // 2. Берём GlobalRef, зарегистрированный при <clinit>.
        let class_ref = DISPATCHER_CLASS
            .get()
            .ok_or_else(|| "DISPATCHER_CLASS not registered".to_string())?;

        // 3. Создаём jstring-и для аргументов.
        let j_event_type = env.new_string(event_type)
            .map_err(|e| format!("new_string event_type: {}", e))?;
        let j_payload = env.new_string(payload)
            .map_err(|e| format!("new_string payload: {}", e))?;

        // 4. Вызываем статический метод handleEvent(String, String).
        // jni-rs 0.21 implements Desc<JClass> for &GlobalRef.
        env.call_static_method(
            class_ref,
            "handleEvent",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[(&j_event_type).into(), (&j_payload).into()],
        )
        .map_err(|e| format!("call_static_method handleEvent: {}", e))?;

        Ok(())
    })
}
```

### Lifecycle: nativeStart/nativeStop

Server хранится в `Arc<Mutex<Option<WebhookServer>>>` (НЕ `OnceLock`, потому что нужно заменить):

```rust
static SERVER: OnceCell<Mutex<Option<WebhookServer>>> = OnceCell::new();

fn server_slot() -> &'static Mutex<Option<WebhookServer>> {
    SERVER.get_or_init(|| Mutex::new(None))
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_webhook_RustWebhookDispatcher_nativeStart(
    mut env: JNIEnv,
    _cls: JClass,
    port: jint,
    hmac_secret: JString,
    bearer_token: JString,
    allowed_cidrs: JString,
    rate_limit_per_minute: jint,
) {
    let port_u16 = port.clamp(0, u16::MAX as jint) as u16;
    // ... decode args ...

    let jvm = env.get_java_vm().expect("JavaVM");

    // Установка callback ДО старта server-а — чтобы первый запрос имел target.
    let cb = make_jni_callback(jvm);
    set_java_callback(cb.clone());

    RT.block_on(async move {
        // Если предыдущий server ещё жив — shutdown его сначала.
        let previous = server_slot().lock().unwrap().take();
        if let Some(mut prev) = previous {
            prev.shutdown().await;
        }

        match WebhookServer::start(port_u16, secret, bearer, cidr_list, rate_limit_u32).await {
            Ok(server) => {
                *server_slot().lock().unwrap() = Some(server);
            }
            Err(e) => tracing::error!(error = %e, "failed to bind"),
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_webhook_RustWebhookDispatcher_nativeStop(
    _env: JNIEnv,
    _cls: JClass,
) {
    RT.block_on(async {
        let server = server_slot().lock().unwrap().take();
        if let Some(mut server) = server {
            server.shutdown().await;
        }
    });
}
```

### Axum handler pipeline

`server.rs` определяет axum router с `WebhookState`:

```rust
#[derive(Clone)]
pub struct WebhookState {
    pub hmac_secret: Option<Arc<String>>,    // None = skip verify
    pub bearer_token: Option<Arc<String>>,   // None = skip check
    pub allowed_cidrs: Arc<Vec<IpCidr>>,     // empty = allow all
    pub rate_limiter: Arc<RateLimiter>,      // per-IP token bucket
}

async fn webhook_handler(
    State(state): State<WebhookState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // 1. IP allowlist
    if !state.allowed_cidrs.is_empty()
        && !state.allowed_cidrs.iter().any(|c| c.contains(&addr.ip())) {
        return (StatusCode::FORBIDDEN, "forbidden");
    }

    // 2. Rate limit
    if !state.rate_limiter.acquire(addr.ip()) {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limited");
    }

    // 3. Bearer token (опционально)
    if let Some(ref expected) = state.bearer_token {
        let got = headers.get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));
        if got != Some(expected.as_str()) {
            return (StatusCode::UNAUTHORIZED, "bad bearer");
        }
    }

    // 4. HMAC verify
    if let Some(ref secret) = state.hmac_secret {
        let got = headers.get("x-gitea-signature").and_then(|v| v.to_str().ok());
        if !verify_hmac(secret, &body, got) {
            return (StatusCode::UNAUTHORIZED, "bad signature");
        }
    }

    // 5. Read event type
    let event_type = headers.get("x-gitea-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    // 6. JNI callback в JVM
    match invoke_callback(event_type, &String::from_utf8_lossy(&body)) {
        Ok(()) => (StatusCode::OK, "ok"),
        Err(e) => {
            tracing::error!(error = %e, "callback failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "callback failed")
        }
    }
}
```

### Health + metrics endpoints

```rust
// GET /gitea-webhook/health — для k8s readiness/liveness probe
async fn health() -> impl IntoResponse {
    (StatusCode::OK, [("Content-Type", "application/json")], r#"{"status":"ok"}"#)
}

// GET /gitea-webhook/metrics — Prometheus text format
async fn metrics() -> impl IntoResponse {
    let encoder = prometheus::TextEncoder::new();
    let body = encoder.encode_to_string(&prometheus::gather()).unwrap_or_default();
    (StatusCode::OK, [("Content-Type", "text/plain; version=0.0.4")], body)
}
```

### Java-side: `RustWebhookDispatcher`

```java
@Extension
public class RustWebhookDispatcher {
    private static final ObjectMapper MAPPER = new ObjectMapper();

    static {
        NativeLibraryLoader.load("gitea_rust");
        nativeRegisterDispatcherClass(RustWebhookDispatcher.class);
        nativeInstallLogBridge();
    }

    // Вызывается из Rust через JNI (static method).
    public static void handleEvent(String type, String json) {
        try {
            switch (type) {
                case "push":
                    GiteaPushEvent ev = MAPPER.readValue(json, GiteaPushEvent.class);
                    new GiteaPushSCMEvent(ev, ORIGIN).fireNow();
                    break;
                case "pull_request":
                    GiteaPullRequestEvent ev = MAPPER.readValue(json, GiteaPullRequestEvent.class);
                    new GiteaPullSCMEvent(ev, ORIGIN).fireNow();
                    break;
                // ... create, delete, release, repository ...
            }
        } catch (IOException e) {
            LOGGER.log(Level.SEVERE, "failed to parse webhook payload", e);
        }
    }

    private static native void nativeRegisterDispatcherClass(Class<?> cls);
    private static native void nativeInstallLogBridge();
    private static native void nativeStart(int port, String hmac, String bearer,
                                            String cidrs, int rateLimit);
    private static native void nativeStop();
}
```

## Подводные камни

1. **`find_class` НЕ работает из tokio worker.** System ClassLoader ≠ plugin ClassLoader. Только `GlobalRef` через `nativeRegisterDispatcherClass`. Это №1 источник багов в этом паттерне.
2. **`OnceLock` для `DISPATCHER_CLASS` — first-call wins.** При plugin reload Jenkins не перезагружает `.so` (см. AGENTS.md "Hot-reload не поддерживается"). Старый `GlobalRef` остаётся — он указывает на старый Class, но это OK потому что все Class-ы — plugin-singleton.
3. **`attach_current_thread` дешев, но не бесплатен.** На каждый webhook запрос — attach. Если 100 req/sec, это ~ms overhead. Решение — кэшировать attach в thread-local, но jni-rs 0.21 делает это под капотом.
4. **`OnceLock` для callback.** Server заменили — callback нужно тоже заменить. Но `OnceLock::set` не перезаписывает. Решение — `JAVA_CB_LOCK` через `Mutex` (см. `jni_webhook.rs::java_cb_slot`).
5. **Pipeline порядок.** IP allowlist → rate limit → bearer → HMAC. HMAC — самый дорогой (SHA256 over body), поэтому последний. IP и rate limit — O(1) проверки, идут первыми.
6. **`X-Gitea-Event` header.** Gitea шлёт lowercase (`"push"`, `"pull_request"`). Java-side switch-case тоже lowercase. Не кодируйте camelCase.
7. **Body — bytes, не string.** HMAC считается над raw bytes. Если декодировать в String и обратно — можно получить другие bytes (encoding artifacts). `axum::body::Bytes` сохраняет raw body.
8. **`ConnectInfo<SocketAddr>` — требует `into_make_service_with_connect_info`.** При `axum::serve` нужно явно:
   ```rust
   axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
   ```
   Иначе `ConnectInfo` extraction падает с 500.
9. **`SCMHeadEvent.fireNow()` — synchronous?** Нет, асинхронный. Он кладёт event в queue, Jenkins обрабатывает в отдельном потоке. Поэтому Rust-side callback возвращается быстро, не блокируя webhook response.
10. **Recursion risk в log_bridge.** `tracing::error!` из callback → forward_to_java → `call_static_method(RustLogReceiver.handleLog)` → если тот сам логирует через JUL → tracing может рекурсивно сработать. Решение — `forward_to_java` игнорирует все ошибки тихо, не логируя их через tracing.
11. **`@Extension` аннотация.** Без неё Jenkins не подхватит класс, и `<clinit>` не сработает при старте. Это критично — без `@Extension` server вообще не стартует.
12. **`WebhookServerStarter.doExecute`.** Это `AsyncPeriodicWork`, который Jenkins запускает на старте. Он читает конфиг из `GiteaServers` и вызывает `RustWebhookDispatcher.configure(port, ...)`. Без него server не запустится автоматически.

## Файлы-референсы

- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/server.rs` — axum server, pipeline, JNI callback
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/jni_webhook.rs` — `nativeStart/nativeStop/nativeRegisterDispatcherClass`, `GlobalRef` + `OnceLock`, `attach_current_thread` callback closure
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/log_bridge.rs` — tracing→JUL bridge (отдельный skill: `tracing→java.util.logging`)
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/rate_limiter.rs` — per-IP token bucket
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/webhook/RustWebhookDispatcher.java` — Java-side `handleEvent(type, json)`, `@Extension`, `<clinit>` с `nativeRegisterDispatcherClass`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/webhook/RustLogReceiver.java` — Java-side приёмник логов от Rust
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/webhook/WebhookServerStarter.java` — `AsyncPeriodicWork`, который стартует server на Jenkins boot
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/docker-compose.yml` — `:8081` порт exposed
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/patterns/jni-bridge-generator/SKILL.md` — базовый JNI naming + хелперы
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/security/security-review/SKILL.md` — security checklist для webhook layer (HMAC, IP allowlist, rate limit)
