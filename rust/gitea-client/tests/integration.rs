//! Integration tests for [`gitea_client::GiteaClient`] using [`wiremock`].
//!
//! Each test stands up a `MockServer`, points a `GiteaClient` at it, invokes
//! one method, and asserts both the wire-level request (URL, headers, body)
//! and the resulting JSON/string returned to the caller.
//!
//! Coverage goals (see IMPLEMENTATION_PLAN.md §"Этап 1"):
//! - one happy-path test per fetch method
//! - the `fetch_owner` orgs/→users/ fallback
//! - 404 → `"[]"` for `fetch_pull_requests` / `fetch_issues`
//! - `Link: rel="next"` pagination with array concatenation
//! - `Auth::Token` emits `Authorization: token <T>` (NOT `Bearer`)
//! - `Auth::Basic` emits `Authorization: Basic <base64>`
//! - `fetch_file` 404 → `GiteaError::FileNotFound`

use gitea_rust::{Auth, GiteaClient, GiteaError};
use wiremock::matchers::{
    header, method, path, query_param, query_param_contains, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper: build a `GiteaClient` rooted at `server.uri("/")` with no auth.
fn client_at(server: &MockServer) -> GiteaClient {
    GiteaClient::new(&server.uri(), Auth::None).expect("client construction failed")
}

/// Helper: build a `GiteaClient` with a specific auth strategy.
fn client_with_auth(server: &MockServer, auth: Auth) -> GiteaClient {
    GiteaClient::new(&server.uri(), auth).expect("client construction failed")
}

// ============================================================================
// 1. /version
// ============================================================================

#[tokio::test]
async fn fetch_version_returns_json() {
    let server = MockServer::start().await;
    let body = r#"{"version":"1.21.0"}"#;
    Mock::given(method("GET"))
        .and(path("/api/v1/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_version().await.expect("fetch_version ok");
    assert_eq!(json, body);
}

// ============================================================================
// 2. /user and /users/{name}
// ============================================================================

#[tokio::test]
async fn fetch_current_user_returns_json() {
    let server = MockServer::start().await;
    let body = r#"{"login":"alice","id":1}"#;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_current_user().await.expect("fetch_current_user ok");
    assert_eq!(json, body);
}

#[tokio::test]
async fn fetch_user_returns_json() {
    let server = MockServer::start().await;
    let body = r#"{"login":"bob","id":2}"#;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/bob"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_user("bob").await.expect("fetch_user ok");
    assert_eq!(json, body);
}

// ============================================================================
// 3. fetchOwner happy path (orgs/ returns data, no fallback)
// ============================================================================

#[tokio::test]
async fn fetch_owner_returns_org_when_orgs_succeeds() {
    let server = MockServer::start().await;
    let org_body = r#"{"username":"acme","full_name":"Acme"}"#;
    Mock::given(method("GET"))
        .and(path("/api/v1/orgs/acme"))
        .respond_with(ResponseTemplate::new(200).set_body_string(org_body))
        .expect(1)
        .mount(&server)
        .await;
    // users/ should NOT be touched.
    Mock::given(method("GET"))
        .and(path("/api/v1/users/acme"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"wrong":true}"#))
        .expect(0)
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_owner("acme").await.expect("fetch_owner ok");
    assert_eq!(json, org_body);
}

// ============================================================================
// 4. fetchOwner fallback (orgs/ → 404, users/ → 200)
// ============================================================================

#[tokio::test]
async fn fetch_owner_falls_back_to_users_on_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/orgs/charlie"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let user_body = r#"{"login":"charlie","id":9}"#;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/charlie"))
        .respond_with(ResponseTemplate::new(200).set_body_string(user_body))
        .expect(1)
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_owner("charlie").await.expect("fetch_owner fallback ok");
    assert_eq!(json, user_body);
}

#[tokio::test]
async fn fetch_owner_propagates_404_when_both_org_and_user_missing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/orgs/ghost"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/ghost"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let err = c.fetch_owner("ghost").await.expect_err("should be 404");
    match err {
        GiteaError::HttpStatus { status: 404, .. } => {}
        other => panic!("expected HttpStatus 404, got {:?}", other),
    }
}

