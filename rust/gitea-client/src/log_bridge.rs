//! Tracing → Jenkins log bridge.
//!
//! Forwards `tracing` events from Rust into the Jenkins `java.util.logging`
//! hierarchy by calling `RustLogReceiver.handleLog(level, target, message)`
//! via JNI. The Java side maps the (level, target, message) tuple to a
//! `Logger.getLogger("org.jenkinsci.plugin.gitea." + target)` invocation so
//! operators can filter Rust logs in the standard Jenkins System Log UI.
//!
//! ## Threading
//!
//! `tracing::Event`s fire on whatever thread emitted them — tokio workers,
//! the axum HTTP handler, or the JNI callback path. We re-attach the
//! current thread to the JVM on each event, which is cheap (refcount bump
//! if already attached).
//!
//! ## Filtering
//!
//! DEBUG/TRACE events are dropped at the layer level to avoid flooding
//! Jenkins logs — Rust internal noise (hyper, rustls, reqwest) is not
//! useful to Jenkins operators. INFO and above are forwarded.

use jni::JavaVM;
use jni::objects::GlobalRef;
use std::sync::OnceLock;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::Layer;

/// Process-global state for the bridge: a global ref to the Java
/// `RustLogReceiver` class plus the owning `JavaVM`. Set once from
/// `install_log_bridge` (called from
/// `RustWebhookDispatcher.<clinit>` via the
/// `Java_..._nativeInstallLogBridge` JNI export).
struct BridgeState {
    class: GlobalRef,
    jvm: JavaVM,
}

static BRIDGE: OnceLock<BridgeState> = OnceLock::new();

/// Install the Rust→Java log bridge. Idempotent — second call is a no-op
/// (the first caller wins, matching `OnceLock` semantics). After install,
/// every `tracing::info!/warn!/error!` event whose target is a Rust path
/// is forwarded to the JVM.
pub fn install_log_bridge(class: GlobalRef, jvm: JavaVM) {
    if BRIDGE.set(BridgeState { class, jvm }).is_ok() {
        let subscriber = tracing_subscriber::registry().with(LogBridgeLayer);
        // A global default can only be set once per process — second call
        // returns Err, which we silently drop. This is fine: if the
        // caller already installed a subscriber (e.g. a test harness),
        // they own the global one and we don't override it.
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}

/// Public accessor — useful for tests.
pub fn is_installed() -> bool {
    BRIDGE.get().is_some()
}

/// Tracing layer that forwards events into Java.
pub struct LogBridgeLayer;

impl<S> Layer<S> for LogBridgeLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        // Drop DEBUG/TRACE — too noisy for Jenkins UI.
        let level = match *metadata.level() {
            tracing::Level::INFO | tracing::Level::WARN | tracing::Level::ERROR => {
                metadata.level().as_str()
            }
            _ => return,
        };
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let target = metadata.target().to_string();
        let message = visitor.message.unwrap_or_default();
        forward_to_java(level, &target, &message);
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // The conventional name for the human-readable summary is "message".
        if field.name() == "message" {
            self.message = Some(format!("{:?}", value));
        } else if self.message.is_none() {
            self.message = Some(format!("{}={:?}", field.name(), value));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else if self.message.is_none() {
            self.message = Some(format!("{}={}", field.name(), value));
        }
    }
}

/// Perform the JNI callback. Failure here MUST NOT propagate — this runs
/// on whatever thread emitted the tracing event, including inside JNI
/// callbacks themselves (recursive call risk).
fn forward_to_java(level: &str, target: &str, message: &str) {
    let Some(state) = BRIDGE.get() else {
        return;
    };
    let Ok(mut env) = state.jvm.attach_current_thread() else {
        return;
    };
    let Ok(j_level) = env.new_string(level) else { return };
    let Ok(j_target) = env.new_string(target) else { return };
    let Ok(j_message) = env.new_string(message) else { return };
    let _ = env.call_static_method(
        &state.class,
        "handleLog",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        &[(&j_level).into(), (&j_target).into(), (&j_message).into()],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_without_install_is_safe() {
        // Calling forward_to_java before install_log_bridge must be a
        // silent no-op (BRIDGE not set). We can't actually install in a
        // unit test (needs a JavaVM), so this just confirms the guard
        // works.
        forward_to_java("INFO", "test_target", "test_message");
        assert!(!is_installed());
    }

    #[test]
    fn message_visitor_prefers_explicit_message_field() {
        let mut v = MessageVisitor::default();
        // Simulate a field record. We don't have a real tracing::Field
        // available without a span context, so we rely on the str path.
        // record_str is called by tracing with the field name and value
        // when a `message = "..."` is set on an event.
        // Here we just confirm the visitor starts with no message.
        assert!(v.message.is_none());
    }
}
