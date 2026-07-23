//! Gitea webhook event types — stage 9.A of the Jenkins Gitea plugin rewrite.
//!
//! This module mirrors the upstream Java POJOs in
//! `org.jenkinsci.plugin.gitea.client.api` (the `GiteaXxxEvent` family),
//! but only the subset of fields the Rust routing layer actually needs.
//! The full payload JSON is forwarded to Java untouched; Java re-parses
//! it with the existing Jackson `ObjectMapper` and the full DTO hierarchy.
//!
//! Sources ported (selectively):
//! * `GiteaEventType` — the `X-Gitea-Event` header enum
//! * `GiteaEvent` — base type (repository + sender)
//! * `GiteaPushEvent`, `GiteaPullRequestEvent`, `GiteaCreateEvent`,
//!   `GiteaDeleteEvent`, `GiteaReleaseEvent`, `GiteaRepositoryEvent`
//!
//! ## Wire format
//!
//! Gitea sends **snake_case** JSON keys (`html_url`, `full_name`,
//! `compare_url`, `ref_type`, `avatar_url`, `clone_url`, `ssh_url`,
//! `default_branch`). The upstream Java POJOs use Jackson
//! `@JsonProperty("snake_case")` annotations to map each field
//! individually. We follow the same convention by keeping Rust field
//! names in snake_case (which is the language default), so serde's
//! default derive matches Gitea's wire format directly.
//!
//! All structs derive `Deserialize` and tolerate unknown fields (so the
//! Rust layer does not need to track every upstream change).

use serde::Deserialize;

/// The event type Gitea sends in the `X-Gitea-Event` HTTP header.
///
/// Upstream `GiteaEventType` only enumerates six values (`create`, `push`,
/// `pull_request`, `repository`, `delete`, `release`) — those are the
/// webhook categories the Jenkins plugin subscribes to. We keep the enum
/// permissive via `other`-style fallback by exposing a `from_header`
/// constructor that returns `None` for unrecognised strings, so the
/// server can return `400 Bad Request` rather than panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GiteaEventType {
    Create,
    Push,
    PullRequest,
    Repository,
    Delete,
    Release,
}

impl GiteaEventType {
    /// Parse the value of the `X-Gitea-Event` header (case-insensitive).
    /// Returns `None` if the string does not match a supported event.
    pub fn from_header(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "create" => Some(GiteaEventType::Create),
            "push" => Some(GiteaEventType::Push),
            "pull_request" => Some(GiteaEventType::PullRequest),
            "repository" => Some(GiteaEventType::Repository),
            "delete" => Some(GiteaEventType::Delete),
            "release" => Some(GiteaEventType::Release),
            _ => None,
        }
    }

    /// Stable lowercase string suitable for passing to the Java callback
    /// (`RustWebhookDispatcher.handleEvent(type, json)`).
    pub fn as_str(&self) -> &'static str {
        match self {
            GiteaEventType::Create => "create",
            GiteaEventType::Push => "push",
            GiteaEventType::PullRequest => "pull_request",
            GiteaEventType::Repository => "repository",
            GiteaEventType::Delete => "delete",
            GiteaEventType::Release => "release",
        }
    }
}

// ---------------------------------------------------------------------------
// Shared nested types
// ---------------------------------------------------------------------------

/// Minimal mirror of `GiteaOwner` as it appears in webhook payloads. Only
/// the fields used for routing / logging are decoded; everything else is
/// forwarded to Java inside the raw payload.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct EventUser {
    /// User or organisation login (`acme` in `acme/widget`).
    pub login: String,
    /// Optional display name. Gitea sends `full_name` (snake_case).
    pub full_name: Option<String>,
    /// Optional email address.
    pub email: Option<String>,
    /// Optional avatar URL. Gitea sends `avatar_url` (snake_case).
    pub avatar_url: Option<String>,
    /// Gitea sometimes sends `username` instead of `login`.
    pub username: Option<String>,
}

/// Minimal mirror of `GiteaRepository` for webhook payloads.
///
/// `clone_url` / `ssh_url` / `default_branch` are `Option` because Gitea
/// omits them on some events (notably `delete` and `repository`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct EventRepository {
    /// Repository short name (`widget`).
    pub name: String,
    /// `owner/repo` slash form (`acme/widget`). Wire key: `full_name`.
    pub full_name: String,
    /// Human-visible HTTPS URL. Wire key: `html_url`.
    pub html_url: String,
    /// HTTPS clone URL. Wire key: `clone_url`.
    pub clone_url: Option<String>,
    /// SSH clone URL. Wire key: `ssh_url`.
    pub ssh_url: Option<String>,
    /// Owner of the repository.
    pub owner: EventUser,
    /// Default branch (e.g. `main`). Optional on non-push events.
    /// Wire key: `default_branch`.
    pub default_branch: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// Optional website.
    pub website: Option<String>,
}

