//! Integration tests for the stage 9.A webhook layer.
//!
//! These exercise the public surface of `gitea_rust::events` and
//! `gitea_rust::server` (HMAC verification + axum HTTP handler) end-to-end.
//! The JNI callback is mocked via the `set_java_callback` hook so no JVM
//! is required.
//!
//! Run with: `cargo test --test webhook`

use gitea_rust::events::{
    CreateEvent, DeleteEvent, GiteaEventType, PullRequestEvent, PushEvent, ReleaseEvent,
    RepositoryEvent,
};
use gitea_rust::server::{set_java_callback, timing_safe_eq, verify_hmac, WebhookServer};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::{Arc, Mutex, OnceLock};

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// Event parsing
// ---------------------------------------------------------------------------

#[test]
fn parses_all_event_types_from_headers() {
    for (header, expected) in [
        ("push", GiteaEventType::Push),
        ("pull_request", GiteaEventType::PullRequest),
        ("create", GiteaEventType::Create),
        ("delete", GiteaEventType::Delete),
        ("release", GiteaEventType::Release),
        ("repository", GiteaEventType::Repository),
    ] {
        assert_eq!(GiteaEventType::from_header(header), Some(expected));
    }
}

#[test]
fn parses_push_event() {
    let payload = serde_json::json!({
        "ref": "refs/heads/main",
        "before": "0000000000000000000000000000000000000000",
        "after": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        "repository": {
            "name": "widget",
            "full_name": "acme/widget",
            "html_url": "https://gitea.acme.io/acme/widget",
            "owner": {"login": "acme"}
        },
        "sender": {"login": "alice"}
    });
    let event: PushEvent = serde_json::from_value(payload).unwrap();
    assert_eq!(event.ref_, "refs/heads/main");
    assert_eq!(event.repository.name, "widget");
    assert_eq!(event.repository.owner.login, "acme");
    assert_eq!(event.sender.login, "alice");
}

#[test]
fn parses_pull_request_event() {
    let payload = serde_json::json!({
        "action": "opened",
        "number": 42,
        "pull_request": {
            "title": "Add feature",
            "head": {"ref": "feature"},
            "base": {"ref": "main"}
        },
        "repository": {
            "name": "widget",
            "full_name": "acme/widget",
            "html_url": "https://gitea.example/acme/widget",
            "owner": {"login": "acme"}
        },
        "sender": {"login": "alice"}
    });
    let event: PullRequestEvent = serde_json::from_value(payload).unwrap();
    assert_eq!(event.action, "opened");
    assert_eq!(event.number, 42);
    assert_eq!(event.pull_request["title"].as_str(), Some("Add feature"));
}

