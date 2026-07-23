//! JNI bindings — stage 2 of the Jenkins Gitea plugin rewrite.
//!
//! Every public async method on [`crate::client::GiteaClient`] gets a
//! corresponding `#[no_mangle] extern "system" fn` here. The Java class
//! `org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection` declares a
//! `private static native` method for each one (with a `nativeXxx` name), and
//! the JVM resolves the symbol via the standard JNI naming convention:
//!
//! ```text
//! Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_<methodName>
//! ```
//!
//! Calling convention: each export
//! 1. Decodes the `serverUrl` / `authType` / `authSecret` triple (common to
//!    all entry points) plus any method-specific arguments.
//! 2. Constructs a fresh [`crate::client::GiteaClient`] inside the shared
//!    tokio runtime and `block_on`s the async method. (The Java side is
//!    synchronous, mirroring the upstream `DefaultGiteaConnection`.)
//! 3. On `Ok`, converts the JSON `String` to a `jstring` (or for `void` /
//!    `bool` / `byte[]` methods, returns the appropriate JNI type).
//! 4. On `Err`, maps the [`crate::error::GiteaError`] to a Java exception
//!    via [`throw_gitea_exception`] and returns a null / zero value.
//!
//! Auth encoding (mirrors the Java `RustGiteaConnection` constructor):
//! - `auth_type == 0` → [`crate::auth::Auth::None`]
//! - `auth_type == 1` → [`crate::auth::Auth::Token`] (secret = raw token)
//! - `auth_type == 2` → [`crate::auth::Auth::Basic`] (secret = `"user:pass"`)
//!
//! All other values of `auth_type` are treated as anonymous.

use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jbyte, jbyteArray, jint, jlong, jstring};
use jni::JNIEnv;

use crate::{Auth, GiteaClient, GiteaError};
use crate::runtime::RT;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decode a `JString` into an owned `String`. Returns `Err` if the JVM
/// cannot produce the UTF-8 representation (e.g. OOM).
fn jstr(env: &mut JNIEnv, s: JString) -> Result<String, jni::errors::Error> {
    env.get_string(&s).map(|c| c.into())
}

/// Like [`jstr`] but substitutes an empty string on error. Used for the
/// optional `authSecret` argument so the JNI call still proceeds if the
/// caller passed `null` for anonymous auth.
fn jstr_or_empty(env: &mut JNIEnv, s: JString) -> String {
    jstr(env, s).unwrap_or_default()
}

/// Decode the `(auth_type, secret)` pair produced by the Java
/// `RustGiteaConnection` constructor back into an [`Auth`] variant.
///
/// See the module docs for the encoding contract.
///
/// Public so that stage 10's `polling` module can reuse the exact same
/// encoding when constructing a [`GiteaClient`] for periodic ETag-based
/// polling.
pub fn decode_auth(auth_type: jint, secret: &str) -> Auth {
    match auth_type {
        1 => Auth::Token(secret.to_string()),
        2 => {
            // secret = "user:password"
            let (user, pass) = secret
                .split_once(':')
                .map(|(u, p)| (u.to_string(), p.to_string()))
                .unwrap_or_else(|| (secret.to_string(), String::new()));
            Auth::Basic { user, pass }
        }
        _ => Auth::None,
    }
}

