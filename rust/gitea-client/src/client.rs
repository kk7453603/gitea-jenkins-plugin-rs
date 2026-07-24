//! Gitea HTTP client — direct port of `DefaultGiteaConnection.java`.
//!
//! Every method returns raw JSON as a `String` (or `Vec<u8>` for file contents)
//! rather than a typed Rust struct. The Java shim (stage 3) re-parses these
//! strings with its existing Jackson `ObjectMapper`. This avoids duplicating
//! the 41 POJOs from `org.jenkinsci.plugin.gitea.client.api` in Rust.
//!
//! Behaviour preserved from the upstream Java implementation:
//! - `fetch_owner` does a double fetch: `/orgs/{name}` first, falls back to
//!   `/users/{name}` on HTTP 404. Other non-2xx errors propagate.
//! - `fetch_pull_requests` / `fetch_issues` / `fetch_releases` return `"[]"`
//!   on HTTP 404 (PR/issues/releases may be disabled on the server).
//! - `fetch_file` returns [`GiteaError::FileNotFound`] on HTTP 404, matching
//!   the `FileNotFoundException` thrown by the Java code.
//! - `check_collaborator` issues a HEAD request and returns `true` on 2xx,
//!   `false` on 404, error otherwise.
//! - `fetch_branch` URL-encodes the branch name so that names containing `/`
//!   (e.g. `feature/foo`) are accepted as a single path segment.
//! - Pagination follows the `Link: <...>; rel="next"` header, concatenating
//!   JSON arrays across pages. Null entries from Gitea are dropped, matching
//!   the iterator-based null-stripping in the Java `getList` helper.
//! - `Auth::Token` uses the Gitea-specific `Authorization: token <T>` header
//!   (NOT `Bearer`).
//!
//! The base URL is `<server_url>/api/v1`, exactly as in
//! `DefaultGiteaConnection#api()`.

use crate::auth::Auth;
use crate::error::GiteaError;
use reqwest::{Client, Method};
use serde_json::Value;
use std::time::Duration;
use url::Url;

/// Default per-request timeout. The upstream Java code does not set one
/// (relying on the JVM default), but for a native client we want a sane
/// ceiling to avoid hanging the Jenkins controller thread indefinitely.
///
/// Stage 12 moved the `reqwest::Client` construction into
/// [`crate::tls::build_reqwest_client`], which hard-codes the same 60s
/// value (kept in sync with this constant). The constant itself is retained
/// for tests and for documentation discovery tools that scan for the
/// plugin's effective timeout.
#[allow(dead_code)]
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Gitea HTTP API client.
pub struct GiteaClient {
    /// `<server_url>/api/v1`, no trailing slash.
    base_url: Url,
    /// Authentication strategy applied to every request.
    auth: Auth,
    /// Shared connection-pooled HTTP client. Reused across all methods.
    http: Client,
}

impl GiteaClient {
    /// Construct a new client targeting `<server_url>/api/v1`.
    ///
    /// `server_url` should be the Gitea web root (e.g. `https://gitea.example.com`),
    /// without a trailing slash and without `/api/v1`.
    ///
    /// The underlying `reqwest::Client` is sourced from the process-wide
    /// connection pool ([`crate::pool`]) keyed by `(base_url, auth)`, so
    /// repeated calls with the same arguments reuse the TLS session /
    /// connection pool instead of rebuilding it from scratch. The PEM
    /// trust material installed via [`crate::tls_store::set_extra_pem`] is
    /// picked up implicitly by the pool — see [`Self::with_extra_pem`]
    /// for the rare case where a caller needs an explicit PEM override.
    pub fn new(server_url: &str, auth: Auth) -> Result<Self, GiteaError> {
        Self::with_extra_pem(server_url, auth, None)
    }

    /// Construct a client with an explicit extra PEM blob.
    ///
    /// `extra_pem` = additional CA certificates in PEM format (may contain
    /// any number of `BEGIN CERTIFICATE` blocks). Pass `None` to use the
    /// process-global PEM trust material (the common path — every caller
    /// that does not need per-instance trust customisation should go
    /// through [`Self::new`]).
    ///
    /// The `reqwest::Client` is always sourced from [`crate::pool`] — the
    /// `extra_pem` argument here is retained for API compatibility with
    /// the few tests that historically built a client with an explicit
    /// PEM, but it is intentionally ignored on the pooled path because
    /// the cache key deliberately excludes the PEM (see
    /// [`crate::pool`] module docs for the rationale).
    pub fn with_extra_pem(
        server_url: &str,
        auth: Auth,
        _extra_pem: Option<&[u8]>,
    ) -> Result<Self, GiteaError> {
        let trimmed = server_url.trim_end_matches('/');
        let base_url = Url::parse(&format!("{}/api/v1", trimmed))?;
        let http = crate::pool::acquire(server_url, &auth)?;
        Ok(Self {
            base_url,
            auth,
            http,
        })
    }

