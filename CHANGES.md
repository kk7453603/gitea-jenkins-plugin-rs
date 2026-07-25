# Changes

<!-- Each version newest first -->

<!-- Template:

## Version X.Y.Z (yyyy-MM-dd)

* details

-->

## [Unreleased] — Rust JNI rewrite

> This section combines the changes from the entire Rust rewrite (v1.0.0 → v1.3.0). For version-specific deltas see the per-version entries below.

### Breaking changes
* Removed `DefaultGiteaConnection` and `DefaultGiteaConnectionFactory`.
* Native library `libgitea_rust.so` is now required at runtime (bundled in the `.hpi` for Linux x86_64).
* The plugin no longer supports hot-reload — restart Jenkins after install or update.
* **Webhook URL changed**: from `<jenkins>/gitea-webhook/post` (served by the Jenkins Stapler HTTP server) to `<jenkins>:<port>/gitea-webhook/post` where `<port>` defaults to `8081`. The webhook listener now runs inside `libgitea_rust.so` as a separate axum HTTP server. **Update your Gitea webhook configuration after upgrade** and ensure the new port is reachable from your Gitea instance.

### Webhook layer (stage 9)
* Removed `GiteaWebhookAction` (the Stapler HTTP endpoint on the Jenkins side).
* Removed `GiteaWebhookHandler` (the Java payload parser).
* Removed the `HandlerImpl` nested classes from the six event classes (`GiteaPushSCMEvent`, `GiteaPullSCMEvent`, `GiteaCreateSCMEvent`, `GiteaDeleteSCMEvent`, `GiteaReleaseSCMEvent`, `GiteaRepositorySCMEvent`).
* Added `RustWebhookDispatcher` (`@Extension`) — receives JNI callbacks from the Rust webhook server and re-fires the payloads on the Jenkins `SCMEvent` bus.
* Added `WebhookServerStarter` (`@Extension` `AsyncPeriodicWork`) — starts the Rust server shortly after Jenkins finishes booting.
* Added `webhookPort` (default `8081`) and `webhookSecret` fields to the `GiteaServers` global config (with corresponding `config.jelly` controls).
* HMAC-SHA256 verification of the `X-Gitea-Signature` header is mandatory when a secret is configured, optional when the secret is empty.
* Added `tests/e2e/webhook_e2e.sh` — five-scenario end-to-end smoke test (push with/without HMAC, forged HMAC, unknown event, missing header).
* Added `RustWebhookDispatcherTest` — Java smoke test for `NativeLibraryLoader` double-load safety and `configure()` idempotency.

### Added
* Rust-based HTTP client at `rust/gitea-client/` covering all 33 Gitea API endpoints used by the plugin.
* `RustGiteaConnection`, `RustGiteaConnectionFactory`, and `NativeLibraryLoader` Java classes that bridge the existing `GiteaConnection` SPI to the Rust `cdylib` over JNI.
* Async HTTP via `reqwest` + `tokio` with a process-wide lazy `Runtime` and pooled `Client`.
* 49 `wiremock`-based integration tests plus 8 unit tests on the Rust side, runnable independently of Jenkins via `cargo test`.

### Changed
* `META-INF/services/org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory` now points at `RustGiteaConnectionFactory`.
* The Maven build now requires `cargo` on `PATH` (auto-invoked at the `generate-resources` phase via `exec-maven-plugin`).
* CI (`Jenkinsfile`) adds a "Build Rust" stage that runs `cargo build --release` and `cargo test` on a Linux `amd64` agent.

### Known limitations
* Jenkins HTTP proxy (`Jenkins.get().proxy`) is not yet honoured by the Rust client (TODO).
* Only Linux x86_64 is supported in production. macOS is supported for development only; Windows / aarch64 are not supported.

---

## Version 1.3.0 (2026-07-25) — corporate integration ready

### Added
* **Custom webhook path** (`GiteaServers.webhookPath`) — operator can override the default `/gitea-webhook` prefix to accommodate corporate reverse-proxy routing (e.g. `/jenkins/gitea-plugin/`).
* **Hot-reload fix** — new `GiteaPluginLifecycle` class extends `hudson.Plugin`, calls `nativeStop()` in `stop()` and registers a JVM shutdown hook for SIGKILL/SIGTERM cases. Fixes the v1.0/1.1 issue where tokio worker threads leaked after plugin reload.
* **Migration tooling** — three new scripts in `tools/`:
    * `migrate-from-upstream.sh` — backup `config.xml` + existing `.jpi`, build new `.hpi`, print operator checklist.
    * `rollback-to-upstream.sh` — stop Jenkins, restore upstream `.jpi` + `config.xml`, remove pinned marker, restart.
    * `smoke-test.sh` — five endpoint tests (health, metrics, 400 bad request, 401 wrong HMAC, 200 valid push).
