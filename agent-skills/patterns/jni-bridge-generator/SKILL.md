---
name: jni-bridge-generator
description: Генерация JNI-bridge между Java и Rust-ядром — naming convention для extern "system" fn, хелперы для jstring/jboolean/jbyteArray/throw, тесты через libloading. Применять когда нужно добавить новый native-метод в плагин, написать новый `#[no_mangle] pub extern "system" fn`, или когда попался `UnsatisfiedLinkError` / `ClassNotFoundException` / `pending exception` из JNI.
origin: gitea-jenkins-plugin-rs v1.1.0
tags: [rust, jni, java, native, libloading, naming-convention]
---

# JNI-bridge: шаблон для export-функций из Rust в JVM

## Когда применять

- Нужно добавить новый `private static native` метод в `RustGiteaConnection` или `RustWebhookDispatcher`
- Видите ошибку `java.lang.UnsatisfiedLinkError: <method name>` — кто-то нарушил naming convention
- Видите `ClassNotFoundException` из JNI-колбэка — используйте `find_class` вместо зарегистрированного `GlobalRef`
- Новая async-функция в `client.rs` должна быть экспортирована в JVM
- Тестируете, что `.so` содержит ожидаемые символы

## Паттерн

### Naming convention — стандарт JNI

JVM резолвит symbol по имени:

```
Java_<package_with_dots_replaced_by_underscores>_<Class>_<methodName>
```

Для нашего пакета `org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection` и метода `nativeFetchVersion` символ:

```
Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchVersion
```

Объявление в Rust — всегда `#[no_mangle] pub extern "system" fn`:

```rust
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchVersion(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
) -> jstring {
    // ...
}
```

`extern "system"` — это JNI calling convention на текущей платформе (на Linux/macOS совпадает с C ABI, на Windows — `__stdcall` для 32-bit).

### Каркас для возвращающего JSON метода

Большинство export-ов — это decode-args → `RT.block_on(async { ... })` → match Ok/Err:

```rust
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchRepository(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let owner = match jstr(&mut env, owner) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let repo = match jstr(&mut env, repo) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_repository(&owner, &repo).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}
```

### Хелперы (фактически стандартные для любого JNI-bridge)

```rust
// Декодируем JString → String. Err если JVM не может отдать UTF-8 (e.g. OOM).
fn jstr(env: &mut JNIEnv, s: JString) -> Result<String, jni::errors::Error> {
    env.get_string(&s).map(|c| c.into())
}

// То же, но пустая строка вместо Err — для опционального auth_secret.
fn jstr_or_empty(env: &mut JNIEnv, s: JString) -> String {
    jstr(env, s).unwrap_or_default()
}

// JSON-строка → jstring. Null если new_string упал (JVM увидит pending exception).
fn json_to_jstring(env: &mut JNIEnv, json: String) -> jstring {
    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// Vec<u8> → jbyteArray — для binary-content (например fetchFile).
fn bytes_to_jbytearray(env: &mut JNIEnv, bytes: Vec<u8>) -> jbyteArray {
    match env.byte_array_from_slice(&bytes) {
        Ok(arr) => arr.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// (serverUrl, authType, authSecret) → GiteaClient — общий код для всех export-ов.
fn build_client(server_url: &str, auth_type: jint, secret: &str) -> Result<GiteaClient, GiteaError> {
    let auth = decode_auth(auth_type, secret);
    GiteaClient::new(server_url, auth)
}

// Auth encoded Java-side как (authType: int, authSecret: String):
//   0 = None, 1 = Token(secret), 2 = Basic("user:pass")
pub fn decode_auth(auth_type: jint, secret: &str) -> Auth {
    match auth_type {
        1 => Auth::Token(secret.to_string()),
        2 => {
            let (user, pass) = secret
                .split_once(':')
                .map(|(u, p)| (u.to_string(), p.to_string()))
                .unwrap_or_else(|| (secret.to_string(), String::new()));
            Auth::Basic { user, pass }
        }
        _ => Auth::None,
    }
}
```

### Маппинг ошибок в Java exception

JNI convention: native method должен `throw_new` и вернуть null/0/sentinel. На Java-стороне exception "pending", и при следующем JNI-вызове JVM его активирует:

```rust
fn throw_gitea_exception(env: &mut JNIEnv, err: &GiteaError) {
    let (class_name, msg) = match err {
        GiteaError::HttpStatus { status, message, body } => (
            "org/jenkinsci/plugin/gitea/client/api/GiteaHttpStatusException",
            format!("HTTP {}/{}{}", status, message,
                body.as_deref().map(|b| format!("\n{}", b)).unwrap_or_default()),
        ),
        GiteaError::FileNotFound(path) => (
            "java/io/FileNotFoundException",
            format!("Not found: {}", path),
        ),
        GiteaError::Network(e) => ("java/io/IOException", format!("network error: {}", e)),
        GiteaError::Url(e) => (
            "java/net/MalformedURLException",
            format!("invalid URL: {}", e),
        ),
        GiteaError::Json(e) => ("java/io/IOException", format!("JSON error: {}", e)),
        GiteaError::Io(e) => ("java/io/IOException", format!("io error: {}", e)),
    };
    let _ = env.throw_new(class_name, &msg);
}
```

### Async → синхронный через `RT.block_on`