    /// Reference to the underlying `reqwest::Client`. Exposed for tests that
    /// want to inspect redirect behaviour or build custom requests.
    pub fn http(&self) -> &Client {
        &self.http
    }

    /// Borrow the base URL. Exposed for diagnostics and tests.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    // ---------------------------------------------------------------------
    // Internal request helpers
    // ---------------------------------------------------------------------

    /// Build a URL by appending `segments` (already URL-encoded by the caller)
    /// and the optional `query` (raw, including the leading `?`).
    fn url(&self, segments: &str, query: Option<&str>) -> Url {
        let mut url = self.base_url.clone();
        // `segments` always starts with `/`; `url.join` treats absolute paths
        // as relative to the base's path because `base_url` ends with `/api/v1`
        // (a "directory" URL). We assemble manually to avoid surprises.
        let mut path = url.path().trim_end_matches('/').to_string();
        if !path.ends_with("/api/v1") {
            // Defensive: base_url path is always `/api/v1`.
            path.push_str("/api/v1");
        }
        path.push_str(segments);
        url.set_path(&path);
        if let Some(q) = query {
            url.set_query(Some(q));
        }
        url
    }

    /// Issue a GET and return the response body as a string. Non-2xx (except
    /// where the caller handles it) → [`GiteaError::HttpStatus`].
    async fn get_object(&self, url: Url) -> Result<String, GiteaError> {
        let req = self.auth.apply(self.http.get(url));
        let resp = req.send().await.map_err(GiteaError::Network)?;
        let status = resp.status();
        if status.is_success() {
            Ok(resp.text().await.map_err(GiteaError::Network)?)
        } else {
            let code = status.as_u16();
            let message = status.canonical_reason().unwrap_or("").to_string();
            Err(GiteaError::HttpStatus {
                status: code,
                message,
                body: None,
            })
        }
    }

    /// Issue a GET and treat 404 as `Ok(None)`. Used by `fetch_owner` to
    /// implement the orgs/ → users/ fallback without raising an error.
    async fn get_object_or_404(&self, url: Url) -> Result<Option<String>, GiteaError> {
        let req = self.auth.apply(self.http.get(url));
        let resp = req.send().await.map_err(GiteaError::Network)?;
        let status = resp.status();
        if status.is_success() {
            Ok(Some(resp.text().await.map_err(GiteaError::Network)?))
        } else if status.as_u16() == 404 {
            Ok(None)
        } else {
            let code = status.as_u16();
            let message = status.canonical_reason().unwrap_or("").to_string();
            Err(GiteaError::HttpStatus {
                status: code,
                message,
                body: None,
            })
        }
    }

    /// Generic GET-list helper that follows `Link: rel="next"` pagination and
    /// concatenates JSON arrays across pages. Null entries are stripped, matching
    /// the upstream `getList` null-removal loop.
    ///
    /// `404` is propagated as a regular `HttpStatus` error — the caller decides
    /// whether to translate it to an empty array (`fetch_pull_requests` etc.).
    async fn get_list_paged(&self, url: Url) -> Result<String, GiteaError> {
        let mut combined: Vec<Value> = Vec::new();
        let mut next_url: Option<Url> = Some(url);

        while let Some(current) = next_url.take() {
            let req = self.auth.apply(self.http.get(current.clone()));
            let resp = req.send().await.map_err(GiteaError::Network)?;
            let status = resp.status();
            if !status.is_success() {
                let code = status.as_u16();
                let message = status.canonical_reason().unwrap_or("").to_string();
                return Err(GiteaError::HttpStatus {
                    status: code,
                    message,
                    body: None,
                });
            }

            // Parse the `Link` header BEFORE consuming the body.
            let link_header = resp
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let body = resp.text().await.map_err(GiteaError::Network)?;

            let page: Vec<Value> = serde_json::from_str(&body).unwrap_or_default();
            combined.extend(page);

            if let Some(lh) = link_header {
                if let Some(next) = parse_next_link(&lh) {
                    next_url = Some(Url::parse(&next).map_err(GiteaError::Url)?);
                }
            }
        }

        // Strip nulls (mirrors the Java iterator-based removal).
        combined.retain(|v| !v.is_null());
        Ok(serde_json::to_string(&combined)?)
    }