* **`docs/MIGRATION.md`** — comprehensive migration playbook with version compatibility matrix (Jenkins LTS 2.479 / 2.504 / 2.528 + weekly).
* **`docs/ARCHITECTURE.md`** — canonical architecture reference with C4 model (context/container/component), 4 sequence diagrams (outbound API call, inbound webhook, plugin load, hot-reload), header processing pipeline, hook type → SCMEvent mapping.
* **`nativeStop`** method on `RustWebhookDispatcher` made public so cross-package callers (`GiteaPluginLifecycle`) can invoke it.

### Changed
* `pom.xml` now declares `<plugin-class>org.jenkinsci.plugin.gitea.GiteaPluginLifecycle</plugin-class>` so Jenkins core instantiates the Plugin class and calls `start()`/`stop()` on lifecycle events.
* `WebhookServerStarter` rewritten as `@Initializer(after=EXTENSIONS_AUGMENTED, before=JOB_LOADED)` instead of `AsyncPeriodicWork` with `Long.MAX_VALUE` recurrence — webhook server now starts deterministically on boot, no manual `configure()` needed.

---

## Version 1.2.0 (2026-07-24) — agent-friendly documentation

### Added
* **`AGENTS.md`** (489 lines) — comprehensive operating manual for AI agents adapting this plugin. 14 sections including: full architecture diagram, security threat model, 10 corporate customization points (CA, proxy, SSO, mTLS, RBAC, audit, secret rotation, URL obfuscation), 12-point corporate hardening checklist, "Known problems + fixes" lookup table (11 symptoms → root cause), agent workflow protocol.
* **`agent-skills/`** catalog (26 `SKILL.md` files):
    * `patterns/` (8): architectural patterns distilled from this project — `docker-rust-jenkins-multi-stage`, `jni-bridge-generator`, `json-over-jni-bridge`, `maven-cargo-integration`, `native-library-loader`, `parallel-multi-stage-orchestration`, `serviceloader-native-replacement`, `webhook-jni-callback-server`.
    * `core/` (7): TDD, verification-loop, continuous-agent-loop, prompt-optimizer, coding-standards, agent-harness-construction, token-budget-advisor.
    * `rust/` (2): rust-patterns, rust-testing.
    * `jenkins/` (5): java-coding-standards, hexagonal-architecture, backend-patterns, docker-patterns, e2e-testing.
    * `security/` (2): security-review, security-scan.
    * `watchmen/` (2): curator brief + setup.
* **`docs/PRODUCTION.md`** — ops playbook with nginx reverse-proxy config, firewall rules, backup strategy, Grafana alert PromQL, Kubernetes liveness probe YAML, troubleshooting matrix.

---

## Version 1.1.0 (2026-07-24) — production hardening

### Added
* **Connection pool** (`rust/gitea-client/src/pool.rs`) — `LazyLock<HashMap<key, PoolEntry>>` keyed by `(base_url, auth_hash)`. TTL eviction (5 min idle) + LRU overflow (max 32 entries). Auth secret is hashed, never logged in plaintext.
* **Health endpoint** (`GET :8081/gitea-webhook/health`) — `200 {"status":"ok"}` without auth. Kubernetes liveness probe target.
* **Prometheus metrics** (`GET :8081/gitea-webhook/metrics`) — `text/plain; version=0.0.4` exposition format.
    * `gitea_webhook_requests_total{event_type,status}` counter with labels: `ok / bad_request / unauthorized / rate_limited / forbidden / duplicate / error`.
    * `gitea_webhook_callback_latency_seconds{event_type}` histogram with buckets `[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0]`.
* **Idempotency dedup** (`DELIVERY_CACHE: LruCache<String, ()>` capacity 2048) — short-circuits duplicate `X-Gitea-Delivery` headers with `200 OK` without invoking the Java callback.
* **Rust→JUL log bridge** — `log_bridge.rs` tracing subscriber layer that forwards INFO/WARN/ERROR events to `RustLogReceiver.handleLog(level, target, message)` via JNI. Maps to `Logger.getLogger("org.jenkinsci.plugin.gitea." + target)` so Rust logs appear in Jenkins System Log UI.
* **Cross-platform native lib** — `META-INF/native/linux/{amd64,aarch64}/libgitea_rust.so` (both bundled in single `.hpi`). `NativeLibraryLoader` detects `os.arch` and tries the exact arch first, then falls back (e.g. aarch64 → amd64 via Rosetta).
* **`webhookExternalUrl` field** — operator can override the synthesized URL registered with Gitea. Useful when Jenkins sits behind nginx/Cloudflare/AWS ALB and the internal hostname is not reachable from Gitea.
* **lru** and **prometheus** crates added to `Cargo.toml`.