// ---------------------------------------------------------------------------
// Event-specific payloads
// ---------------------------------------------------------------------------

/// `GiteaEventType::Push` payload.
///
/// Ported from `GiteaPushEvent`. `ref` is renamed to `ref_` because `ref`
/// is a Rust reserved word. `compare_url` / `commits` / `pusher` are
/// optional since Gitea may omit them on empty pushes.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct PushEvent {
    /// Wire key: `ref` (reserved Rust word).
    #[serde(rename = "ref")]
    pub ref_: String,
    pub before: String,
    pub after: String,
    pub compare_url: Option<String>,
    /// Commits are forwarded to Java as raw JSON — we do not decode the
    /// individual `GiteaCommit` objects in Rust.
    pub commits: Option<serde_json::Value>,
    pub repository: EventRepository,
    pub sender: EventUser,
    /// Gitea uses `pusher` for push events (older field name).
    pub pusher: Option<EventUser>,
}

/// `GiteaEventType::PullRequest` payload.
///
/// The `pull_request` sub-document is large and varies between Gitea
/// versions, so we forward it to Java untouched as a raw JSON `Value`.
/// `action` is kept as a `String` (values like `"opened"`, `"synchronized"`,
/// `"closed"`) rather than an enum because Java's
/// `GiteaPullRequestEventType` is the source of truth.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct PullRequestEvent {
    pub action: String,
    pub number: i64,
    /// Raw pull request object — forwarded to Java untouched. Gitea sends
    /// this under the `pull_request` key.
    pub pull_request: serde_json::Value,
    pub repository: EventRepository,
    pub sender: EventUser,
}

/// `GiteaEventType::Create` payload. Ported from `GiteaCreateEvent`.
///
/// `ref_type` is `branch` / `tag` / `repository` on the wire.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CreateEvent {
    #[allow(dead_code)]
    pub sha: Option<String>,
    /// Wire key: `ref` (reserved Rust word, escaped as `r#ref`).
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub ref_type: String,
    pub repository: EventRepository,
    pub sender: EventUser,
}

/// `GiteaEventType::Delete` payload. Ported from `GiteaDeleteEvent`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DeleteEvent {
    /// Wire key: `ref` (reserved Rust word, escaped as `r#ref`).
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub ref_type: String,
    pub repository: EventRepository,
    pub sender: EventUser,
}

/// `GiteaEventType::Release` payload. Ported from `GiteaReleaseEvent`.
///
/// `release` is forwarded as raw JSON to Java.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ReleaseEvent {
    pub action: String,
    pub release: serde_json::Value,
    pub repository: EventRepository,
    pub sender: EventUser,
}

