//! Adaptive polling scheduler for Gitea repositories. Stage 10.
//!
//! When webhook delivery is unavailable (Gitea behind a firewall, test
//! installations, etc.) the plugin must fall back to polling. The
//! upstream Java `SCMTrigger` re-runs `GiteaSCMSource.fetch()` on every
//! tick, which issues a full `fetchBranches` + `fetchPullRequests` —
//! expensive for large Gitea instances.
//!
//! This module provides a cheaper Tokio task that:
//! 1. Iterates the configured [`PollTarget`] list.
//! 2. Sends `GET /repos/{owner}/{repo}/branches` with an
//!    `If-None-Match: <last-etag>` header.
//! 3. On HTTP 304 (or unchanged ETag) → no-op.
//! 4. On HTTP 200 with a new ETag → records the new ETag and invokes
//!    the same JNI callback as the webhook layer
//!    (`RustWebhookDispatcher.handleEvent("push", body)`).
//!
//! The synthetic push payload contains just enough metadata for the
//! Java side to identify the repository; the actual branch list is
//! re-fetched by the standard SCM-triggered fetch path once the
//! callback lands. This keeps the polling loop O(servers) rather than
//! O(servers * branches * pull-requests) per tick.

use crate::runtime::RT;
use jni::JavaVM;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

/// One repository on one server that the scheduler should poll.
///
/// Field names are `camelCase` to match the JSON built on the Java side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PollTarget {
    /// Gitea web root (e.g. `https://gitea.corp`). Must NOT include
    /// `/api/v1` — that suffix is appended by [`GiteaClient`].
    pub server_url: String,
    /// `0` = anonymous, `1` = token, `2` = `"user:pass"`. See
    /// [`crate::jni::decode_auth`].
    pub auth_type: i32,
    /// Token | `"user:pass"` | `""`.
    pub auth_secret: String,
    /// Repository owner (user or org).
    pub owner: String,
    /// Repository name.
    pub repo: String,
}

/// Top-level configuration document sent by the Java side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PollConfig {
    /// Sleep between full sweeps across all targets. Recommended
    /// 300–3600 s (5 min – 1 h). Values below 60 are clamped to 60.
    pub interval_seconds: u64,
    /// Targets to poll.
    pub targets: Vec<PollTarget>,
}

/// Per-target cached state. Currently just the last-seen ETag, but
/// kept as a struct so future fields (Last-Modified, etc.) slot in.
#[derive(Default)]
struct PollState {
    /// Key: `"{owner}/{repo}@{server_url}"` — see [`key_for`].
    etags: HashMap<String, String>,
}

/// JoinHandle of the currently running sweep loop. `None` when stopped.
static POLL_HANDLE: OnceCell<Mutex<Option<JoinHandle<()>>>> = OnceCell::new();

/// Mutable per-target state, shared between the sweep task and any
/// future stop/restart call.
static POLL_STATE: OnceCell<Arc<Mutex<PollState>>> = OnceCell::new();

fn poll_state() -> &'static Arc<Mutex<PollState>> {
    POLL_STATE.get_or_init(|| Arc::new(Mutex::new(PollState::default())))
}

fn handle_slot() -> &'static Mutex<Option<JoinHandle<()>>> {
    POLL_HANDLE.get_or_init(|| Mutex::new(None))
}

fn key_for(target: &PollTarget) -> String {
    format!("{}/{}@{}", target.owner, target.repo, target.server_url)
}