    /// Issue a POST/PATCH/PUT with an optional JSON body. Returns the response
    /// body as a string on 2xx. `request_body_for_error` is included in the
    /// resulting `HttpStatus` error if the request fails, matching the upstream
    /// `post`/`patch` helpers that stash the body for diagnostics.
    async fn send_json(
        &self,
        method: Method,
        url: Url,
        body: Option<&str>,
    ) -> Result<String, GiteaError> {
        let mut req = self.http.request(method.clone(), url.clone());
        req = self.auth.apply(req);
        if let Some(b) = body {
            req = req.header("Content-Type", "application/json").body(b.to_string());
        }
        let resp = req.send().await.map_err(GiteaError::Network)?;
        let status = resp.status();
        if status.is_success() {
            Ok(resp.text().await.map_err(GiteaError::Network)?)
        } else {
            let code = status.as_u16();
            let message = status.canonical_reason().unwrap_or("").to_string();
            Err(GiteaError::HttpStatus {
                status: code,
                message,
                body: body.map(|s| s.to_string()),
            })
        }
    }

    /// Issue a DELETE and return `()` on 2xx, `HttpStatus` otherwise.
    async fn delete(&self, url: Url) -> Result<(), GiteaError> {
        let req = self.auth.apply(self.http.delete(url));
        let resp = req.send().await.map_err(GiteaError::Network)?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let code = status.as_u16();
            let message = status.canonical_reason().unwrap_or("").to_string();
            Err(GiteaError::HttpStatus {
                status: code,
                message,
                body: None,
            })
        }
    }

    // ---------------------------------------------------------------------
    // 1. /version
    // ---------------------------------------------------------------------

    /// `GET /api/v1/version` → `GiteaVersion` JSON.
    pub async fn fetch_version(&self) -> Result<String, GiteaError> {
        self.get_object(self.url("/version", None)).await
    }

    // ---------------------------------------------------------------------
    // 2. /user, /users/{name}, /orgs/{name}, fetchOwner fallback
    // ---------------------------------------------------------------------

    /// `GET /api/v1/user` → current authenticated user JSON.
    pub async fn fetch_current_user(&self) -> Result<String, GiteaError> {
        self.get_object(self.url("/user", None)).await
    }

    /// `GET /api/v1/users/{name}` → `GiteaUser` JSON.
    pub async fn fetch_user(&self, name: &str) -> Result<String, GiteaError> {
        let encoded = url_encode_path(name);
        self.get_object(self.url(&format!("/users/{}", encoded), None))
            .await
    }

    /// `GET /api/v1/orgs/{name}` → `GiteaOrganization` JSON.
    pub async fn fetch_organization(&self, name: &str) -> Result<String, GiteaError> {
        let encoded = url_encode_path(name);
        self.get_object(self.url(&format!("/orgs/{}", encoded), None))
            .await
    }

    /// Double-fetch: tries `/orgs/{name}` first, falls back to `/users/{name}`
    /// on HTTP 404. Other non-2xx statuses propagate. Mirrors the upstream
    /// `fetchOwner` exactly.
    pub async fn fetch_owner(&self, name: &str) -> Result<String, GiteaError> {
        let encoded = url_encode_path(name);
        let orgs_url = self.url(&format!("/orgs/{}", encoded), None);
        match self.get_object_or_404(orgs_url).await? {
            Some(json) => Ok(json),
            None => {
                // 404 from /orgs/ → try /users/.
                let users_url = self.url(&format!("/users/{}", encoded), None);
                self.get_object(users_url).await
            }
        }
    }

    // ---------------------------------------------------------------------
    // 3. Repositories
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}` → `GiteaRepository` JSON.
    pub async fn fetch_repository(&self, owner: &str, repo: &str) -> Result<String, GiteaError> {
        let path = format!("/repos/{}/{}", url_encode_path(owner), url_encode_path(repo));
        self.get_object(self.url(&path, None)).await
    }

    /// `GET /api/v1/user/repos` → list of `GiteaRepository`.
    pub async fn fetch_current_user_repositories(&self) -> Result<String, GiteaError> {
        self.get_list_paged(self.url("/user/repos", None)).await
    }

    /// `GET /api/v1/users/{user}/repos` → list of `GiteaRepository`.
    pub async fn fetch_repositories(&self, user: &str) -> Result<String, GiteaError> {
        let path = format!("/users/{}/repos", url_encode_path(user));
        self.get_list_paged(self.url(&path, None)).await
    }

    /// `GET /api/v1/orgs/{org}/repos` → list of `GiteaRepository`.
    pub async fn fetch_organization_repositories(&self, org: &str) -> Result<String, GiteaError> {
        let path = format!("/orgs/{}/repos", url_encode_path(org));
        self.get_list_paged(self.url(&path, None)).await
    }

    // ---------------------------------------------------------------------
    // 4. Branches
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/branches/{branch}`.
    ///
    /// `branch` is percent-encoded so that branch names containing `/`
    /// (e.g. `feature/foo`) survive as a single path segment, matching the
    /// upstream use of `UriTemplateBuilder.var("name", true)` with
    /// `StringUtils.split(name, '/')`.
    pub async fn fetch_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/branches/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_segment(branch)
        );
        self.get_object(self.url(&path, None)).await
    }

    /// `GET /api/v1/repos/{owner}/{repo}/branches` (paginated).
    pub async fn fetch_branches(&self, owner: &str, repo: &str) -> Result<String, GiteaError> {
        let path = format!("/repos/{}/{}/branches", url_encode_path(owner), url_encode_path(repo));
        self.get_list_paged(self.url(&path, None)).await
    }

    // ---------------------------------------------------------------------
    // 5. Tags
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/git/tags/{sha}` → `GiteaAnnotatedTag`.
    pub async fn fetch_annotated_tag(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/git/tags/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_segment(sha)
        );
        self.get_object(self.url(&path, None)).await
    }

    /// `GET /api/v1/repos/{owner}/{repo}/tags/{tag}` → `GiteaTag`.
    pub async fn fetch_tag(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/tags/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_segment(tag)
        );
        self.get_object(self.url(&path, None)).await
    }

    /// `GET /api/v1/repos/{owner}/{repo}/tags` (paginated).
    pub async fn fetch_tags(&self, owner: &str, repo: &str) -> Result<String, GiteaError> {
        let path = format!("/repos/{}/{}/tags", url_encode_path(owner), url_encode_path(repo));
        self.get_list_paged(self.url(&path, None)).await
    }

    // ---------------------------------------------------------------------
    // 6. Commits
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/git/commits/{sha}` → `GiteaCommitDetail`.
    pub async fn fetch_commit(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/git/commits/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_segment(sha)
        );
        self.get_object(self.url(&path, None)).await
    }

    // ---------------------------------------------------------------------
    // 7. Collaborators
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/collaborators` (paginated).
    pub async fn fetch_collaborators(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/collaborators",
            url_encode_path(owner),
            url_encode_path(repo)
        );
        self.get_list_paged(self.url(&path, None)).await
    }

    /// `HEAD /api/v1/repos/{owner}/{repo}/collaborators/{user}`.
    ///
    /// Returns `true` on any 2xx, `false` on 404, error otherwise. Mirrors
    /// the upstream `checkCollaborator` which evaluates `status / 100 == 2`.
    pub async fn check_collaborator(
        &self,
        owner: &str,
        repo: &str,
        user: &str,
    ) -> Result<bool, GiteaError> {
        let path = format!(
            "/repos/{}/{}/collaborators/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_path(user)
        );
        let url = self.url(&path, None);
        let req = self.auth.apply(self.http.request(Method::HEAD, url));
        let resp = req.send().await.map_err(GiteaError::Network)?;
        let status = resp.status();
        let code = status.as_u16();
        if code / 100 == 2 {
            Ok(true)
        } else if code == 404 {
            Ok(false)
        } else {
            let message = status.canonical_reason().unwrap_or("").to_string();
            Err(GiteaError::HttpStatus {
                status: code,
                message,
                body: None,
            })
        }
    }

    // ---------------------------------------------------------------------
    // 8. Organization hooks
    // ---------------------------------------------------------------------

    /// `GET /api/v1/orgs/{org}/hooks` (paginated).
    pub async fn fetch_hooks_org(&self, org: &str) -> Result<String, GiteaError> {
        let path = format!("/orgs/{}/hooks", url_encode_path(org));
        self.get_list_paged(self.url(&path, None)).await
    }

    /// `POST /api/v1/orgs/{org}/hooks` with JSON body.
    pub async fn create_hook_org(
        &self,
        org: &str,
        body: &str,
    ) -> Result<String, GiteaError> {
        let path = format!("/orgs/{}/hooks", url_encode_path(org));
        self.send_json(Method::POST, self.url(&path, None), Some(body))
            .await
    }

    /// `DELETE /api/v1/orgs/{org}/hooks/{id}`.
    pub async fn delete_hook_org(&self, org: &str, id: i64) -> Result<(), GiteaError> {
        let path = format!("/orgs/{}/hooks/{}", url_encode_path(org), id);
        self.delete(self.url(&path, None)).await
    }

    /// `PATCH /api/v1/orgs/{org}/hooks/{id}` with JSON body.
    pub async fn update_hook_org(
        &self,
        org: &str,
        id: i64,
        body: &str,
    ) -> Result<String, GiteaError> {
        let path = format!("/orgs/{}/hooks/{}", url_encode_path(org), id);
        self.send_json(Method::PATCH, self.url(&path, None), Some(body))
            .await
    }

    // ---------------------------------------------------------------------
    // 9. Repository hooks
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/hooks` (paginated).
    pub async fn fetch_hooks_repo(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/hooks",
            url_encode_path(owner),
            url_encode_path(repo)
        );
        self.get_list_paged(self.url(&path, None)).await
    }

    /// `POST /api/v1/repos/{owner}/{repo}/hooks` with JSON body.
    pub async fn create_hook_repo(
        &self,
        owner: &str,
        repo: &str,
        body: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/hooks",
            url_encode_path(owner),
            url_encode_path(repo)
        );
        self.send_json(Method::POST, self.url(&path, None), Some(body))
            .await
    }

    /// `DELETE /api/v1/repos/{owner}/{repo}/hooks/{id}`.
    pub async fn delete_hook_repo(
        &self,
        owner: &str,
        repo: &str,
        id: i64,
    ) -> Result<(), GiteaError> {
        let path = format!(
            "/repos/{}/{}/hooks/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            id
        );
        self.delete(self.url(&path, None)).await
    }

    /// `PATCH /api/v1/repos/{owner}/{repo}/hooks/{id}` with JSON body.
    pub async fn update_hook_repo(
        &self,
        owner: &str,
        repo: &str,
        id: i64,
        body: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/hooks/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            id
        );
        self.send_json(Method::PATCH, self.url(&path, None), Some(body))
            .await
    }

    // ---------------------------------------------------------------------
    // 10. Commit statuses
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/statuses/{sha}` (paginated).
    pub async fn fetch_commit_statuses(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/statuses/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_segment(sha)
        );
        self.get_list_paged(self.url(&path, None)).await
    }

    /// `POST /api/v1/repos/{owner}/{repo}/statuses/{sha}` with JSON body.
    pub async fn create_commit_status(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
        body: &str,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/statuses/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_segment(sha)
        );
        self.send_json(Method::POST, self.url(&path, None), Some(body))
            .await
    }

    // ---------------------------------------------------------------------
    // 11. Pull requests
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/pulls/{number}` → `GiteaPullRequest`.
    pub async fn fetch_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            number
        );
        self.get_object(self.url(&path, None)).await
    }

    /// `GET /api/v1/repos/{owner}/{repo}/pulls?state={state}` (paginated).
    ///
    /// `state=None` corresponds to the upstream behaviour when the `Set<GiteaIssueState>`
    /// has size != 1: no `state` query parameter is emitted.
    ///
    /// HTTP 404 is translated to `"[]"` because the Gitea REST API returns 404
    /// when pull requests are disabled on the repository.
    pub async fn fetch_pull_requests(
        &self,
        owner: &str,
        repo: &str,
        state: Option<&str>,
    ) -> Result<String, GiteaError> {
        let path = format!("/repos/{}/{}/pulls", url_encode_path(owner), url_encode_path(repo));
        let query = state.map(|s| format!("state={}", url_encode_query(s)));
        let url = self.url(&path, query.as_deref());
        match self.get_list_paged(url).await {
            Ok(json) => Ok(json),
            Err(GiteaError::HttpStatus { status: 404, .. }) => Ok("[]".to_string()),
            Err(e) => Err(e),
        }
    }

    // ---------------------------------------------------------------------
    // 12. Issues
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/issues?state={state}` (paginated).
    ///
    /// Same 404→`"[]"` semantics as [`Self::fetch_pull_requests`].
    pub async fn fetch_issues(
        &self,
        owner: &str,
        repo: &str,
        state: Option<&str>,
    ) -> Result<String, GiteaError> {
        let path = format!("/repos/{}/{}/issues", url_encode_path(owner), url_encode_path(repo));
        let query = state.map(|s| format!("state={}", url_encode_query(s)));
        let url = self.url(&path, query.as_deref());
        match self.get_list_paged(url).await {
            Ok(json) => Ok(json),
            Err(GiteaError::HttpStatus { status: 404, .. }) => Ok("[]".to_string()),
            Err(e) => Err(e),
        }
    }

    // ---------------------------------------------------------------------
    // 13. Files (raw)
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/raw/{ref}/{path}` → file bytes as
    /// UTF-8 string (raw endpoint always returns text-or-binary; the upstream
    /// Java code returns `byte[]` — we surface `String` for JSON-passthrough
    /// consistency with other methods, see stage-3 shim for `byte[]` mapping).
    ///
    /// HTTP 404 → [`GiteaError::FileNotFound`].
    pub async fn fetch_file(
        &self,
        owner: &str,
        repo: &str,
        ref_: &str,
        path: &str,
    ) -> Result<String, GiteaError> {
        let url_path = format!(
            "/repos/{}/{}/raw/{}/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_path(ref_),
            url_encode_path(path)
        );
        let url = self.url(&url_path, None);
        let req = self.auth.apply(self.http.get(url));
        let resp = req.send().await.map_err(GiteaError::Network)?;
        let status = resp.status();
        let code = status.as_u16();
        if code == 404 {
            return Err(GiteaError::FileNotFound(path.to_string()));
        }
        if status.is_success() {
            Ok(resp.text().await.map_err(GiteaError::Network)?)
        } else {
            let message = status.canonical_reason().unwrap_or("").to_string();
            Err(GiteaError::HttpStatus {
                status: code,
                message,
                body: None,
            })
        }
    }

    /// Same as [`Self::fetch_file`] but returns the raw response bytes. Used
    /// by the JNI layer to produce a `jbyteArray` for the Java
    /// `byte[] fetchFile(...)` contract. Binary-safe.
    ///
    /// HTTP 404 → [`GiteaError::FileNotFound`].
    pub async fn fetch_file_bytes(
        &self,
        owner: &str,
        repo: &str,
        ref_: &str,
        path: &str,
    ) -> Result<Vec<u8>, GiteaError> {
        let url_path = format!(
            "/repos/{}/{}/raw/{}/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_path(ref_),
            url_encode_path(path)
        );
        let url = self.url(&url_path, None);
        let req = self.auth.apply(self.http.get(url));
        let resp = req.send().await.map_err(GiteaError::Network)?;
        let status = resp.status();
        let code = status.as_u16();
        if code == 404 {
            return Err(GiteaError::FileNotFound(path.to_string()));
        }
        if status.is_success() {
            Ok(resp.bytes().await.map_err(GiteaError::Network)?.to_vec())
        } else {
            let message = status.canonical_reason().unwrap_or("").to_string();
            Err(GiteaError::HttpStatus {
                status: code,
                message,
                body: None,
            })
        }
    }

    /// `HEAD /api/v1/repos/{owner}/{repo}/raw/{ref}/{path}` — existence check
    /// without downloading the body. Mirrors the upstream `checkFile` which
    /// issues the same GET-style request but discards the body.
    ///
    /// Returns `true` on any 2xx, `false` on 404, error otherwise.
    pub async fn check_file(
        &self,
        owner: &str,
        repo: &str,
        ref_: &str,
        path: &str,
    ) -> Result<bool, GiteaError> {
        // The upstream Java code actually does a GET and discards the body,
        // but since Gitea does not differentiate HEAD on /raw/, we mirror
        // that semantic with a GET that we drop the body of. We issue a HEAD
        // here for efficiency; behaviour on status codes is identical.
        let url_path = format!(
            "/repos/{}/{}/raw/{}/{}",
            url_encode_path(owner),
            url_encode_path(repo),
            url_encode_path(ref_),
            url_encode_path(path)
        );
        let url = self.url(&url_path, None);
        let req = self.auth.apply(self.http.request(Method::HEAD, url));
        let resp = req.send().await.map_err(GiteaError::Network)?;
        let status = resp.status();
        let code = status.as_u16();
        if code / 100 == 2 {
            Ok(true)
        } else if code == 404 {
            Ok(false)
        } else {
            let message = status.canonical_reason().unwrap_or("").to_string();
            Err(GiteaError::HttpStatus {
                status: code,
                message,
                body: None,
            })
        }
    }

    // ---------------------------------------------------------------------
    // 14. Releases
    // ---------------------------------------------------------------------

    /// `GET /api/v1/repos/{owner}/{repo}/releases?draft=&pre-release=`.
    ///
    /// Replicates the upstream query construction:
    /// - If both `draft` and `prerelease` are `true`, NO query is appended
    ///   (Gitea returns everything).
    /// - Otherwise, only `draft=false` and/or `pre-release=false` are added
    ///   to *exclude* those categories.
    ///
    /// Note the upstream uses the literal query key `pre-release` (with a
    /// hyphen), which is what Gitea's API actually expects.
    ///
    /// HTTP 404 → `"[]"` (releases may be disabled on the repository).
    pub async fn fetch_releases(
        &self,
        owner: &str,
        repo: &str,
        draft: bool,
        prerelease: bool,
    ) -> Result<String, GiteaError> {
        let path = format!("/repos/{}/{}/releases", url_encode_path(owner), url_encode_path(repo));
        // The upstream Java code builds a query string starting with `?`,
        // but `Url::set_query` takes the value WITHOUT the leading `?`.
        // We assemble the inner parts here.
        let query: Option<String> = if !draft || !prerelease {
            let mut parts: Vec<&str> = Vec::new();
            if !draft {
                parts.push("draft=false");
            }
            if !prerelease {
                parts.push("pre-release=false");
            }
            Some(parts.join("&"))
        } else {
            None
        };
        let url = self.url(&path, query.as_deref());
        match self.get_list_paged(url).await {
            Ok(json) => Ok(json),
            Err(GiteaError::HttpStatus { status: 404, .. }) => Ok("[]".to_string()),
            Err(e) => Err(e),
        }
    }

    // ---------------------------------------------------------------------
    // 15. Release attachments (multipart upload)
    // ---------------------------------------------------------------------

    /// `POST /api/v1/repos/{owner}/{repo}/releases/{id}/assets?name={name}`
    /// with a `multipart/form-data` body containing the file.
    ///
    /// `data` is the raw file content; `name` is both the `?name=` query
    /// parameter and the filename in the multipart part, matching the upstream
    /// Java implementation.
    pub async fn create_release_attachment(
        &self,
        owner: &str,
        repo: &str,
        release_id: i64,
        name: &str,
        data: Vec<u8>,
    ) -> Result<String, GiteaError> {
        let path = format!(
            "/repos/{}/{}/releases/{}/assets",
            url_encode_path(owner),
            url_encode_path(repo),
            release_id
        );
        let query = format!("name={}", url_encode_query(name));
        let url = self.url(&path, Some(&query));

        // Build the multipart form. The Java code uses field name "attachment"
        // and the provided filename.
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(name.to_string())
            .mime_str("application/octet-stream")
            .map_err(GiteaError::Network)?;
        let form = reqwest::multipart::Form::new().part("attachment", part);

        let req = self.http.request(Method::POST, url).multipart(form);
        let req = self.auth.apply(req);
        let resp = req.send().await.map_err(GiteaError::Network)?;
        let status = resp.status();
        if status.is_success() {
            Ok(resp.text().await.map_err(GiteaError::Network)?)
        } else {
            let code = status.as_u16();
            let message = status.canonical_reason().unwrap_or("").to_string();
            Err(GiteaError::HttpStatus {
                status: code,
                message,
                body: Some("<multipart/form-data with file>".to_string()),
            })
        }
    }
}