/// Map a [`GiteaError`] onto the Java exception hierarchy and throw it on
/// the current thread. Best-effort: if `throw_new` itself fails (e.g. class
/// not found), the error is silently dropped and the caller returns a null
/// / zero value, which surfaces as a `NullPointerException` or similar on
/// the Java side. This matches the JNI convention: a thrown exception is
/// pending, the native method returns a sentinel, and Java rethrows on the
/// next JNI boundary.
fn throw_gitea_exception(env: &mut JNIEnv, err: &GiteaError) {
    let (class_name, msg) = match err {
        GiteaError::HttpStatus {
            status,
            message,
            body,
        } => (
            "org/jenkinsci/plugin/gitea/client/api/GiteaHttpStatusException",
            // GiteaHttpStatusException(int, String, String) formats the
            // message itself; we pass the body via the (statusCode,
            // statusMessage, responseBody) constructor by encoding it into
            // the message that `throw_new` will feed into the 2-arg
            // `(int, String)` form. To preserve the body we instead emit
            // the upstream's human-readable composite string.
            format!(
                "HTTP {}/{}{}",
                status,
                message,
                body.as_deref()
                    .map(|b| format!("\n{}", b))
                    .unwrap_or_default()
            ),
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
    // Use the (class, message) 2-arg form. The exception carries the
    // composite message; GiteaHttpStatusException will then expose status
    // info only if Java catches and re-wraps it. For the MVP this is
    // sufficient — see IMPLEMENTATION_PLAN.md for the richer constructor
    // future work.
    let _ = env.throw_new(class_name, &msg);
}

/// Build a `GiteaClient` from the (serverUrl, authType, authSecret) triple.
/// Split into a helper because every export does exactly this.
fn build_client(server_url: &str, auth_type: jint, secret: &str) -> Result<GiteaClient, GiteaError> {
    let auth = decode_auth(auth_type, secret);
    GiteaClient::new(server_url, auth)
}

/// Convert a JSON `String` into a raw `jstring`. Returns a null pointer on
/// failure (the JVM will then see the pending exception, if any).
fn json_to_jstring(env: &mut JNIEnv, json: String) -> jstring {
    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Convert a `Vec<u8>` into a `jbyteArray`. Returns a null pointer on
/// failure. Used by `fetchFile`.
fn bytes_to_jbytearray(env: &mut JNIEnv, bytes: Vec<u8>) -> jbyteArray {
    match env.byte_array_from_slice(&bytes) {
        Ok(arr) => arr.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// 1. /version
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchVersion(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_version().await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 2. /user, /users/{name}, /orgs/{name}, fetchOwner fallback
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCurrentUser(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_current_user().await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchUser(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    name: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let name = match jstr(&mut env, name) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_user(&name).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchOrganization(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    name: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let name = match jstr(&mut env, name) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_organization(&name).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchOwner(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    name: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let name = match jstr(&mut env, name) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_owner(&name).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Repositories
// ---------------------------------------------------------------------------

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

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCurrentUserRepositories(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_current_user_repositories().await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchRepositories(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    user: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let user = match jstr(&mut env, user) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_repositories(&user).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchOrganizationRepositories(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    org: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let org = match jstr(&mut env, org) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_organization_repositories(&org).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Branches
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchBranch(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    branch: JString,
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
    let branch = match jstr(&mut env, branch) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_branch(&owner, &repo, &branch).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchBranches(
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
        client.fetch_branches(&owner, &repo).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Tags
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchAnnotatedTag(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    sha: JString,
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
    let sha = match jstr(&mut env, sha) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_annotated_tag(&owner, &repo, &sha).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchTag(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    tag: JString,
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
    let tag = match jstr(&mut env, tag) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_tag(&owner, &repo, &tag).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchTags(
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
        client.fetch_tags(&owner, &repo).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Commits
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCommit(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    sha: JString,
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
    let sha = match jstr(&mut env, sha) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_commit(&owner, &repo, &sha).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Collaborators
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCollaborators(
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
        client.fetch_collaborators(&owner, &repo).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCheckCollaborator(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    user: JString,
) -> jboolean {
    // 0 = false, 1 = true in the JNI convention.
    const JNI_FALSE: jboolean = 0;
    const JNI_TRUE: jboolean = 1;

    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return JNI_FALSE,
    };
    let owner = match jstr(&mut env, owner) {
        Ok(s) => s,
        Err(_) => return JNI_FALSE,
    };
    let repo = match jstr(&mut env, repo) {
        Ok(s) => s,
        Err(_) => return JNI_FALSE,
    };
    let user = match jstr(&mut env, user) {
        Ok(s) => s,
        Err(_) => return JNI_FALSE,
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.check_collaborator(&owner, &repo, &user).await
    });

    match result {
        Ok(b) => {
            if b {
                JNI_TRUE
            } else {
                JNI_FALSE
            }
        }
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            JNI_FALSE
        }
    }
}

// ---------------------------------------------------------------------------
// 8. Organization hooks
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchHooksOrg(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    org: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let org = match jstr(&mut env, org) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_hooks_org(&org).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCreateHookOrg(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    org: JString,
    body: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let org = match jstr(&mut env, org) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let body = match jstr(&mut env, body) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.create_hook_org(&org, &body).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeDeleteHookOrg(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    org: JString,
    id: jlong,
) {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return,
    };
    let org = match jstr(&mut env, org) {
        Ok(s) => s,
        Err(_) => return,
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.delete_hook_org(&org, id).await
    });

    if let Err(e) = result {
        throw_gitea_exception(&mut env, &e);
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeUpdateHookOrg(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    org: JString,
    id: jlong,
    body: JString,
) -> jstring {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let org = match jstr(&mut env, org) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let body = match jstr(&mut env, body) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.update_hook_org(&org, id, &body).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 9. Repository hooks
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchHooksRepo(
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
        client.fetch_hooks_repo(&owner, &repo).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCreateHookRepo(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    body: JString,
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
    let body = match jstr(&mut env, body) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.create_hook_repo(&owner, &repo, &body).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeDeleteHookRepo(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    id: jlong,
) {
    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return,
    };
    let owner = match jstr(&mut env, owner) {
        Ok(s) => s,
        Err(_) => return,
    };
    let repo = match jstr(&mut env, repo) {
        Ok(s) => s,
        Err(_) => return,
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.delete_hook_repo(&owner, &repo, id).await
    });

    if let Err(e) = result {
        throw_gitea_exception(&mut env, &e);
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeUpdateHookRepo(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    id: jlong,
    body: JString,
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
    let body = match jstr(&mut env, body) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.update_hook_repo(&owner, &repo, id, &body).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 10. Commit statuses
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCommitStatuses(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    sha: JString,
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
    let sha = match jstr(&mut env, sha) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_commit_statuses(&owner, &repo, &sha).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCreateCommitStatus(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    sha: JString,
    body: JString,
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
    let sha = match jstr(&mut env, sha) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let body = match jstr(&mut env, body) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.create_commit_status(&owner, &repo, &sha, &body).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 11. Pull requests
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchPullRequest(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    number: jlong,
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
        client.fetch_pull_request(&owner, &repo, number).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchPullRequests(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    state: JString,
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
    // Empty string from Java means "no state filter" (matches Option::<&str>::None).
    let state_raw = jstr_or_empty(&mut env, state);
    let state_opt: Option<&str> = if state_raw.is_empty() {
        None
    } else {
        Some(state_raw.as_str())
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_pull_requests(&owner, &repo, state_opt).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 12. Issues
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchIssues(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    state: JString,
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
    let state_raw = jstr_or_empty(&mut env, state);
    let state_opt: Option<&str> = if state_raw.is_empty() {
        None
    } else {
        Some(state_raw.as_str())
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_issues(&owner, &repo, state_opt).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 13. Files (raw)
// ---------------------------------------------------------------------------

/// Returns the raw file content as a `byte[]`. The Java contract is
/// `byte[] fetchFile(GiteaRepository, String ref, String path)`, so binary
/// content must survive unmodified.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchFile(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    ref_: JString,
    path: JString,
) -> jbyteArray {
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
    let ref_ = match jstr(&mut env, ref_) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let path = match jstr(&mut env, path) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_file_bytes(&owner, &repo, &ref_, &path).await
    });

    match result {
        Ok(bytes) => bytes_to_jbytearray(&mut env, bytes),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

/// `checkFile`: existence check via HEAD on the `/raw/` endpoint.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCheckFile(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    ref_: JString,
    path: JString,
) -> jboolean {
    const JNI_FALSE: jboolean = 0;
    const JNI_TRUE: jboolean = 1;

    let server_url = match jstr(&mut env, server_url) {
        Ok(s) => s,
        Err(_) => return JNI_FALSE,
    };
    let owner = match jstr(&mut env, owner) {
        Ok(s) => s,
        Err(_) => return JNI_FALSE,
    };
    let repo = match jstr(&mut env, repo) {
        Ok(s) => s,
        Err(_) => return JNI_FALSE,
    };
    let ref_ = match jstr(&mut env, ref_) {
        Ok(s) => s,
        Err(_) => return JNI_FALSE,
    };
    let path = match jstr(&mut env, path) {
        Ok(s) => s,
        Err(_) => return JNI_FALSE,
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.check_file(&owner, &repo, &ref_, &path).await
    });

    match result {
        Ok(b) => {
            if b {
                JNI_TRUE
            } else {
                JNI_FALSE
            }
        }
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            JNI_FALSE
        }
    }
}

// ---------------------------------------------------------------------------
// 14. Releases
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchReleases(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    draft: jboolean,
    prerelease: jboolean,
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
    // jboolean: 0 = false, anything else (conventionally 1) = true.
    let draft = draft != 0;
    let prerelease = prerelease != 0;

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client.fetch_releases(&owner, &repo, draft, prerelease).await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 15. Release attachments (multipart upload)
// ---------------------------------------------------------------------------

/// Multipart upload. `data` is the file content as a `byte[]`.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCreateReleaseAttachment(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,
    auth_secret: JString,
    owner: JString,
    repo: JString,
    release_id: jlong,
    name: JString,
    data: JByteArray,
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
    let name = match jstr(&mut env, name) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let data: Vec<u8> = match env.convert_byte_array(&data) {
        Ok(v) => v,
        Err(_) => return std::ptr::null_mut(),
    };
    let secret = jstr_or_empty(&mut env, auth_secret);

    let result = RT.block_on(async {
        let client = build_client(&server_url, auth_type, &secret)?;
        client
            .create_release_attachment(&owner, &repo, release_id, &name, data)
            .await
    });

    match result {
        Ok(json) => json_to_jstring(&mut env, json),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// 16. TLS trust material (stage 12)
// ---------------------------------------------------------------------------

/// `Java_…_RustGiteaConnection_nativeSetTrustedCertificates` — install
/// additional PEM-encoded CA certificates into the process-wide trust store.
///
/// The Java side (`RustGiteaConnection.nativeSetTrustedCertificates(byte[])`)
/// is invoked once during plugin initialisation (from
/// `GiteaServers.configure()` when the operator has filled in the
/// "Trusted certificates (PEM)" field, or from `WebhookServerStarter.doExecute`
/// on Jenkins startup).
///
/// Behaviour:
/// * `pem == null` or `pem.length == 0` — clears the slot (only Mozilla CA
///   will be trusted).
/// * Otherwise the bytes are stored in a `OnceCell<Option<Arc<Vec<u8>>>>`
///   (see [`crate::tls_store`]). Subsequent calls are silently ignored —
///   `OnceCell` semantics — because the trust material is not expected to
///   change without a Jenkins restart (see `AGENTS.md` "known limitations").
///
/// This export is deliberately infallible at the JNI boundary: a parse
/// error inside `set_extra_pem` cannot happen (we store raw bytes), and
/// surfacing the "already initialised" case as a Java exception would
/// break the `configure()` save loop.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeSetTrustedCertificates(
    env: JNIEnv,
    _cls: JClass,
    pem: JByteArray,
) {
    // A null byte[] is treated identically to an empty one.
    let pem_bytes: Vec<u8> = if pem.is_null() {
        Vec::new()
    } else {
        match env.convert_byte_array(&pem) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "nativeSetTrustedCertificates: convert_byte_array failed");
                return;
            }
        }
    };
    let pem_opt = if pem_bytes.is_empty() {
        None
    } else {
        Some(pem_bytes)
    };
    crate::tls_store::set_extra_pem(pem_opt);
}

// ---------------------------------------------------------------------------
// 17. HTTP proxy configuration (stage 13)
// ---------------------------------------------------------------------------

/// `Java_…_RustGiteaConnection_nativeSetProxy` — install the
/// process-wide HTTP/HTTPS/SOCKS5 proxy used by all outbound Gitea
/// requests.
///
/// The Java side (`RustGiteaConnection.nativeSetProxy(String)`) is invoked
/// once during plugin initialisation from `GiteaServers.configure()` with
/// a JSON document produced by `GiteaServers.buildProxyJson()`.
///
/// Behaviour:
/// * `configJson == null` / empty / not valid JSON / a config with an
///   empty `url` — clears the slot, falling back to reqwest's env-var
///   lookup (`HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY`).
/// * Otherwise the parsed [`crate::proxy::ProxyConfig`] is stashed in a
///   `OnceLock<Option<Arc<ProxyConfig>>>`. Subsequent calls are silently
///   ignored — `OnceLock` semantics — because the proxy is not expected
///   to change without a Jenkins restart (see `AGENTS.md` "known
///   limitations").
///
/// Like `nativeSetTrustedCertificates`, this export is deliberately
/// infallible at the JNI boundary: a parse error is logged at WARN and
/// the proxy slot is left unset (i.e. env-var fallback) — surfacing it as
/// a Java exception would break the `configure()` save loop.
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeSetProxy(
    mut env: JNIEnv,
    _cls: JClass,
    config_json: JString,
) {
    let json = jstr_or_empty(&mut env, config_json);
    if json.is_empty() {
        crate::proxy::set_proxy(None);
        return;
    }
    match serde_json::from_str::<crate::proxy::ProxyConfig>(&json) {
        Ok(cfg) => {
            if cfg.is_empty() {
                crate::proxy::set_proxy(None);
            } else {
                crate::proxy::set_proxy(Some(cfg));
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "nativeSetProxy: invalid ProxyConfig JSON, proxy disabled");
            crate::proxy::set_proxy(None);
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests for helpers (JNI itself needs a JVM and is not exercised here)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_auth_none() {
        assert!(matches!(decode_auth(0, ""), Auth::None));
        assert!(matches!(decode_auth(99, "weird"), Auth::None));
    }

    #[test]
    fn decode_auth_token() {
        match decode_auth(1, "abc123") {
            Auth::Token(t) => assert_eq!(t, "abc123"),
            other => panic!("expected Token, got {:?}", other),
        }
    }

    #[test]
    fn decode_auth_basic_with_colon() {
        match decode_auth(2, "alice:s3cr3t") {
            Auth::Basic { user, pass } => {
                assert_eq!(user, "alice");
                assert_eq!(pass, "s3cr3t");
            }
            other => panic!("expected Basic, got {:?}", other),
        }
    }

    #[test]
    fn decode_auth_basic_without_colon_falls_back_to_empty_pass() {
        // Defensive: if the Java side ever produces a secret without a `:`,
        // we treat the whole string as the username with an empty password.
        match decode_auth(2, "alice") {
            Auth::Basic { user, pass } => {
                assert_eq!(user, "alice");
                assert_eq!(pass, "");
            }
            other => panic!("expected Basic, got {:?}", other),
        }
    }

    #[test]
    fn decode_auth_basic_with_multiple_colons_keeps_colon_in_password() {
        // Passwords may legally contain `:`. We split on the FIRST colon
        // only.
        match decode_auth(2, "alice:pass:with:colons") {
            Auth::Basic { user, pass } => {
                assert_eq!(user, "alice");
                assert_eq!(pass, "pass:with:colons");
            }
            other => panic!("expected Basic, got {:?}", other),
        }
    }
}

// Silence unused-import warning for `JByteArray` / `jbyte` when the
// corresponding export is conditionally compiled out. Both are always used
// in the current build, but we keep the directive to future-proof against
// refactors that gate the multipart export behind a feature flag.
#[allow(dead_code)]
fn _type_assertions(_b: jbyte, _a: JByteArray) {}
