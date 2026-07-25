//! JNI exports for managing the webhook server lifecycle — stage 9.A.
//!
//! These `#[no_mangle]` functions are resolved by the JVM against the
//! Java class `org.jenkinsci.plugin.gitea.webhook.RustWebhookDispatcher`,
//! which declares:
//!
//! ```java
//! private static native void nativeStart(
//!     int port,
//!     String hmacSecret,
//!     String bearerToken,
//!     String allowedCidrs,
//!     int rateLimitPerMinute
//! );
//! private static native void nativeStop();
//! ```
//!
//! The JVM calls `nativeStart` from `RustWebhookDispatcher.<init>` (or
//! an explicit `start()` method) once the plugin is loaded, and
//! `nativeStop` from `stop()` / Jenkins shutdown.
//!
//! ## Why an `Arc<Mutex<Option<…>>>` instead of `OnceLock`
//!
//! We need to be able to replace the server (start after stop, port
//! change) without reusing the same process. `OnceLock` only lets you
//! set once. A `Mutex<Option<WebhookServer>>` lets `nativeStop` take
//! the server out and shut it down, and a subsequent `nativeStart` can
//! put a new one in.
//!
//! ## Threading
//!
//! `nativeStart` calls into the shared tokio runtime via `RT.block_on`
//! because the Java caller is a synchronous thread. The HTTP server
//! itself runs in a background tokio task; only the bind and the spawn
//! happen on the calling thread.

use jni::objects::{GlobalRef, JClass, JString};
use jni::sys::jint;
use jni::JNIEnv;
use once_cell::sync::OnceCell;
use std::sync::{Arc, Mutex};

use crate::runtime::RT;
use crate::server::{set_java_callback, WebhookServer};

/// Global slot holding the currently-running server. `None` when the
/// server is stopped.
static SERVER: OnceCell<Mutex<Option<WebhookServer>>> = OnceCell::new();

fn server_slot() -> &'static Mutex<Option<WebhookServer>> {
    SERVER.get_or_init(|| Mutex::new(None))
}

/// Access the registered `RustWebhookDispatcher` class global reference.
/// Returns `None` if `nativeRegisterDispatcherClass` has not been called
/// yet (e.g. during cold start, or in unit tests without a JVM).
pub fn dispatcher_class() -> Option<&'static GlobalRef> {
    DISPATCHER_CLASS.get()
}

/// Global reference to `org.jenkinsci.plugin.gitea.webhook.RustWebhookDispatcher`
/// class, registered by Java-side `<clinit>` via `nativeRegisterDispatcherClass`.
///
/// **Why:** `JNIEnv::find_class` uses the *system* ClassLoader, but Jenkins
/// plugin classes live in the plugin ClassLoader. Without this global ref,
/// `find_class` throws `ClassNotFoundException` when invoked from a tokio
/// worker thread. The Java side passes its own `Class<?>` once at static-init
/// time, and we hold it as a `GlobalRef` for the lifetime of the JVM.
static DISPATCHER_CLASS: OnceCell<GlobalRef> = OnceCell::new();

/// The Java callback closure installed when the server starts. Kept in a
/// static so the HTTP handler (which is `Clone`) can reach it through
/// `crate::server::invoke_callback`.
static JAVA_CB_LOCK: OnceCell<Mutex<Option<Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>>>> =
    OnceCell::new();

fn java_cb_slot(
) -> &'static Mutex<Option<Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>>>
{
    JAVA_CB_LOCK.get_or_init(|| Mutex::new(None))
}

/// Build the JNI-backed callback that the HTTP handler invokes. The
/// closure captures a copy of the `JavaVM` (which is `Copy` — it is a
/// pointer) and re-attaches the calling tokio worker thread on every
/// webhook delivery.
fn make_jni_callback(
    jvm: jni::JavaVM,
) -> Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync> {
    Arc::new(move |event_type: &str, payload: &str| {
        // Each tokio worker thread has never seen the JVM before, so we
        // must attach it. `attach_current_thread` is cheap on a thread
        // that is already attached (it just bumps a refcount).
        let mut env = jvm
            .attach_current_thread()
            .map_err(|e| format!("jni attach: {}", e))?;

        // Use the plugin-classloader-resident global ref registered at
        // static-init time, instead of find_class (which goes through
        // the system ClassLoader and misses plugin classes).
        // jni-rs 0.21 implements `Desc<JClass>` for `&GlobalRef`.
        let class_ref = DISPATCHER_CLASS
            .get()
            .ok_or_else(|| "DISPATCHER_CLASS not registered — call nativeRegisterDispatcherClass first".to_string())?;

        let j_event_type = env
            .new_string(event_type)
            .map_err(|e| format!("new_string event_type: {}", e))?;
        let j_payload = env
            .new_string(payload)
            .map_err(|e| format!("new_string payload: {}", e))?;

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

/// `Java_…_RustWebhookDispatcher_nativeRegisterDispatcherClass` — receive
/// a global reference to the `RustWebhookDispatcher` class from Java's
/// static initializer. Must be called before `nativeStart`.
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
            } else {
                tracing::info!("DISPATCHER_CLASS registered for JNI callbacks");
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "nativeRegisterDispatcherClass: new_global_ref failed");
        }
    }
}