/// Start (or restart) the polling sweep loop.
///
/// If a loop is already running it is aborted first so that changes to
/// the interval/target list take effect immediately. Hot-reload within
/// a running Jenkins controller is therefore supported (subject to the
/// broader process-global Tokio runtime caveat in `AGENTS.md`).
///
/// Passing `interval_seconds == 0` or an empty `targets` list disables
/// polling: any previous loop is stopped and no new one is started.
pub fn start(config: PollConfig, jvm: JavaVM) {
    // Cancel any previous loop regardless of the new config — even if
    // we end up returning early we want the old task gone.
    if let Ok(mut guard) = handle_slot().lock() {
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    if config.interval_seconds == 0 || config.targets.is_empty() {
        tracing::info!(
            "Polling disabled (interval_seconds={}, targets={})",
            config.interval_seconds,
            config.targets.len()
        );
        return;
    }

    // Clamp to a 60s floor to protect the Gitea server from accidental
    // tight-loop configurations.
    let interval = Duration::from_secs(config.interval_seconds.max(60));
    let state = poll_state().clone();

    let handle = RT.spawn(async move {
        tracing::info!(
            interval_secs = interval.as_secs(),
            targets = config.targets.len(),
            "Polling sweep loop started"
        );
        loop {
            for target in &config.targets {
                if let Err(e) = poll_once(target, &state, &jvm).await {
                    tracing::warn!(
                        error = %e,
                        server = %target.server_url,
                        owner = %target.owner,
                        repo = %target.repo,
                        "poll_once failed"
                    );
                }
            }
            tokio::time::sleep(interval).await;
        }
    });

    if let Ok(mut guard) = handle_slot().lock() {
        *guard = Some(handle);
    }
}

/// Stop the polling sweep loop if one is running. Idempotent.
pub fn stop() {
    if let Ok(mut guard) = handle_slot().lock() {
        if let Some(handle) = guard.take() {
            handle.abort();
            tracing::info!("Polling sweep loop stopped");
        }
    }
}

/// Poll a single target. Public for future unit-testing with wiremock;
/// not currently exercised by the test suite because it requires a
/// JavaVM for the callback path.
async fn poll_once(
    target: &PollTarget,
    state: &Arc<Mutex<PollState>>,
    jvm: &JavaVM,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let key = key_for(target);
    let prev_etag = state.lock().unwrap().etags.get(&key).cloned();

    // Reuse the same client + auth decoding as the main JNI path so
    // proxy/TLS configuration is honoured automatically.
    let auth = crate::jni::decode_auth(target.auth_type, &target.auth_secret);
    let client = crate::client::GiteaClient::new(&target.server_url, auth)?;

    // `client.base_url()` already ends with `/api/v1`, so we append the
    // repo-scoped branches path directly.
    let url = format!(
        "{}/repos/{}/{}/branches",
        client.base_url(),
        target.owner,
        target.repo
    );
    let mut req = client.http().get(&url);
    if let Some(e) = &prev_etag {
        req = req.header("If-None-Match", e);
    }
    let resp = req.send().await?;

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        // Gitea confirmed nothing changed — cheap path, no callback.
        return Ok(());
    }

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()).into());
    }

    let new_etag = resp
        .headers()
        .get("ETag")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = resp.text().await?;

    // Even on a 200 Gitea may return the same ETag (e.g. when ETag is
    // computed before pagination). If so, treat as no-change.
    if let Some(new_e) = &new_etag {
        if Some(new_e) == prev_etag.as_ref() {
            return Ok(());
        }
        state
            .lock()
            .unwrap()
            .etags
            .insert(key.clone(), new_e.clone());
    }

    // Synthesise a minimal "push" payload and hand it off to the same
    // dispatcher the webhook server uses. The Java side re-fetches the
    // actual branch/PR state — we deliberately do NOT embed the full
    // branches list in the synthetic payload, both to keep payloads
    // small and to avoid duplicating the upstream POJO mapping here.
    let server_root = target.server_url.trim_end_matches("/api/v1");
    let synthetic_push = serde_json::json!({
        "action": "push",
        "ref": null,
        "before": null,
        "after": null,
        "repository": {
            "name": target.repo,
            "fullName": format!("{}/{}", target.owner, target.repo),
            "htmlUrl": format!("{}/{}/{}", server_root, target.owner, target.repo),
            "owner": {"login": target.owner}
        },
        "sender": {"login": "polling"},
        "_polled": true,
        "_polledBranchesBody": body,
    })
    .to_string();

    invoke_callback(jvm, "push", &synthetic_push);
    Ok(())
}

/// Invoke `RustWebhookDispatcher.handleEvent(eventType, payload)` via
/// JNI. Best-effort: failures are logged at WARN and do not propagate,
/// because a transient JNI error should not abort the polling loop.
fn invoke_callback(jvm: &JavaVM, event_type: &str, payload: &str) {
    let mut env = match jvm.attach_current_thread() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = ?e, "polling: attach_current_thread failed");
            return;
        }
    };
    // Use the plugin-classloader global ref registered by RustWebhookDispatcher.<clinit>
    // via nativeRegisterDispatcherClass — env.find_class uses the system ClassLoader
    // and cannot see plugin classes.
    let class_ref = match crate::jni_webhook::dispatcher_class() {
        Some(c) => c,
        None => {
            tracing::warn!("polling: DISPATCHER_CLASS not registered — call nativeRegisterDispatcherClass first");
            return;
        }
    };
    let j_type = match env.new_string(event_type) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "polling: new_string event_type failed");
            return;
        }
    };
    let j_payload = match env.new_string(payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "polling: new_string payload failed");
            return;
        }
    };
    if let Err(e) = env.call_static_method(
        class_ref,
        "handleEvent",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[(&j_type).into(), (&j_payload).into()],
    ) {
        tracing::warn!(error = ?e, "polling: handleEvent JNI call failed");
    }
}