### Changed
* `server.rs` exposes `/gitea-webhook/health`, `/gitea-webhook/metrics`, and the existing `/post` endpoint.
* `client.rs` uses `pool::acquire()` instead of allocating a fresh `reqwest::Client` per call.
* `cleanup_loop` in `server.rs` now also evicts stale pool entries.

---

## Version 1.0.0 (2026-07-23) — production-ready MVP

### Breaking changes
* Removed `DefaultGiteaConnection` and `DefaultGiteaConnectionFactory` (~1200 lines).
* Native library `libgitea_rust.so` is now required at runtime (bundled in the `.hpi` for Linux x86_64).
* The plugin no longer supports hot-reload without restart.
* **Webhook URL changed**: from `<jenkins>/gitea-webhook/post` (served by the Jenkins Stapler HTTP server) to `<jenkins>:<port>/gitea-webhook/post` where `<port>` defaults to `8081`. The webhook listener now runs inside `libgitea_rust.so` as a separate axum HTTP server. **Update your Gitea webhook configuration after upgrade** and ensure the new port is reachable from your Gitea instance.

### Webhook layer
* Removed `GiteaWebhookAction` (the Stapler HTTP endpoint on the Jenkins side).
* Removed `GiteaWebhookHandler` (the Java payload parser).
* Removed the `HandlerImpl` nested classes from the six event classes.
* Added `RustWebhookDispatcher` (`@Extension`) — receives JNI callbacks from the Rust webhook server and re-fires the payloads on the Jenkins `SCMEvent` bus.
* Added `WebhookServerStarter` (`@Extension`) — starts the Rust server after Jenkins finishes booting.
* HMAC-SHA256 verification of the `X-Gitea-Signature` header (optional when secret is empty).
* `tests/e2e/webhook_e2e.sh` — five-scenario end-to-end smoke test.
* `RustWebhookDispatcherTest` — Java smoke test for `NativeLibraryLoader` double-load safety and `configure()` idempotency.

### Rust HTTP client
* 33 async Gitea API endpoints via `reqwest` + `tokio`.
* 49 `wiremock`-based integration tests + 8 unit tests on the Rust side.
* Process-wide lazy `Runtime` and pooled `Client`.

### Java-side bridge
* `RustGiteaConnection`, `RustGiteaConnectionFactory`, `NativeLibraryLoader` Java classes.
* `META-INF/services/...GiteaConnectionFactory` points at `RustGiteaConnectionFactory`.

### Stage 12 — TLS trust store
* `tls.rs` / `tls_store.rs` — custom PEM CA bundle appended on top of Mozilla CA.
* `GiteaServers.trustedCertificatesPem` field.
* Resolves self-signed Gitea cert / corporate CA rejections.

### Stage 13 — HTTP proxy
* `proxy.rs` — outbound HTTP/HTTPS/SOCKS5 proxy via `reqwest::Proxy`.
* `GiteaServers.{proxyUrl,proxyUsername,proxyPassword,noProxyHosts}` fields.
* Jenkins global proxy fallback in `buildProxyJson()`.

### Stage 10 — Polling scheduler
* `polling.rs` — adaptive ETag-based polling loop as fallback for when webhooks fail.
* Targets collected from `SCMSourceOwner → GiteaSCMSource` enumeration.

### Stage 16 — Auth extensions
* `rate_limiter.rs` — per-IP token bucket (configurable capacity, default 60/min).
* IP CIDR allowlist via `cidr` crate.
* Optional bearer token check.
* Cleanup task evicts idle buckets every 5 min.

### JNI bridge fix
* `nativeRegisterDispatcherClass` exports a `GlobalRef` to `RustWebhookDispatcher` class. Tokio worker threads use this ref instead of `env.find_class(...)` (which uses the system ClassLoader and cannot see plugin classes).

---