/// `Java_…_RustWebhookDispatcher_nativeStart` — start the webhook server.
///
/// Behaviour:
/// * `port == 0` lets the OS pick a port (useful for tests); the actual
///   port is logged and also reported back to Java via the (future)
///   `RustWebhookDispatcher.getPort()` accessor — for now, Java must
///   pass a concrete port number.
/// * `hmacSecret == null || hmacSecret.isEmpty()` disables HMAC
///   verification. A WARN-level log is emitted.
/// * `bearerToken == null || bearerToken.isEmpty()` disables the
///   optional bearer-token check (stage 16).
/// * `allowedCidrs` is a comma-separated list of CIDR strings
///   (e.g. `"10.0.0.0/8,192.168.0.0/16"`). Empty / null means "allow
///   all source IPs". Individual unparseable entries are skipped with
///   a WARN log; the rest still applies.
/// * `rateLimitPerMinute <= 0` is clamped to 1 inside the server so
///   each client can always send at least one probe.
/// * If a server is already running, it is shut down first (so a
///   Jenkins plugin reload does not leak listeners).
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_webhook_RustWebhookDispatcher_nativeStart(
    mut env: JNIEnv,
    _cls: JClass,
    port: jint,
    hmac_secret: JString,
    bearer_token: JString,
    allowed_cidrs: JString,
    rate_limit_per_minute: jint,
    path_prefix: JString,
) {
    let port_u16 = port.clamp(0, u16::MAX as jint) as u16;

    // Empty / null secret ⇒ None ⇒ verification disabled.
    let secret: Option<String> = env
        .get_string(&hmac_secret)
        .ok()
        .map(|c| c.into())
        .filter(|s: &String| !s.is_empty());

    // Empty / null bearer token ⇒ None ⇒ check disabled.
    let bearer: Option<String> = env
        .get_string(&bearer_token)
        .ok()
        .map(|c| c.into())
        .filter(|s: &String| !s.is_empty());

    // Parse the comma-separated CIDR list. Empty / null ⇒ empty Vec ⇒
    // "allow all" on the Rust side.
    let cidrs_raw: String = env
        .get_string(&allowed_cidrs)
        .ok()
        .map(|c| c.into())
        .unwrap_or_default();
    let cidr_list: Vec<String> = cidrs_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    // Clamp negative values to 0; the Rust side further clamps to >= 1.
    let rate_limit_u32 = if rate_limit_per_minute < 0 {
        0
    } else {
        rate_limit_per_minute as u32
    };

    // Optional path prefix override. Empty / null ⇒ None ⇒ Rust defaults
    // to "/gitea-webhook" (back-compat with v1.0).
    let prefix: Option<String> = env
        .get_string(&path_prefix)
        .ok()
        .map(|c| c.into())
        .filter(|s: &String| !s.is_empty());

    let jvm = match env.get_java_vm() {
        Ok(jvm) => jvm,
        Err(e) => {
            tracing::error!(
                error = %e,
                "nativeStart: failed to obtain JavaVM — aborting server start"
            );
            return;
        }
    };

    // Install the Java callback before starting the server so the first
    // incoming request is guaranteed to have a target.
    let cb = make_jni_callback(jvm);
    if let Ok(mut guard) = java_cb_slot().lock() {
        *guard = Some(cb.clone());
    }
    set_java_callback(cb);

    RT.block_on(async move {
        // If a previous server is somehow still alive, shut it down first.
        let previous = {
            let mut guard = match server_slot().lock() {
                Ok(g) => g,
                Err(_) => {
                    tracing::error!("nativeStart: server slot poisoned");
                    return;
                }
            };
            guard.take()
        };
        if let Some(mut prev) = previous {
            tracing::info!("nativeStart: shutting down previous webhook server");
            prev.shutdown().await;
        }

        match WebhookServer::start(
            port_u16,
            secret,
            bearer,
            cidr_list,
            rate_limit_u32,
            prefix,
        )
        .await
        {
            Ok(server) => {
                tracing::info!(
                    port = %port_u16,
                    actual_addr = %server.local_addr(),
                    "nativeStart: webhook server started"
                );
                if let Ok(mut guard) = server_slot().lock() {
                    *guard = Some(server);
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    port = %port_u16,
                    "nativeStart: failed to bind webhook server"
                );
            }
        }
    });
}

/// `Java_…_RustWebhookDispatcher_nativeStop` — stop the webhook server.
///
/// Idempotent: calling `nativeStop` when no server is running is a
/// no-op.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_webhook_RustWebhookDispatcher_nativeStop(
    _env: JNIEnv,
    _cls: JClass,
) {
    RT.block_on(async {
        let server = {
            let mut guard = match server_slot().lock() {
                Ok(g) => g,
                Err(_) => {
                    tracing::error!("nativeStop: server slot poisoned");
                    return;
                }
            };
            guard.take()
        };
        if let Some(mut server) = server {
            server.shutdown().await;
            tracing::info!("nativeStop: webhook server stopped");
        } else {
            tracing::debug!("nativeStop: no server running (no-op)");
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_slot_is_lockable() {
        // Smoke test: ensure the OnceCell initialises and the Mutex is
        // usable. We don't actually put a server in (it would need a
        // running tokio runtime within the test, which we have via RT
        // but we want to keep this test synchronous).
        let lock = server_slot();
        let guard = lock.lock().unwrap();
        assert!(guard.is_none(), "server slot should be empty by default");
    }

    #[test]
    fn java_cb_slot_is_lockable() {
        let lock = java_cb_slot();
        let guard = lock.lock().unwrap();
        assert!(guard.is_none(), "java callback slot should be empty by default");
    }
}