#[tokio::test]
async fn fetch_owner_propagates_non_404_from_orgs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/orgs/acme"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    // users/ must NOT be consulted when orgs/ fails non-404.
    Mock::given(method("GET"))
        .and(path("/api/v1/users/acme"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let c = client_at(&server);
    let err = c.fetch_owner("acme").await.expect_err("should be 500");
    match err {
        GiteaError::HttpStatus { status: 500, .. } => {}
        other => panic!("expected HttpStatus 500, got {:?}", other),
    }
}

// ============================================================================
// 5. Repository fetches
// ============================================================================

#[tokio::test]
async fn fetch_repository_returns_json() {
    let server = MockServer::start().await;
    let body = r#"{"name":"myrepo","full_name":"alice/myrepo"}"#;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/alice/myrepo"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c
        .fetch_repository("alice", "myrepo")
        .await
        .expect("fetch_repository ok");
    assert_eq!(json, body);
}

#[tokio::test]
async fn fetch_current_user_repositories_paginates() {
    let server = MockServer::start().await;
    // Page 1 — must NOT match a request that carries ?page=2, otherwise the
    // first mock would also catch the page-2 follow-up and loop forever.
    let page2_url = format!("{}/api/v1/user/repos?page=2", server.uri());
    Mock::given(method("GET"))
        .and(path("/api/v1/user/repos"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"[{"name":"a"},{"name":"b"}]"#)
                .insert_header("Link", format!(r#"<{}>; rel="next""#, page2_url).as_str()),
        )
        .mount(&server)
        .await;
    // wiremock matches by path+query; second page has a `page=2` query.
    Mock::given(method("GET"))
        .and(path("/api/v1/user/repos"))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"[{"name":"c"},{"name":"d"}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c
        .fetch_current_user_repositories()
        .await
        .expect("pagination ok");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a", "b", "c", "d"]);
}

#[tokio::test]
async fn fetch_repositories_for_user() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/alice/repos"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"[{"name":"r1"}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_repositories("alice").await.expect("ok");
    assert!(json.contains(r#""name":"r1""#));
}

#[tokio::test]
async fn fetch_organization_repositories() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/orgs/acme/repos"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"[{"name":"r1"}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_organization_repositories("acme").await.expect("ok");
    assert!(json.contains(r#""name":"r1""#));
}

// ============================================================================
// 6. Branches
// ============================================================================

#[tokio::test]
async fn fetch_branch_url_encodes_slash() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/branches/feature%2Ffoo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"name":"feature/foo"}"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_branch("o", "r", "feature/foo").await.expect("ok");
    assert!(json.contains("feature/foo"));
}

#[tokio::test]
async fn fetch_branches_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/branches"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"[{"name":"main"},{"name":"dev"}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_branches("o", "r").await.expect("ok");
    assert!(json.contains(r#""main""#) && json.contains(r#""dev""#));
}

// ============================================================================
// 7. Tags
// ============================================================================

#[tokio::test]
async fn fetch_annotated_tag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/git/tags/abc123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"tag":"v1.0"}"#))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_annotated_tag("o", "r", "abc123").await.expect("ok");
    assert!(json.contains("v1.0"));
}

#[tokio::test]
async fn fetch_tag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/tags/v2.0"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name":"v2.0"}"#))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_tag("o", "r", "v2.0").await.expect("ok");
    assert!(json.contains("v2.0"));
}

#[tokio::test]
async fn fetch_tags_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/tags"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"[{"name":"v1.0"},{"name":"v2.0"}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_tags("o", "r").await.expect("ok");
    assert!(json.contains("v1.0") && json.contains("v2.0"));
}

// ============================================================================
// 8. Commit detail
// ============================================================================

#[tokio::test]
async fn fetch_commit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/git/commits/deadbeef"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"sha":"deadbeef"}"#))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_commit("o", "r", "deadbeef").await.expect("ok");
    assert!(json.contains("deadbeef"));
}

// ============================================================================
// 9. Collaborators + checkCollaborator (HEAD)
// ============================================================================

#[tokio::test]
async fn fetch_collaborators_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/collaborators"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"[{"login":"alice"}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_collaborators("o", "r").await.expect("ok");
    assert!(json.contains("alice"));
}

#[tokio::test]
async fn check_collaborator_returns_true_on_2xx() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/api/v1/repos/o/r/collaborators/alice"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let c = client_at(&server);
    assert!(c.check_collaborator("o", "r", "alice").await.expect("ok"));
}

#[tokio::test]
async fn check_collaborator_returns_false_on_404() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/api/v1/repos/o/r/collaborators/nobody"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = client_at(&server);
    assert!(!c
        .check_collaborator("o", "r", "nobody")
        .await
        .expect("ok — 404 mapped to false"));
}

#[tokio::test]
async fn check_collaborator_errs_on_500() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/api/v1/repos/o/r/collaborators/alice"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let err = c
        .check_collaborator("o", "r", "alice")
        .await
        .expect_err("500 should error");
    match err {
        GiteaError::HttpStatus { status: 500, .. } => {}
        other => panic!("expected HttpStatus 500, got {:?}", other),
    }
}

