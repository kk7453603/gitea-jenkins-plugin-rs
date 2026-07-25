# Jenkins Gitea Plugin (Rust-accelerated)

[![Version](https://img.shields.io/badge/version-1.3.0-blue.svg)](https://github.com/kk7453603/gitea-jenkins-plugin-rs/releases/tag/v1.3.0)
[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE.txt)
[![Jenkins](https://img.shields.io/badge/Jenkins-LTS%202.479%2B-blue.svg)](https://www.jenkins.io/download/)

A fork of [jenkinsci/gitea-plugin](https://github.com/jenkinsci/gitea-plugin) (upstream commit `ae31972`) with the Gitea HTTP client **and** webhook receiver rewritten in Rust for better performance, memory safety, and security isolation. The Java plugin architecture is preserved — only the `GiteaConnection` implementation is replaced. The plugin still produces a single `gitea.hpi` that drops into any Jenkins controller (Linux x86_64 or aarch64).

**Jump to**: [Architecture](docs/ARCHITECTURE.md) · [Migration](docs/MIGRATION.md) · [Production guide](docs/PRODUCTION.md) · [Corporate migration](corporate-migration/) · [Agent skills](agent-skills/) · [Changelog](CHANGES.md)

---

## What's new (v1.3.0)

| Feature | Where | What it does |
|---|---|---|
| **HTTP client rewritten in Rust** | `rust/gitea-client/` | 33 async Gitea API methods via reqwest + tokio |
| **Webhook receiver in Rust** | `server.rs` (axum on `:8081`) | Separate port for HMAC-authenticated webhooks |
| **HMAC-SHA256 + bearer + CIDR + rate limit** | `server.rs`, `rate_limiter.rs` | Defence-in-depth security pipeline |
| **Idempotency (replay protection)** | LRU cache 2048 entries | Dedup by `X-Gitea-Delivery` header |
| **TLS trust store** | `tls.rs`, UI field `trustedCertificatesPem` | Self-signed Gitea / corporate CA support |
| **HTTP proxy** | `proxy.rs`, UI field `proxyUrl` + Jenkins proxy fallback | Corporate proxy compatibility |
| **Custom webhook path** | UI field `webhookPath` | Reverse-proxy path override |
| **External webhook URL** | UI field `webhookExternalUrl` | NAT / reverse-proxy URL override |
| **Connection pool** | `pool.rs` (TTL+LRU, max 32) | TLS session reuse across requests |
| **Polling scheduler** | `polling.rs` (ETag-based) | Fallback if webhooks fail |
| **Prometheus metrics** | `GET :8081/metrics` | `gitea_webhook_requests_total{event_type,status}` + latency histogram |
| **Health endpoint** | `GET :8081/health` | Kubernetes liveness probe target |
| **Rust→JUL log bridge** | `log_bridge.rs`, `RustLogReceiver.java` | Rust `tracing` events visible in Jenkins System Log |
| **Multi-arch .so** | `META-INF/native/linux/{amd64,aarch64}/` | Single `.hpi` works on x86_64 + Apple Silicon / Graviton |
| **Hot-reload fix** | `GiteaPluginLifecycle.java` (`Plugin.stop()` → `nativeStop`) | Tokio threads cleaned up on plugin unload |
| **Auto-start** | `@Initializer(after=EXTENSIONS_AUGMENTED)` | Webhook server starts on boot, no manual trigger |

---

## Architecture (TL;DR)

For deep dive see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) with C4 + sequence diagrams + header processing pipeline.

```
Gitea server
   │
   │ POST :8081/gitea-webhook/post
   │ + X-Gitea-Signature: HMAC-SHA256
   │ + Authorization: Bearer <token>
   ▼
┌────────────────────────────────────────────────────────────────┐
│ Jenkins Controller (JVM 21)                                    │
│                                                                │
│  :8080  Jenkins UI (Stapler / Jetty)                           │
│  :8081  Rust webhook server (axum, separate port)              │
│  :50000 Jenkins agent protocol                                 │
│                                                                │
│  libgitea_rust.so (~5 MB, 41 JNI symbols, multi-arch)          │
│    ├── axum HTTP server + HMAC + rate limit + dedup            │
│    ├── reqwest HTTP client + connection pool                   │
│    ├── tokio runtime (1 per process)                           │
│    ├── Prometheus / health endpoints                           │
│    └── tracing → java.util.logging bridge                      │
│                                                                │
│  ~95 upstream Java classes UNTOUCHED (SCM, traits, POJOs, UI)  │
│  ServiceLoader SPI → RustGiteaConnectionFactory                │
└────────────────────────────────────────────────────────────────┘
         ↑                                   ↑
         │ outbound HTTPS                    │ outbound HTTPS
         │ (token auth)                      │ (token auth)
    Gitea API                            Gitea API
```

The plugin uses the upstream `ServiceLoader SPI` (`GiteaConnectionFactory`)
as the only integration point — ~95 Java classes from upstream stay
byte-identical.

The HTTP client logic (~1200 lines in upstream `DefaultGiteaConnection.java`) is replaced by a Rust crate at `rust/gitea-client/` that uses `reqwest` + `tokio`. Java calls into it via JNI; Rust returns raw JSON strings that Java deserializes through the existing Jackson `ObjectMapper`. The 41 Gitea POJOs are not duplicated on the Rust side.

**For the full component breakdown, sequence diagrams, C4 model, header processing pipeline, and hook type mapping, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).**

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
| **Jenkins versions** | 2.479.3+ LTS, 2.504.x LTS, 2.528.x LTS (default), weekly. See [`docs/MIGRATION.md`](docs/MIGRATION.md) for rebuild instructions. |
| **Plugin interactions** | 100% API-compatible — same POJOs, same Jelly UI, same SCM behaviour, same webhook contract |
| **Operating systems** | Linux x86_64 + Linux aarch64 (both bundled in single `.hpi`). macOS arm64 supported for development (`.dylib`). Windows not yet supported. |
| **Gitea API** | Same coverage as upstream `DefaultGiteaConnection`: 33 endpoints |
| **Hot-reload** | Supported since v1.3.0 via `GiteaPluginLifecycle.stop()` → `nativeStop()`. Tokio runtime cleanly torn down on plugin unload. A JVM shutdown hook also covers SIGKILL/SIGTERM cases. |
| **HTTP proxy** | Supported since v1.0.0 via `GiteaServers.proxyUrl` + Jenkins global proxy fallback. Supports `http://`, `https://`, `socks5://`, `socks5h://`. |
| **TLS / self-signed certs** | Supported since v1.0.0 via `trustedCertificatesPem` (corporate CA in PEM format). |

## Limitations

- **mTLS outbound** (Jenkins → Gitea client cert) — only server cert verification is supported via `trustedCertificatesPem`. Client cert for mutual TLS is a future enhancement.
- **Windows Jenkins controllers** — only `linux/amd64` and `linux/aarch64` `.so` are bundled. macOS `.dylib` is built for dev only, no `.dll` for Windows.
- **Webhook signature schemes other than HMAC-SHA256** — Gitea's default. RSA-SHA256 / Ed25519 not yet wired in.
- **Cluster mode (HA Jenkins)** — Tokio runtime + HMAC secret are per-process. Multi-controller Jenkins needs shared state (Redis / DB) — not supported.
- **Durable webhook retry queue** — relies on Gitea retry. If Gitea gives up after N attempts, the event is lost. In-memory LRU dedup (2048 entries) absorbs retries but does not survive Jenkins restart.
- **Issue / PR comment events** — `issues` event type is accepted (200 OK) but ignored (no Jenkins-SCM semantic equivalent).
- **No fallback** — there is no Java-side `DefaultGiteaConnection` fallback. If the native library fails to load, the plugin will not function. This is a deliberate choice.

> **Note:** Jenkins HTTP proxy **and** hot-reload are now fully supported (v1.0.0 and v1.3.0 respectively):
> - Proxy: `GiteaServers.proxyUrl` + Jenkins global proxy fallback (`buildProxyJson()`).
> - Hot-reload: `GiteaPluginLifecycle.stop()` calls `nativeStop()` to tear down the Tokio runtime on plugin unload; a JVM shutdown hook covers SIGKILL/SIGTERM cases.
>
> See [`docs/ARCHITECTURE.md` §11](./docs/ARCHITECTURE.md#11-whats-not-in-scope-v1x) for the full out-of-scope list.

## Differences from upstream

- **Removed:** `DefaultGiteaConnection.java`, `DefaultGiteaConnectionFactory.java`
- **Added:** `RustGiteaConnection.java`, `RustGiteaConnectionFactory.java`, `NativeLibraryLoader.java`
- **Added:** `rust/gitea-client/` Rust crate (`Cargo.toml`, `src/{lib,client,auth,error,runtime}.rs`, `tests/integration.rs`)
- **Changed:** `META-INF/services/org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory` now registers `RustGiteaConnectionFactory`
- **Changed:** `pom.xml` adds `exec-maven-plugin` (cargo build) and extends `maven-resources-plugin` (bundle `.so`)
- **Changed:** `Jenkinsfile` adds a "Build Rust" stage on a Linux `amd64` agent

All other ~100 Java classes are unchanged from upstream `ae31972`. The 41 Gitea POJOs, the Jelly UI, the SCM traits, and the webhook handlers are byte-for-byte identical to upstream.

---

## Corporate migration (for AI agents and ops)

If you have a **corporate-customized** fork of `jenkinsci/gitea-plugin` (with custom headers, proxy modes, audit sinks, or auth schemes) and want to migrate to this Rust-accelerated version:

➡️ **Read [`corporate-migration/AGENTS.md`](corporate-migration/AGENTS.md)** — operating manual for AI agents (pi, qwen, claude, etc.) tasked with the migration.

The `corporate-migration/` directory contains:

| File | Purpose |
|---|---|
| [`AGENTS.md`](corporate-migration/AGENTS.md) | Entry point for AI agents — constraints, DO-NOT-TOUCH list, workflow |
| [`JNI-EXTENSIONS.md`](corporate-migration/JNI-EXTENSIONS.md) | How to add new Rust↔Java bridges safely |
| [`HEADER-MIGRATION.md`](corporate-migration/HEADER-MIGRATION.md) | Porting custom header checks (inbound + outbound) |
| [`PROXY-MIGRATION.md`](corporate-migration/PROXY-MIGRATION.md) | Porting corporate proxy configurations (7 patterns) |
| [`CHECKLIST.md`](corporate-migration/CHECKLIST.md) | Step-by-step migration workflow (5 phases, 4-12 hours) |
| [`examples/custom-header-injection.md`](corporate-migration/examples/custom-header-injection.md) | Template: add `X-Corp-Token` check (30 min) |
| [`examples/multi-proxy.md`](corporate-migration/examples/multi-proxy.md) | Template: per-host proxy routing (2-3 hours) |
| [`examples/custom-auth.md`](corporate-migration/examples/custom-auth.md) | Template: OAuth token refresh (4-6 hours) |
| [`examples/audit-sink.md`](corporate-migration/examples/audit-sink.md) | Template: webhook → SIEM forwarding (1-2 hours) |

**Key constraint:** the JNI integration (41 native methods in `RustGiteaConnection`, `jni.rs`, `runtime.rs`, `NativeLibraryLoader`) is load-bearing — DO NOT modify. Corporate customizations go in extension points (header pipeline, proxy module, new JNI bridges in `jni_corp.rs`).

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
