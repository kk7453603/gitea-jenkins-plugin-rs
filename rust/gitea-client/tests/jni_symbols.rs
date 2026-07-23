//! Verifies that the Rust crate exports the JNI symbols expected by the Java
//! shim class {@code RustGiteaConnection}.
//!
//! This is a build-time contract check: it opens the compiled native library
//! and confirms a representative sample of {@code Java_org_jenkinsci_plugin_...}
//! entry points are present. It does not call into the JVM, so it can run as a
//! plain {@code cargo test} without a running Jenkins or mock HTTP server.
//!
//! Behaviour:
//! * If the native library has not been built yet, the test is silently
//!   skipped (with a stderr note). This keeps {@code cargo test} green in
//!   checkouts where the user only built the rlib for the integration suite.
//! * On platforms other than Linux/macOS the test is skipped as well.

use libloading::Library;

/// Returns the path to the compiled cdylib for the host target, or {@code None}
/// on platforms we do not ship a native artifact for.
fn native_lib_path() -> Option<std::path::PathBuf> {
    let filename = if cfg!(target_os = "linux") {
        "libgitea_rust.so"
    } else if cfg!(target_os = "macos") {
        "libgitea_rust.dylib"
    } else {
        return None;
    };

    // Tests run with `CARGO_MANIFEST_DIR` as the crate root. Resolve the lib
    // from `target/release` (the profile Jenkins CI builds) and fall back to
    // `target/debug` so `cargo test --no-run` against a debug build still works.
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let release = manifest.join("target").join("release").join(filename);
    if release.exists() {
        return Some(release);
    }
    let debug = manifest.join("target").join("debug").join(filename);
    if debug.exists() {
        return Some(debug);
    }
    None
}

/// Symbols expected by {@code RustGiteaConnection}'s {@code native*} methods,
/// plus the lifecycle exports for the stage 9.A webhook server
/// ({@code RustWebhookDispatcher}). Keep in sync with {@code src/jni.rs},
/// {@code src/jni_webhook.rs} and the Java {@code private native}
/// declarations.
const EXPECTED_SYMBOLS: &[&str] = &[
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchVersion",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCurrentUser",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchUser",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchOrganization",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchOwner",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchRepository",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchRepositories",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchBranch",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchBranches",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchAnnotatedTag",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchTag",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchTags",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCommit",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCollaborators",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCheckCollaborator",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchHooksOrg",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCreateHookOrg",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeDeleteHookOrg",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeUpdateHookOrg",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchHooksRepo",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCreateHookRepo",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeDeleteHookRepo",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeUpdateHookRepo",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchCommitStatuses",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCreateCommitStatus",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchPullRequest",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchPullRequests",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchIssues",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchFile",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCheckFile",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchReleases",
    "Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeCreateReleaseAttachment",
    // --- Stage 9.A: webhook server lifecycle ---
    "Java_org_jenkinsci_plugin_gitea_webhook_RustWebhookDispatcher_nativeStart",
    "Java_org_jenkinsci_plugin_gitea_webhook_RustWebhookDispatcher_nativeStop",
];

#[test]
fn jni_exports_are_present() {
    let lib_path = match native_lib_path() {
        Some(p) => p,
        None => {
            eprintln!(
                "jni_exports_are_present: native lib not built for this target \
                 ({}) or not yet compiled — skipping",
                std::env::consts::OS
            );
            return;
        }
    };

    let lib = unsafe {
        Library::new(&lib_path).unwrap_or_else(|e| {
            panic!(
                "failed to dlopen {}: {}",
                lib_path.display(),
                e
            )
        })
    };

    for symbol in EXPECTED_SYMBOLS {
        unsafe {
            let sym: libloading::Symbol<unsafe extern "system" fn()> = lib
                .get(symbol.as_bytes())
                .unwrap_or_else(|e| panic!("missing JNI export `{}`: {}", symbol, e));
            // Touch the symbol so it can't be dead-code eliminated and so the
            // Symbol binding survives to this line.
            let _addr = *sym as *const ();
            assert!(!_addr.is_null(), "symbol `{}` resolved to NULL", symbol);
        }
    }

    println!(
        "jni_exports_are_present: verified {} JNI symbols in {}",
        EXPECTED_SYMBOLS.len(),
        lib_path.display()
    );
}
