# Jenkins Gitea Plugin (Rust-accelerated)

[![Version](https://img.shields.io/jenkins/plugin/v/gitea.svg?label=version)](https://plugins.jenkins.io/gitea)
[![Changelog](https://img.shields.io/github/v/release/jenkinsci/gitea-plugin.svg?label=changelog)](https://github.com/jenkinsci/gitea-plugin/releases/latest)
[![Installs](https://img.shields.io/jenkins/plugin/i/gitea.svg?color=blue)](https://plugins.jenkins.io/gitea)

This is a fork of [jenkinsci/gitea-plugin](https://github.com/jenkinsci/gitea-plugin) (upstream commit `ae31972`) with the Gitea HTTP client rewritten in Rust for better performance and memory safety. The Java plugin architecture is preserved — only the `GiteaConnection` implementation is replaced. The plugin still produces a single `gitea.hpi` that drops into any Jenkins controller.

For details on the original plugin see [plugins.jenkins.io/gitea](https://plugins.jenkins.io/gitea).

## Architecture

The plugin is built around the `org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory` ServiceLoader SPI. Upstream already uses this SPI to swap implementations (e.g. a mock factory in tests), which means the HTTP client can be replaced without touching any of the ~100 SCM, trait, event, webhook, or UI classes.

```
Jenkins Controller
└── gitea.hpi  (single deployable artifact)
    ├── ~100 Java classes (UNCHANGED from upstream)
    │   └── SCM, traits, events, webhook handlers, 41 POJOs, Jelly templates
    ├── RustGiteaConnection.java           (thin JNI bridge)
    ├── RustGiteaConnectionFactory.java    (SPI implementation)
    ├── NativeLibraryLoader.java           (unpacks + loads libgitea_rust.so)
    ├── libgitea_rust.so                   (Rust cdylib, bundled)
    └── META-INF/services/...GiteaConnectionFactory
            → RustGiteaConnectionFactory
```

The HTTP client logic (~1200 lines in upstream `DefaultGiteaConnection.java`) is replaced by a Rust crate at `rust/gitea-client/` that uses `reqwest` + `tokio`. Java calls into it via JNI; Rust returns raw JSON strings that Java deserializes through the existing Jackson `ObjectMapper`. The 41 Gitea POJOs are not duplicated on the Rust side.

```
┌────────────────────────────────────────────────────────────────┐
│  GiteaSCMSource / GiteaWebhookListener / ...  (Java, unchanged)│
└──────────────────────────────┬─────────────────────────────────┘
                               │ uses
                               ▼
                interface GiteaConnection  (unchanged)
                               ▲
                               │ implements
┌──────────────────────────────┴─────────────────────────────────┐
│  RustGiteaConnection.java                                      │
│   - 33 native methods (one per Gitea API operation)            │
│   - static { NativeLibraryLoader.load("gitea_rust"); }         │
│   - Jackson ObjectMapper to parse JSON returned from Rust      │
└──────────────────────────────┬─────────────────────────────────┘
                               │ JNI
                               ▼
┌────────────────────────────────────────────────────────────────┐
│  libgitea_rust.so  (Rust cdylib)                               │
│   - reqwest async client (Lazy static, connection pooled)      │
│   - tokio Runtime (Lazy static)                                │
│   - 33 #[no_mangle] extern "system" fn exports                 │
│   - Auth: None / Token ("Authorization: token <T>") / Basic    │
│   - Returns raw JSON as JString                                │
└────────────────────────────────────────────────────────────────┘
```

## Why Rust?

- **Memory safety** — eliminates whole classes of CVEs common in HTTP client code (buffer handling, string parsing, deserialisation of untrusted input).
- **Performance** — async `reqwest` with HTTP/1.1 keep-alive and connection pooling; no GC pressure for transient request/response objects.
- **Testability** — pure Rust core with `wiremock`-based tests; no Jenkins boilerplate required to exercise the client.
- **Ecosystem alignment** — modern async HTTP stack (`tokio` + `reqwest` + `serde`), actively maintained crates.

## Requirements

- **Build toolchain:** JDK 21+, Rust toolchain (`cargo` + `rustup`), Linux x86_64 host (for production builds).
- **Runtime:** Jenkins controller running on Linux x86_64. macOS (`.dylib`) is supported for local development only, not for production deployments.
- **Jenkins baseline:** 2.479.3+ (unchanged from upstream).

## Build

```bash
# prerequisites
rustup toolchain install stable
mvn --version     # Jenkins build tool
java -version     # JDK 21+

# build the plugin (produces target/gitea.hpi)
mvn clean package
# → target/gitea.hpi
#   (includes libgitea_rust.so under META-INF/native/linux/amd64/)
```

The Maven build automatically invokes `cargo build --release` via `exec-maven-plugin` (phase `generate-resources`) and bundles the resulting `.so` into the `.hpi` via `maven-resources-plugin` (phase `process-resources`). You do not need to run `cargo` manually for a regular build.

## Development

```bash
# iterate on the Rust core independently of Jenkins
cd rust/gitea-client
cargo test           # integration tests via wiremock (no live Gitea required)
cargo build --release
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings

# full Maven build (Java plugin + Rust native lib)
cd ../..
mvn hpi:run          # launches a local Jenkins with the plugin loaded
```

The Rust crate is a standalone project; you can develop, test, and benchmark it without ever starting Jenkins.

## Compatibility

| Aspect | Status |
|---|---|
| **Jenkins versions** | 2.479.3+ (unchanged from upstream) |
| **Plugin interactions** | 100% API-compatible — same POJOs, same Jelly UI, same SCM behaviour, same webhook contract |
| **Operating systems** | Linux x86_64 only (production). macOS supported for development (`.dylib`). Windows / aarch64 not yet supported. |
| **Gitea API** | Same coverage as upstream `DefaultGiteaConnection`: 33 endpoints |
| **Hot-reload** | Not supported. Restart Jenkins after installing or updating the plugin. |

## Limitations (MVP)

- **Jenkins HTTP proxy** — upstream `DefaultGiteaConnection` uses `Jenkins.get().proxy`; the Rust side does not yet read this setting. Tracked as TODO in `rust/gitea-client/src/client.rs`.
- **Hot-reload** — the Tokio background runtime spawns worker threads that are not cleanly torn down on plugin unload. Restart Jenkins instead of hot-reloading the plugin.
- **Cross-platform** — only Linux x86_64 is shipped in production builds. Adding macOS / Windows / aarch64 requires building a per-platform `.so` / `.dylib` / `.dll` and extending `NativeLibraryLoader` to select the right one based on `os.name` / `os.arch`.
- **No fallback** — there is no Java-side `DefaultGiteaConnection` fallback. If the native library fails to load, the plugin will not function. This is a deliberate choice for the MVP.

## Differences from upstream

- **Removed:** `DefaultGiteaConnection.java`, `DefaultGiteaConnectionFactory.java`
- **Added:** `RustGiteaConnection.java`, `RustGiteaConnectionFactory.java`, `NativeLibraryLoader.java`
- **Added:** `rust/gitea-client/` Rust crate (`Cargo.toml`, `src/{lib,client,auth,error,runtime}.rs`, `tests/integration.rs`)
- **Changed:** `META-INF/services/org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory` now registers `RustGiteaConnectionFactory`
- **Changed:** `pom.xml` adds `exec-maven-plugin` (cargo build) and extends `maven-resources-plugin` (bundle `.so`)
- **Changed:** `Jenkinsfile` adds a "Build Rust" stage on a Linux `amd64` agent

All other ~100 Java classes are unchanged from upstream `ae31972`. The 41 Gitea POJOs, the Jelly UI, the SCM traits, and the webhook handlers are byte-for-byte identical to upstream.

## Webhook Configuration

This plugin uses a **separate HTTP server** (running inside `libgitea_rust.so`) to receive webhooks from Gitea. This is different from upstream — the Jenkins HTTP server is no longer the webhook receiver. The Rust listener (`rust/gitea-client/src/server.rs`) is an axum HTTP server that verifies HMAC-SHA256 signatures and dispatches payloads into the Jenkins `SCMEvent` bus via a JNI callback.

### Setup

1. In **Manage Jenkins → System → Gitea Servers**, set:
   - **Webhook listen port** (default `8081`) — TCP port for incoming webhooks.
   - **HMAC secret** (recommended) — shared secret for HMAC-SHA256 verification.
2. Open the port on your firewall / security group: `http://<jenkins-host>:8081/gitea-webhook/post` must be reachable from your Gitea server.
3. The plugin auto-registers hooks on Gitea via the `createHook` API call.

The webhook server is started once by `WebhookServerStarter` (an `AsyncPeriodicWork` extension) shortly after Jenkins finishes booting. It is restarted automatically when the Gitea global config is saved with a changed port or secret.

### URL format

```
http://<jenkins-host>:<webhookPort>/gitea-webhook/post
```

Example: `http://jenkins.internal:8081/gitea-webhook/post`

### HMAC verification

If an HMAC secret is configured:

- All incoming webhooks MUST include the `X-Gitea-Signature: <hex sha256>` header.
- Without a valid signature the request is rejected with HTTP **401**.
- Configure the same secret in your Gitea webhook settings.

If the secret is empty:

- HMAC verification is skipped (**NOT** recommended for production).
- A warning is logged on the Rust side.

The `X-Gitea-Event` request header determines the event type. Unknown types are acknowledged with HTTP 200 and logged at `FINE` level (no Jenkins action).

### Supported events

| Gitea event      | Jenkins action                                                |
|------------------|---------------------------------------------------------------|
| `push`           | `SCMHeadEvent.fireNow(GiteaPushSCMEvent)` — branch scan       |
| `pull_request`   | `SCMHeadEvent.fireNow(GiteaPullSCMEvent)` — PR scan           |
| `create`         | `SCMHeadEvent.fireNow(GiteaCreateSCMEvent)` — tag/branch create |
| `delete`         | `SCMHeadEvent.fireNow(GiteaDeleteSCMEvent)` — tag/branch delete |
| `release`        | `SCMHeadEvent.fireNow(GiteaReleaseSCMEvent)` — release event  |
| `repository`     | `SCMSourceEvent.fireNow(GiteaRepositorySCMEvent)` — repo metadata change |

Other events return `200` with a `FINE` log entry (no action). The full mapping lives in `RustWebhookDispatcher.dispatch()`.

### End-to-end testing

A bash script is bundled for smoke-testing the running webhook endpoint:

```bash
# against a Jenkins + plugin started via docker compose
./tests/e2e/webhook_e2e.sh 8081 my-secret
```

It covers five scenarios: push with/without HMAC, forged HMAC (must 401), unknown event type (must 200, no-op), and missing `X-Gitea-Event` header (must 400).

### Troubleshooting

- **HTTP 401 / 403 on webhook delivery**: the HMAC secret configured in Jenkins does not match the one in Gitea. Re-enter the secret on both sides and save.
- **Connection refused**: the webhook port is not exposed, or the Rust server has not finished starting. Check the `WebhookServerStarter` log entry in Jenkins system log.
- **Events delivered but no build triggered**: check the `RustWebhookDispatcher` log entries — a `FINE` "Ignoring unsupported Gitea webhook event type" line means the event type is not one of the six handled ones.
- **Port already in use**: pick a different `webhookPort` in the global config; the change takes effect on save without a Jenkins restart.

## License

MIT — see [LICENSE.txt](LICENSE.txt). Same license as upstream.

## Acknowledgements

Based on [jenkinsci/gitea-plugin](https://github.com/jenkinsci/gitea-plugin) by the Jenkins community. All SCM, webhook, trait, and UI code is their work; this fork only replaces the HTTP client implementation.
