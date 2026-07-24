//! JNI export for installing the Rust→Jenkins log bridge.
//!
//! `Java_..._RustWebhookDispatcher_nativeInstallLogBridge` is called from
//! `RustWebhookDispatcher.<clinit>` (after `nativeRegisterDispatcherClass`)
//! to wire the tracing layer into the global subscriber.

use jni::objects::{GlobalRef, JClass};
use jni::JNIEnv;

/// `nativeInstallLogBridge()` — install the tracing→JUL bridge.
///
/// On the Rust side this captures the calling thread's `JavaVM` and a
/// global ref to the `RustLogReceiver` class (passed implicitly as the
/// first static native method's declaring class), then registers a
/// `LogBridgeLayer` with the global tracing subscriber. Every
/// `tracing::info!/warn!/error!` event emitted after this call is
/// forwarded to `RustLogReceiver.handleLog(level, target, message)`.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_webhook_RustWebhookDispatcher_nativeInstallLogBridge(
    mut env: JNIEnv,
    cls: JClass,
) {
    let jvm = match env.get_java_vm() {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(error = %e, "nativeInstallLogBridge: get_java_vm failed");
            return;
        }
    };
    // Take a global ref to RustLogReceiver so the layer can resolve it
    // from any tokio worker thread without find_class (which uses the
    // system ClassLoader and misses plugin classes).
    let receiver_class_path = "org/jenkinsci/plugin/gitea/webhook/RustLogReceiver";
    let receiver_class = match env.find_class(receiver_class_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                error = %e,
                class = receiver_class_path,
                "nativeInstallLogBridge: find_class RustLogReceiver failed"
            );
            return;
        }
    };
    let global: GlobalRef = match env.new_global_ref(receiver_class) {
        Ok(g) => g,
        Err(e) => {
            tracing::error!(error = %e, "nativeInstallLogBridge: new_global_ref failed");
            return;
        }
    };
    // The JClass `cls` (RustWebhookDispatcher) is unused here — we look up
    // RustLogReceiver explicitly because that's where handleLog lives.
    let _ = cls;

    crate::log_bridge::install_log_bridge(global, jvm);
}