// ============================================================================
// 10. Organization hooks (CRUD)
// ============================================================================

#[tokio::test]
async fn fetch_hooks_org_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/orgs/acme/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"[{"id":1}]"#))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_hooks_org("acme").await.expect("ok");
    assert!(json.contains(r#""id":1"#));
}

#[tokio::test]
async fn create_hook_org_posts_json() {
    let server = MockServer::start().await;
    let body = r#"{"type":"gitea","config":{"url":"https://x"},"events":["push"],"active":true}"#;
    Mock::given(method("POST"))
        .and(path("/api/v1/orgs/acme/hooks"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_string(r#"{"id":7,"type":"gitea"}"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.create_hook_org("acme", body).await.expect("ok");
    assert!(json.contains(r#""id":7"#));
}

#[tokio::test]
async fn delete_hook_org_returns_unit_on_2xx() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/orgs/acme/hooks/7"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let c = client_at(&server);
    c.delete_hook_org("acme", 7).await.expect("ok");
}

#[tokio::test]
async fn delete_hook_org_errs_on_404() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/orgs/acme/hooks/999"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let err = c.delete_hook_org("acme", 999).await.expect_err("404");
    match err {
        GiteaError::HttpStatus { status: 404, .. } => {}
        other => panic!("expected HttpStatus 404, got {:?}", other),
    }
}

#[tokio::test]
async fn update_hook_org_uses_patch() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/orgs/acme/hooks/7"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c
        .update_hook_org("acme", 7, r#"{"active":false}"#)
        .await
        .expect("ok");
    assert_eq!(json, "{}");
}

// ============================================================================
// 11. Repository hooks (CRUD)
// ============================================================================

#[tokio::test]
async fn fetch_hooks_repo_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"[{"id":3}]"#))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_hooks_repo("o", "r").await.expect("ok");
    assert!(json.contains(r#""id":3"#));
}

#[tokio::test]
async fn create_hook_repo_posts_json() {
    let server = MockServer::start().await;
    let body = r#"{"type":"gitea","active":true}"#;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/hooks"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_string(r#"{"id":99,"type":"gitea"}"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.create_hook_repo("o", "r", body).await.expect("ok");
    assert!(json.contains("99"));
}

#[tokio::test]
async fn delete_hook_repo_returns_unit() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/repos/o/r/hooks/99"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let c = client_at(&server);
    c.delete_hook_repo("o", "r", 99).await.expect("ok");
}

#[tokio::test]
async fn update_hook_repo_uses_patch() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/o/r/hooks/42"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c
        .update_hook_repo("o", "r", 42, r#"{"active":true}"#)
        .await
        .expect("ok");
    assert_eq!(json, "{}");
}

// ============================================================================
// 12. Commit statuses
// ============================================================================

#[tokio::test]
async fn fetch_commit_statuses_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/statuses/deadbeef"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"[{"id":1,"status":"success"}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_commit_statuses("o", "r", "deadbeef").await.expect("ok");
    assert!(json.contains("success"));
}

#[tokio::test]
async fn create_commit_status_posts_json() {
    let server = MockServer::start().await;
    let body = r#"{"state":"success","target_url":"https://jenkins/x","description":"ok","context":"jenkins"}"#;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/statuses/deadbeef"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_string(r#"{"id":5,"state":"success"}"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c
        .create_commit_status("o", "r", "deadbeef", body)
        .await
        .expect("ok");
    assert!(json.contains(r#""id":5"#));
}

// ============================================================================
// 13. Pull requests
// ============================================================================

#[tokio::test]
async fn fetch_pull_request_single() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls/42"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"number":42}"#))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_pull_request("o", "r", 42).await.expect("ok");
    assert!(json.contains("42"));
}

#[tokio::test]
async fn fetch_pull_requests_with_state_open() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls"))
        .and(query_param("state", "open"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"[{"number":1},{"number":2}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c
        .fetch_pull_requests("o", "r", Some("open"))
        .await
        .expect("ok");
    assert!(json.contains("1") && json.contains("2"));
}

#[tokio::test]
async fn fetch_pull_requests_404_returns_empty_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/pulls"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c
        .fetch_pull_requests("o", "r", Some("open"))
        .await
        .expect("404 should map to empty array");
    assert_eq!(json, "[]");
}

// ============================================================================
// 14. Issues
// ============================================================================

#[tokio::test]
async fn fetch_issues_with_state() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues"))
        .and(query_param("state", "closed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"[{"number":5}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_issues("o", "r", Some("closed")).await.expect("ok");
    assert!(json.contains("5"));
}

#[tokio::test]
async fn fetch_issues_404_returns_empty_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_issues("o", "r", None).await.expect("404 → []");
    assert_eq!(json, "[]");
}

// ============================================================================
// 15. fetch_file — 404 maps to FileNotFound
// ============================================================================

#[tokio::test]
async fn fetch_file_returns_content_on_200() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/raw/HEAD/README.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string("# hello"))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let s = c.fetch_file("o", "r", "HEAD", "README.md").await.expect("ok");
    assert_eq!(s, "# hello");
}

#[tokio::test]
async fn fetch_file_404_returns_file_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/raw/HEAD/missing.txt"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let err = c
        .fetch_file("o", "r", "HEAD", "missing.txt")
        .await
        .expect_err("404 → FileNotFound");
    match err {
        GiteaError::FileNotFound(p) => assert_eq!(p, "missing.txt"),
        other => panic!("expected FileNotFound, got {:?}", other),
    }
}

// ============================================================================
// 16. Releases
// ============================================================================

#[tokio::test]
async fn fetch_releases_no_filters_when_both_true() {
    let server = MockServer::start().await;
    // draft=true, prerelease=true → no query params expected
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/releases"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"[{"id":1,"tag_name":"v1"}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_releases("o", "r", true, true).await.expect("ok");
    assert!(json.contains("v1"));
}

#[tokio::test]
async fn fetch_releases_appends_draft_false_when_draft_false() {
    let server = MockServer::start().await;
    // Pre-release filter uses the literal query key `pre-release` (with a
    // hyphen) — this is what Gitea's API actually expects, see upstream
    // `DefaultGiteaConnection#fetchReleases`.
    //
    // We register a permissive catch-all on the releases path and inspect the
    // emitted query string via `query_param_contains` to keep the matcher
    // independent of how exactly `reqwest::Url` serialises the hyphenated
    // key.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/releases"))
        .and(query_param_contains("draft", "false"))
        .and(query_param_contains("pre-release", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"[{"id":1}]"#))
        .expect(1)
        .named("draft+pre-release matcher")
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_releases("o", "r", false, false).await;
    let json = match json {
        Ok(j) => j,
        Err(e) => panic!("fetch_releases errored: {:?}", e),
    };
    assert!(
        json.contains(r#""id":1"#),
        "expected release JSON, got: {}",
        json
    );
}

#[tokio::test]
async fn fetch_releases_404_returns_empty_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/releases"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_releases("o", "r", true, true).await.expect("ok");
    assert_eq!(json, "[]");
}

// ============================================================================
// 17. Release attachments (multipart upload)
// ============================================================================

#[tokio::test]
async fn create_release_attachment_uploads_multipart() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/o/r/releases/10/assets"))
        .and(query_param("name", "file.zip"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_string(r#"{"id":77,"name":"file.zip"}"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let payload = b"hello-bytes".to_vec();
    let json = c
        .create_release_attachment("o", "r", 10, "file.zip", payload)
        .await
        .expect("ok");
    assert!(json.contains("77") && json.contains("file.zip"));
}

// ============================================================================
// 18. Auth header shapes
// ============================================================================

#[tokio::test]
async fn auth_token_emits_gitea_specific_header() {
    let server = MockServer::start().await;
    // The Gitea scheme is "token <T>" (not "Bearer <T>"). Verify exactly.
    Mock::given(method("GET"))
        .and(path("/api/v1/version"))
        .and(header("Authorization", "token abc123"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let c = client_with_auth(&server, Auth::Token("abc123".to_string()));
    c.fetch_version().await.expect("ok");
}

#[tokio::test]
async fn auth_basic_emits_base64_header() {
    let server = MockServer::start().await;
    // "alice:s3cret" base64 == "YWxpY2U6czNjcmV0"
    Mock::given(method("GET"))
        .and(path("/api/v1/version"))
        .and(header("Authorization", "Basic YWxpY2U6czNjcmV0"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let c = client_with_auth(
        &server,
        Auth::Basic {
            user: "alice".to_string(),
            pass: "s3cret".to_string(),
        },
    );
    c.fetch_version().await.expect("ok");
}

#[tokio::test]
async fn auth_none_sends_no_authorization_header() {
    let server = MockServer::start().await;
    // We don't assert absence directly via wiremock, but verifying that the
    // request still succeeds confirms no Authorization header was required.
    Mock::given(method("GET"))
        .and(path("/api/v1/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let c = client_with_auth(&server, Auth::None);
    c.fetch_version().await.expect("ok");
}

// ============================================================================
// 19. Pagination end-to-end (the canonical "two-page fetch" test)
// ============================================================================

#[tokio::test]
async fn pagination_concatenates_pages_via_link_header() {
    let server = MockServer::start().await;

    // Page 1 returns two items and a `Link` to page 2.
    // Important: must NOT also match the page-2 request, otherwise the first
    // mock re-emits the Link header and pagination loops forever.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/branches"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"[{"name":"a"},{"name":"b"}]"#)
                .insert_header(
                    "Link",
                    format!(r#"<{}/api/v1/repos/o/r/branches?page=2>; rel="next""#, server.uri()).as_str(),
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Page 2 returns two more items and no Link header (end of pagination).
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/branches"))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"[{"name":"c"},{"name":"d"}]"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_branches("o", "r").await.expect("pagination ok");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a", "b", "c", "d"]);
}

#[tokio::test]
async fn pagination_strips_null_entries() {
    let server = MockServer::start().await;
    // Gitea occasionally emits `null` array entries (race in their pagination).
    // The upstream Java code strips them; we must too.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/branches"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"[{"name":"a"},null,{"name":"b"}]"#),
        )
        .mount(&server)
        .await;

    let c = client_at(&server);
    let json = c.fetch_branches("o", "r").await.expect("ok");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().all(|e| !e.is_null()));
}

// ============================================================================
// 20. Error propagation
// ============================================================================

#[tokio::test]
async fn http_500_surfaces_as_http_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/o/r/git/commits/xyz"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let c = client_at(&server);
    let err = c.fetch_commit("o", "r", "xyz").await.expect_err("should 500");
    match err {
        GiteaError::HttpStatus { status: 500, .. } => {}
        other => panic!("expected HttpStatus 500, got {:?}", other),
    }
}