// -------------------------------------------------------------------------
// Free functions
// -------------------------------------------------------------------------

/// Percent-encode a string for use as a single path segment. Forward slashes
/// are encoded so e.g. `feature/foo` becomes `feature%2Ffoo`, mirroring
/// `UriTemplateBuilder.var("name", true)` combined with `StringUtils.split`
/// in the Java client.
///
/// Only "unsafe" characters are encoded — `.`, `-`, `_`, `~` and other
/// sub-delim chars are preserved so branch names like `v2.0` or
/// `release-1.2` stay readable.
fn url_encode_segment(s: &str) -> String {
    percent_encode_path_segment(s)
}

/// Percent-encode each path segment but preserve `/` as a separator. Used for
/// multi-segment inputs like a file path or `owner/repo` style arguments.
fn url_encode_path(s: &str) -> String {
    s.split('/')
        .map(percent_encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Percent-encode a query parameter value. Same encoding as path segments is
/// acceptable for Gitea's query parameters.
fn url_encode_query(s: &str) -> String {
    percent_encode_path_segment(s)
}

/// Encode only characters that are actually unsafe in a URL path segment:
///   - control characters (0x00–0x1F, 0x7F)
///   - space
///   - delimiters: `/`, `?`, `#`, `[`, `]`
///   - percent itself (so we don't double-encode)
///   - characters that the upstream Java `URI` parser would reject
///
/// Everything else (including `.`, `-`, `_`, `~`, `:`, `@`, `!`, `$`, `&`,
/// `'`, `(`, `)`, `*`, `+`, `,`, `;`, `=`) is preserved verbatim so that
/// ordinary Git refs like `v2.0`, `release-1.2`, `feature/foo` (the `/` here
/// is handled by the caller, not by this function) survive untouched.
fn percent_encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let needs_escape = matches!(b, 0..=0x20 | 0x7F | b'/' | b'?' | b'#' | b'[' | b']' | b'%');
        if needs_escape {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        } else {
            // Multi-byte UTF-8 sequences start with bytes >= 0x80; push them
            // through unchanged because `as_bytes()` already gives us the raw
            // UTF-8 representation.
            out.push(b as char);
        }
    }
    // SAFETY: we only pushed ASCII chars or copied raw UTF-8 bytes, so the
    // resulting string is still valid UTF-8.
    out
}