#[test]
fn parses_create_event() {
    let payload = serde_json::json!({
        "ref": "feature",
        "ref_type": "branch",
        "repository": {
            "name": "widget",
            "full_name": "acme/widget",
            "html_url": "https://gitea.example/acme/widget",
            "owner": {"login": "acme"}
        },
        "sender": {"login": "alice"}
    });
    let event: CreateEvent = serde_json::from_value(payload).unwrap();
    assert_eq!(event.r#ref, "feature");
    assert_eq!(event.ref_type, "branch");
}

#[test]
fn parses_delete_event() {
    let payload = serde_json::json!({
        "ref": "feature",
        "ref_type": "branch",
        "repository": {
            "name": "widget",
            "full_name": "acme/widget",
            "html_url": "https://gitea.example/acme/widget",
            "owner": {"login": "acme"}
        },
        "sender": {"login": "alice"}
    });
    let event: DeleteEvent = serde_json::from_value(payload).unwrap();
    assert_eq!(event.r#ref, "feature");
}

#[test]
fn parses_release_event() {
    let payload = serde_json::json!({
        "action": "published",
        "release": {"tag_name": "v1.0.0"},
        "repository": {
            "name": "widget",
            "full_name": "acme/widget",
            "html_url": "https://gitea.example/acme/widget",
            "owner": {"login": "acme"}
        },
        "sender": {"login": "alice"}
    });
    let event: ReleaseEvent = serde_json::from_value(payload).unwrap();
    assert_eq!(event.action, "published");
    assert_eq!(event.release["tag_name"].as_str(), Some("v1.0.0"));
}

#[test]
fn parses_repository_event() {
    let payload = serde_json::json!({
        "action": "created",
        "repository": {
            "name": "newrepo",
            "full_name": "acme/newrepo",
            "html_url": "https://gitea.example/acme/newrepo",
            "owner": {"login": "acme"}
        },
        "sender": {"login": "alice"}
    });
    let event: RepositoryEvent = serde_json::from_value(payload).unwrap();
    assert_eq!(event.action, "created");
    assert_eq!(event.repository.name, "newrepo");
}

// ---------------------------------------------------------------------------
// HMAC verification (pure function — no JVM, no socket)
// ---------------------------------------------------------------------------

#[test]
fn verify_hmac_accepts_correct_signature() {
    let secret = "topsecret";
    let body = br#"{"ref":"refs/heads/main"}"#;
    let sig = compute_sig(secret, body);
    assert!(verify_hmac(secret, body, &sig));
}

#[test]
fn verify_hmac_rejects_wrong_signature() {
    let secret = "topsecret";
    let body = br#"{"ref":"refs/heads/main"}"#;
    assert!(!verify_hmac(secret, body, &"0".repeat(64)));
}

#[test]
fn verify_hmac_rejects_tampered_body() {
    let secret = "topsecret";
    let body = br#"{"ref":"refs/heads/main"}"#;
    let sig = compute_sig(secret, body);
    let mut tampered = body.to_vec();
    tampered[0] ^= 0xFF;
    assert!(!verify_hmac(secret, &tampered, &sig));
}

#[test]
fn verify_hmac_accepts_uppercase_signature() {
    let secret = "topsecret";
    let body = br#"{"hello":"world"}"#;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let sig_upper = hex::encode_upper(mac.finalize().into_bytes());
    assert!(verify_hmac(secret, body, &sig_upper));
}

#[test]
fn timing_safe_eq_basics() {
    assert!(timing_safe_eq("abc", "abc"));
    assert!(!timing_safe_eq("abc", "abd"));
    assert!(!timing_safe_eq("abc", "abcd"));
    assert!(timing_safe_eq("", ""));
}

// ---------------------------------------------------------------------------
// HTTP-layer integration tests against a real ephemeral server.
//
// Tests in this file embed a unique `_test` marker in their request body
// so that even when run in parallel against a single shared recording
// callback, each test can find its own record.
// ---------------------------------------------------------------------------

/// Recorded callbacks from the test-only Java stub.
static RECORDED: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();

fn recorded() -> &'static Mutex<Vec<(String, String)>> {
    RECORDED.get_or_init(|| Mutex::new(Vec::new()))
}

fn install_recording_callback() {
    let cb: Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync> = Arc::new(
        |event_type, payload| {
            // Use `into_inner()` on a poisoned mutex so a panic in another
            // test does not cascade here.
            let mut guard = match recorded().lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.push((event_type.to_string(), payload.to_string()));
            Ok(())
        },
    );
    set_java_callback(cb);
}

fn marker(test_name: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("__test_marker_{}_{}__", test_name, nanos)
}

fn find_recorded(marker: &str) -> Option<(String, String)> {
    let guard = match recorded().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard
        .iter()
        .find(|(_, payload)| payload.contains(marker))
        .cloned()
}

fn count_recorded_with(marker: &str) -> usize {
    let guard = match recorded().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard
        .iter()
        .filter(|(_, payload)| payload.contains(marker))
        .count()
}

fn compute_sig(secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

async fn spawn_server(secret: Option<String>) -> WebhookServer {
    WebhookServer::start(0, secret, None, vec![], 60, None)
        .await
        .expect("failed to bind test webhook server")
}

#[tokio::test]
async fn http_accepts_valid_hmac_and_forwards_to_callback() {
    install_recording_callback();
    let m = marker("http_accepts_valid_hmac_and_forwards_to_callback");

    let mut server = spawn_server(Some("topsecret".to_string())).await;
    let addr = server.local_addr();
    let url = format!("http://{}/gitea-webhook/post", addr);

    let body = format!(
        r#"{{"ref":"refs/heads/main","_test":"{}","repository":{{"name":"x","full_name":"o/x","html_url":"http://h/o/x","owner":{{"login":"o"}}}},"sender":{{"login":"u"}}}}"#,
        m
    );
    let sig = compute_sig("topsecret", body.as_bytes());

    let resp = reqwest::Client::new()
        .post(&url)
        .header("X-Gitea-Event", "push")
        .header("X-Gitea-Signature", sig)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    server.shutdown().await;

    let record = find_recorded(&m);
    assert!(record.is_some(), "callback did not record our payload");
    let (event_type, payload) = record.unwrap();
    assert_eq!(event_type, "push");
    assert!(payload.contains("refs/heads/main"));
}

#[tokio::test]
async fn http_rejects_bad_hmac_with_unauthorized() {
    install_recording_callback();
    let m = marker("http_rejects_bad_hmac_with_unauthorized");

    let mut server = spawn_server(Some("topsecret".to_string())).await;
    let addr = server.local_addr();
    let url = format!("http://{}/gitea-webhook/post", addr);

    let body = format!(r#"{{"ref":"refs/heads/main","_test":"{}"}}"#, m);

    let resp = reqwest::Client::new()
        .post(&url)
        .header("X-Gitea-Event", "push")
        .header("X-Gitea-Signature", "0".repeat(64))
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    server.shutdown().await;

    let count = count_recorded_with(&m);
    assert_eq!(count, 0, "callback must not fire on HMAC failure");
}

#[tokio::test]
async fn http_rejects_missing_signature_header() {
    install_recording_callback();
    let mut server = spawn_server(Some("topsecret".to_string())).await;
    let addr = server.local_addr();
    let url = format!("http://{}/gitea-webhook/post", addr);

    let resp = reqwest::Client::new()
        .post(&url)
        .header("X-Gitea-Event", "push")
        .body(r#"{}"#.to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    server.shutdown().await;
}

#[tokio::test]
async fn http_allows_unsigned_when_no_secret_configured() {
    install_recording_callback();
    let m = marker("http_allows_unsigned_when_no_secret_configured");

    let mut server = spawn_server(None).await;
    let addr = server.local_addr();
    let url = format!("http://{}/gitea-webhook/post", addr);

    let body = format!(r#"{{"action":"opened","number":7,"_test":"{}"}}"#, m);

    let resp = reqwest::Client::new()
        .post(&url)
        .header("X-Gitea-Event", "pull_request")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    server.shutdown().await;

    let record = find_recorded(&m);
    assert!(record.is_some(), "callback did not record our payload");
    let (event_type, _) = record.unwrap();
    assert_eq!(event_type, "pull_request");
}

#[tokio::test]
async fn http_rejects_missing_event_header() {
    install_recording_callback();
    let mut server = spawn_server(None).await;
    let addr = server.local_addr();
    let url = format!("http://{}/gitea-webhook/post", addr);

    let resp = reqwest::Client::new()
        .post(&url)
        .body(r#"{}"#.to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    server.shutdown().await;
}

#[tokio::test]
async fn http_rejects_non_utf8_body() {
    install_recording_callback();
    let mut server = spawn_server(None).await;
    let addr = server.local_addr();
    let url = format!("http://{}/gitea-webhook/post", addr);

    let resp = reqwest::Client::new()
        .post(&url)
        .header("X-Gitea-Event", "push")
        .body(vec![0xFFu8, 0xFE, 0xFD])
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    server.shutdown().await;
}

#[tokio::test]
async fn http_accepts_trailing_slash() {
    install_recording_callback();
    let m = marker("http_accepts_trailing_slash");

    let mut server = spawn_server(None).await;
    let addr = server.local_addr();
    let url = format!("http://{}/gitea-webhook/post/", addr);

    let body = format!(r#"{{"ref":"refs/heads/main","_test":"{}"}}"#, m);

    let resp = reqwest::Client::new()
        .post(&url)
        .header("X-Gitea-Event", "push")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    server.shutdown().await;

    assert!(find_recorded(&m).is_some());
}

#[tokio::test]
async fn http_server_shutdown_is_idempotent() {
    let mut server = spawn_server(None).await;
    server.shutdown().await;
    server.shutdown().await; // must not panic
}