* Fix the case where the SSH URI port was not specified ([JENKINS-61996](https://issues.jenkins-ci.org/browse/JENKINS-61996))
* Propertly fetch tags ([JENKINS-61258](https://issues.jenkins-ci.org/browse/JENKINS-61258)) 
* Handle unknown pull request event payload actions ([JENKINS-61753](https://issues.jenkins-ci.org/browse/JENKINS-61753)) 
* Reluctantly adding `@Symbol` to the branch discovery traits ([JENKINS-60885](https://issues.jenkins-ci.org/browse/JENKINS-60885)). For anyone wondering why reluctantly... the idea behind `@Symbol` is that it is supposed to use type information to determine the set of possible candidates for a specific injection point. The current implementation of `@Symbol` support, however, decides to ignore type information from generics - even though that information is used elseweher in Jenkins and thus for the `@Symbol` case it gets confused and thinks that e.g. GitHub's `BranchDiscoveryTrait` is a viable candidate for injection into the Gitea SCM classes. As a result we cannot use default naming and thus have to add an explicit name. Even worse each SCM plugin has to prefix with their own names leading to an excess of `gitea` in configuration snippets. The final insult to injury is the naming conventions that have been followed. It pains me no end to have to add this workaround just because the symbol api maintainers refuse to add the type info filtering.
* Hopefully workaround NPE in branch discovery where there is a PR from a head that has already been deleted ([JENKINS-60825](https://issues.jenkins-ci.org/browse/JENKINS-60825))

## Version 1.2.0 (2020-02-17)

* Added basic setup documentation ([PR-13](https://github.com/jenkinsci/gitea-plugin/pull/13))
* Fixed plugin URLs from `http:` to `https` ([PR-14](https://github.com/jenkinsci/gitea-plugin/pull/14))
* Fixed display of organization website ([PR-18](https://github.com/jenkinsci/gitea-plugin/pull/18))
* Fixed repository polling with disabled issues or pull requests ([PR-17](https://github.com/jenkinsci/gitea-plugin/pull/17))
                                                                 ([JENKINS-54516](https://issues.jenkins-ci.org/browse/JENKINS-54516))
* Optimized imports, less redundant code and other cleanups ([PR-15](https://github.com/jenkinsci/gitea-plugin/pull/15))
* Added support for tag discovery ([PR-6](https://github.com/jenkinsci/gitea-plugin/pull/6))
* Updated documentation with details of how to setup ([PR-20](https://github.com/jenkinsci/gitea-plugin/pull/20))
* Tweak commit status checkf for pull requests so that they are consistently named based on the target branch ([PR-19](https://github.com/jenkinsci/gitea-plugin/pull/19))

## Version 1.1.2 (2019-05-27)

* Fix improper handling of untrusted branches ([SECURITY-1046](https://issues.jenkins-ci.org/browse/SECURITY-1046))
## Version 1.1.1 (2019-02-15)

* Allow non-admins to fetch organizational repositories ([PR-11](https://github.com/jenkinsci/gitea-plugin/pull/11))

## Version 1.1.0 (2019-01-17)

* Fix PR and branch links ([JENKINS-54517](https://issues.jenkins-ci.org/browse/JENKINS-54517)) 
* Switch to handy-uri Jenkins API plugin rather than bundle duplicate classes within plugin.


## Version 1.0.8 (2018-04-04)

* Use Jenkins configured proxy settings to connect to Gitea ([JENKINS-50565](https://issues.jenkins-ci.org/browse/JENKINS-50565))

## Version 1.0.7 (2018-03-22)

* Fix NPE during dynamic installation of the plugin ([JENKINS-50349](https://issues.jenkins-ci.org/browse/JENKINS-50349))

## Version 1.0.6 (2018-03-21)

* Fix NPE during dynamic installation of the plugin ([JENKINS-50319](https://issues.jenkins-ci.org/browse/JENKINS-50319))

## Version 1.0.5 (2018-03-14)

* Fix receipt of `pull_request` webhooks.
* Fix parsing of clone URLs when Gitea is publishes scp style clone URLs ([JENKINS-49768](https://issues.jenkins-ci.org/browse/JENKINS-49768))
* Misc fixes in Branch discovery strategies and pull request discovery traits

## Version 1.0.4 (2017-12-18)

* Added support for Webhook notification of repository creation / deletion now that Gitea 1.3 supports those events
* Verified branch deletion events sent by Gitea 1.3 are parsed correctly

## Version 1.0.3 (2017-10-24)

* Update to new Gitea logo

## Version 1.0.2 (2017-08-08)

* Fix Webhook notification of pushes to branches
* Add webhook notification and management of non-`SCMSource` based job types

## Version 1.0.1 (2017-07-28)

* Disable shallow clone when we know a merge will take place ([JENKINS-45771](https://issues.jenkins-ci.org/browse/JENKINS-45771))

## Version 1.0.0 (2017-07-18)

* Initial release