/// `GiteaEventType::Repository` payload. Ported from `GiteaRepositoryEvent`.
///
/// `action` values include `created` / `deleted` / `published` /
/// `transferred`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct RepositoryEvent {
    pub action: String,
    pub repository: EventRepository,
    pub sender: EventUser,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_type_round_trips_through_header() {
        for (header, expected) in [
            ("push", GiteaEventType::Push),
            ("Pull_Request", GiteaEventType::PullRequest),
            ("  CREATE  ", GiteaEventType::Create),
            ("delete", GiteaEventType::Delete),
            ("release", GiteaEventType::Release),
            ("repository", GiteaEventType::Repository),
        ] {
            assert_eq!(
                GiteaEventType::from_header(header),
                Some(expected),
                "header parsing for {:?}",
                header
            );
        }
    }

    #[test]
    fn event_type_rejects_unknown_header() {
        assert_eq!(GiteaEventType::from_header("foobar"), None);
        assert_eq!(GiteaEventType::from_header(""), None);
    }

    #[test]
    fn event_type_as_str_matches_header_values() {
        // The strings we pass to Java must match the values Gitea puts in
        // `X-Gitea-Event`, so Java can switch on them identically.
        assert_eq!(GiteaEventType::Push.as_str(), "push");
        assert_eq!(GiteaEventType::PullRequest.as_str(), "pull_request");
        assert_eq!(GiteaEventType::Create.as_str(), "create");
        assert_eq!(GiteaEventType::Delete.as_str(), "delete");
        assert_eq!(GiteaEventType::Release.as_str(), "release");
        assert_eq!(GiteaEventType::Repository.as_str(), "repository");
    }

    #[test]
    fn parses_push_event_minimal() {
        let payload = json!({
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
        assert_eq!(event.repository.full_name, "acme/widget");
        assert_eq!(event.repository.owner.login, "acme");
        assert_eq!(event.sender.login, "alice");
        assert_eq!(event.after, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
    }

    #[test]
    fn parses_push_event_with_full_gitea_payload() {
        // Realistic Gitea push payload using snake_case keys throughout.
        let payload = json!({
            "ref": "refs/heads/develop",
            "before": "aaa",
            "after": "bbb",
            "compare_url": "https://gitea.example/c/aaa...bbb",
            "repository": {
                "name": "widget",
                "full_name": "acme/widget",
                "html_url": "https://gitea.example/acme/widget",
                "clone_url": "https://gitea.example/acme/widget.git",
                "ssh_url": "git@gitea.example:acme/widget.git",
                "owner": {"login": "acme"},
                "default_branch": "develop"
            },
            "sender": {"login": "alice"},
            "pusher": {"login": "alice", "email": "alice@acme.io"},
            "commits": [{"id": "bbb", "message": "fix"}]
        });
        let event: PushEvent = serde_json::from_value(payload).unwrap();
        assert_eq!(event.repository.full_name, "acme/widget");
        assert_eq!(
            event.repository.clone_url.as_deref(),
            Some("https://gitea.example/acme/widget.git")
        );
        assert_eq!(event.repository.default_branch.as_deref(), Some("develop"));
        assert_eq!(event.pusher.as_ref().unwrap().login, "alice");
        assert!(event.commits.is_some());
    }

    #[test]
    fn parses_pull_request_event() {
        let payload = json!({
            "action": "opened",
            "number": 42,
            "pull_request": {
                "number": 42,
                "title": "Add new feature",
                "head": {"ref": "feature", "label": "acme:feature"},
                "base": {"ref": "main", "label": "acme:main"}
            },
            "repository": {
                "name": "widget",
                "full_name": "acme/widget",
                "html_url": "https://gitea.acme.io/acme/widget",
                "owner": {"login": "acme"}
            },
            "sender": {"login": "alice"}
        });
        let event: PullRequestEvent = serde_json::from_value(payload).unwrap();
        assert_eq!(event.action, "opened");
        assert_eq!(event.number, 42);
        assert_eq!(
            event.pull_request["title"].as_str(),
            Some("Add new feature")
        );
        assert_eq!(event.repository.full_name, "acme/widget");
    }

    #[test]
    fn parses_create_event_branch() {
        let payload = json!({
            "ref": "feature",
            "ref_type": "branch",
            "sha": "abc123",
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
    fn parses_create_event_tag() {
        let payload = json!({
            "ref": "v1.2.3",
            "ref_type": "tag",
            "repository": {
                "full_name": "acme/widget",
                "html_url": "https://gitea.example/acme/widget",
                "name": "widget",
                "owner": {"login": "acme"}
            },
            "sender": {"login": "alice"}
        });
        let event: CreateEvent = serde_json::from_value(payload).unwrap();
        assert_eq!(event.r#ref, "v1.2.3");
        assert_eq!(event.ref_type, "tag");
    }

    #[test]
    fn parses_delete_event() {
        let payload = json!({
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
        assert_eq!(event.ref_type, "branch");
    }

    #[test]
    fn parses_release_event() {
        let payload = json!({
            "action": "published",
            "release": {
                "tag_name": "v1.0.0",
                "name": "First stable",
                "draft": false,
                "prerelease": false
            },
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
        let payload = json!({
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

    #[test]
    fn ignores_unknown_fields() {
        // Future-proofing: Gitea may add new top-level keys; the Rust layer
        // must not reject the payload.
        let payload = json!({
            "ref": "refs/heads/main",
            "before": "a",
            "after": "b",
            "some_new_field": "value",
            "another_future_field": 123,
            "repository": {
                "name": "widget",
                "full_name": "acme/widget",
                "html_url": "https://example/acme/widget",
                "owner": {"login": "acme"},
                "future_repo_field": true
            },
            "sender": {"login": "alice", "future_user_field": "x"}
        });
        let event: PushEvent = serde_json::from_value(payload).unwrap();
        assert_eq!(event.ref_, "refs/heads/main");
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        // A push payload with only the bare minimum required fields.
        let payload = json!({
            "ref": "refs/heads/main",
            "before": "000",
            "after": "111",
            "repository": {
                "name": "r",
                "full_name": "o/r",
                "html_url": "https://example/o/r",
                "owner": {"login": "o"}
            },
            "sender": {"login": "u"}
        });
        let event: PushEvent = serde_json::from_value(payload).unwrap();
        assert_eq!(event.ref_, "refs/heads/main");
        assert!(event.compare_url.is_none());
        assert!(event.commits.is_none());
        assert!(event.pusher.is_none());
    }
}
