//! JNI exports for the adaptive polling scheduler — stage 10.
//!
//! These `#[no_mangle]` functions are resolved by the JVM against the
//! Java class `org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection`,
//! which declares:
//!
//! ```java
//! public static native void nativeStartPolling(String configJson);
//! public static native void nativeStopPolling();
//! ```
//!
//! The JVM calls `nativeStartPolling` from `GiteaServers.configure()`
//! after the webhook/TLS/proxy setup, and `nativeStopPolling` when the
//! operator disables polling by setting the interval to 0.
//!
//! The argument to `nativeStartPolling` is the JSON encoding of
//! [`crate::polling::PollConfig`], assembled on the Java side.

use jni::objects::{JClass, JString};
use jni::JNIEnv;

/// `Java_…_RustGiteaConnection_nativeStartPolling` — start the polling
/// sweep loop with the supplied configuration.
///
/// Infallible at the JNI boundary: malformed JSON, missing JavaVM, or
/// invalid configuration are logged and silently ignored so that saving
/// the global Jenkins config never fails because of the polling layer.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeStartPolling(
    mut env: JNIEnv,
    _cls: JClass,
    config_json: JString,
) {
    let json: String = env
        .get_string(&config_json)
        .ok()
        .map(|c| c.into())
        .unwrap_or_default();
    if json.is_empty() {
        tracing::debug!("nativeStartPolling: empty config JSON, ignoring");
        return;
    }
    let config: crate::polling::PollConfig = match serde_json::from_str(&json) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "nativeStartPolling: invalid PollConfig JSON");
            return;
        }
    };
    let jvm = match env.get_java_vm() {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = ?e, "nativeStartPolling: get_java_vm failed");
            return;
        }
    };
    crate::polling::start(config, jvm);
}

/// `Java_…_RustGiteaConnection_nativeStopPolling` — stop the polling
/// sweep loop if one is running. Idempotent.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeStopPolling(
    _env: JNIEnv,
    _cls: JClass,
) {
    crate::polling::stop();
}