Java-сторона синхронная (как upstream `DefaultGiteaConnection`), Rust — async. Мост — глобальный tokio runtime:

```rust
// runtime.rs — Lazy<tokio::Runtime>, 1 на process.
pub static RT: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
});
```

Каждый JNI export блокирует вызывающий JVM-поток на `RT.block_on(async { ... })`. Это OK для типичного Gitea API вызова (десятки мс), но не делайте долгих polling-циклов внутри JNI export-а — для polling-а есть отдельный фоновый task в `runtime.rs`.

### Тесты символов через `libloading`

`tests/jni_symbols.rs` — контракт между Java `private native` декларациями и Rust `#[no_mangle]`:

```rust
const EXPECTED_SYMBOLS: &[&str] = &[
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchVersion",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCurrentUser",
    // ... 35 штук для jni.rs, 3 для jni_webhook.rs, и т.д.
];

#[test]
fn jni_exports_are_present() {
    let lib = unsafe { Library::new(native_lib_path().unwrap()).unwrap() };
    for symbol in EXPECTED_SYMBOLS {
        unsafe {
            let sym: libloading::Symbol<unsafe extern "system" fn()> = lib
                .get(symbol.as_bytes())
                .unwrap_or_else(|e| panic!("missing JNI export `{}`: {}", symbol, e));
            assert!(!(*sym as *const ()).is_null());
        }
    }
}
```

Тест **не запускает JVM** — это просто `dlopen` + `dlsym`. Запускается под `cargo test` без Jenkins.

### Структура exports в проекте

| Файл | Что | Кол-во |
|---|---|---|
| `rust/gitea-client/src/jni.rs` | HTTP API методы Gitea | 35 |
| `rust/gitea-client/src/jni_webhook.rs` | Lifecycle webhook server | 3 |
| `rust/gitea-client/src/jni_polling.rs` | Lifecycle polling scheduler | 2 |
| `rust/gitea-client/src/jni_log.rs` | Tracing→JUL bridge install | 1 |

## Подводные камни

1. **Overloaded native methods.** Если в Java есть два `native foo(String)` и `native foo(int)`, JNI требует суффикс `__1` плюс signature: `Java_pkg_Class_foo__Ljava_lang_String_2` и `Java_pkg_Class_foo__I`. Решение: **не делайте overloaded native методы** — давайте уникальные имена.
2. **`JNIEnv` не thread-safe.** `JNIEnv` валиден только на вызывающем потоке. Если запускаете фоновый tokio-task и хотите колбэк в JVM — используйте `JavaVM::attach_current_thread()` (см. `jni_webhook.rs`).
3. **`find_class` из tokio worker-а не видит plugin classes.** System ClassLoader ≠ plugin ClassLoader. Решение: Java-side `<clinit>` регистрирует `Class<?>` через отдельный native-метод (`nativeRegisterDispatcherClass`), Rust хранит `GlobalRef` в `OnceLock`. См. `webhook-jni-callback-server/SKILL.md`.
4. **Pending exception при return null.** После `throw_new` обязателно верните null/0/false. Иначе JVM вызовет следующий JNI-method с pending exception и упадёт с `FatalError`.
5. **`jboolean` — это `u8`, не `bool`.** 0 = false, всё остальное = true. При возврате явно пишите `JNI_TRUE = 1`, `JNI_FALSE = 0` — не кастуйте `bool as jboolean` (некоторые компиляторы дают !=1 значения).
6. **`jlong` — всегда 64-bit.** Rust-side `i64`. Не путайте с `jint` (32-bit). Для Gitea PR-number используйте `jlong` (Gitea шлёт `number` как int64_t).
7. **Async внутри `RT.block_on`.** Не создавайте свой `tokio::runtime::Runtime` внутри export-а — будет panic "cannot start a runtime from within a runtime". Используйте `crate::runtime::RT`.
8. **Dead-code elimination.** Без `#[no_mangle]` symbol может быть выкинут линкером. `crate-type = ["cdylib"]` в `Cargo.toml` обязателен.
9. **JByteArray → Vec<u8> через `convert_byte_array`.** В обратную сторону — `byte_array_from_slice`. Не путайте с `byte_array_region` (это для частичного заполнения).
10. **`jstr_or_empty` для опциональных аргументов.** Если Java передаёт `null` для опционального `state`-фильтра, Rust получает пустую строку — переводим её в `Option::<&str>::None`. См. `nativeFetchPullRequests` в `jni.rs`.

## Файлы-референсы

- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/jni.rs` — 35 export-ов + все хелперы + `decode_auth` + `throw_gitea_exception`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/jni_webhook.rs` — `nativeStart`/`nativeStop`/`nativeRegisterDispatcherClass`, `OnceLock<GlobalRef>` паттерн
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/jni_polling.rs` — `nativeStartPolling`/`nativeStopPolling` (JSON config как аргумент)
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/jni_log.rs` — `nativeInstallLogBridge` (1 export, устанавливает tracing-bridge)
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/runtime.rs` — `RT: Lazy<Runtime>` общий для всех export-ов
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/tests/jni_symbols.rs` — контракт-тест через `libloading`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/client/impl/RustGiteaConnection.java` — Java-side декларации `private static native`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/patterns/json-over-jni-bridge/SKILL.md` — смежный паттерн про JSON-контракт