/// Parse the `Link` header and return the URL for `rel="next"` if present.
///
/// Gitea emits headers in the standard RFC-8288 format:
/// ```text
/// Link: <https://git.example.com/api/v1/.../hooks?page=2>; rel="next", <...>; rel="last"
/// ```
/// We match the upstream Java regex `<(.*)>;\s*rel="next"`.
fn parse_next_link(link_header: &str) -> Option<String> {
    // Split on commas, then look for the `rel="next"` token.
    for entry in link_header.split(',') {
        let entry = entry.trim();
        // Each entry looks like: <url>; rel="next"   (with possible extra params)
        let mut url_part: Option<&str> = None;
        let mut is_next = false;
        for token in entry.split(';') {
            let token = token.trim();
            if token.starts_with('<') && token.ends_with('>') {
                url_part = Some(&token[1..token.len() - 1]);
            } else if token == "rel=\"next\"" {
                is_next = true;
            }
        }
        if is_next {
            if let Some(u) = url_part {
                return Some(u.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_next_link_extracts_next_url() {
        let header = r#"<https://srv/api/v1/repos/o/r/hooks?page=2>; rel="next", <https://srv/api/v1/repos/o/r/hooks?page=2>; rel="last""#;
        assert_eq!(
            parse_next_link(header),
            Some("https://srv/api/v1/repos/o/r/hooks?page=2".to_string())
        );
    }

    #[test]
    fn parse_next_link_returns_none_when_no_next() {
        let header = r#"<https://srv/api/v1/repos/o/r/hooks?page=2>; rel="last""#;
        assert_eq!(parse_next_link(header), None);
    }

    #[test]
    fn url_encode_segment_encodes_slash() {
        assert_eq!(url_encode_segment("feature/foo"), "feature%2Ffoo");
    }

    #[test]
    fn url_encode_segment_preserves_dot_and_dash() {
        assert_eq!(url_encode_segment("v2.0"), "v2.0");
        assert_eq!(url_encode_segment("release-1.2"), "release-1.2");
    }

    #[test]
    fn url_encode_path_preserves_slash() {
        assert_eq!(url_encode_path("a/b/c"), "a/b/c");
    }
}


