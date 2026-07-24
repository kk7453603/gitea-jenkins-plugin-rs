# AGENTS.md — Operating manual for AI agents

**Read this file in full before touching anything.** It is the contract between human operators and any AI agent (Claude, qwen, gpt-opus, etc.) that works on this plugin.

This file supersedes the v1.0.0 README in agent-facing detail. For end-user documentation see `README.md`; for ops see `docs/PRODUCTION.md`.

---

## 0. TL;DR

| Aspect | Value |
|---|---|
| What | Fork of `jenkinsci/gitea-plugin` @ `ae31972` with HTTP client + webhook layer rewritten in **Rust via JNI** |
| Stack | Rust 1.86 + tokio + reqwest + axum; Java 21 + Maven + Jackson; Jenkins LTS 2.479.3+ |
| Compatibility | 100% API-compatible with upstream — drop-in replacement |
| Current release | **v1.1.0** (see `CHANGES.md`) |
| GitHub | https://github.com/kk7453603/gitea-jenkins-plugin-rs |
| Agent skills | `agent-skills/` catalog (see §10) |
| Build | `mvn package` → `target/gitea.hpi` (auto-runs `cargo build --release`) |
| Deploy | `docker compose up -d` → http://localhost:8080 (UI), :8081 (webhook) |

---

## 1. Technology stack

### Rust side (`rust/gitea-client/`)

| Crate | Version | Purpose |
|---|---|---|
| `tokio` | 1.x (rt-multi-thread, macros, sync, net, time) | Async runtime, 1 per process (Lazy static) |
| `reqwest` | 0.12 (rustls-tls, json, stream, multipart, socks) | HTTP client with proxy + TLS |
| `axum` | 0.7 | Webhook HTTP server on :8081 |
| `serde` / `serde_json` | 1.x | JSON (de)serialization for Gitea payloads |
| `jni` | 0.21 | JNI bindings (`extern "system"`, `JNIEnv`, `GlobalRef`) |
| `rustls` + `webpki-roots` + `rustls-pemfile` | 0.23 / 1.x / 2 | Custom TLS trust store |
| `hmac` + `sha2` + `hex` | 0.12 / 0.10 / 0.4 | HMAC-SHA256 webhook verification |
| `cidr` | 0.2 | IP allowlist parsing |
| `prometheus` | 0.13 | Metrics endpoint |
| `lru` | 0.12 | Idempotency dedup (X-Gitea-Delivery cache) |
| `once_cell` | 1.x | Lazy statics (`OnceCell`, `Lazy`) |
| `thiserror` | 1.x | Error enum derivation |
| `tracing` + `tracing-subscriber` | 0.1 / 0.3 (env-filter, registry) | Structured logs → Jenkins JUL |

Profile: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = "debuginfo"`.

### Java side (`src/main/java/.../gitea/`)

| Component | Files | Notes |
|---|---|---|
| **JNI shim** | `client/impl/RustGiteaConnection.java` (38 native methods), `RustGiteaConnectionFactory.java`, `NativeLibraryLoader.java` | Thin glue, returns JSON to Java which Jackson parses |
| **Webhook dispatcher** | `webhook/RustWebhookDispatcher.java`, `WebhookServerStarter.java`, `RustLogReceiver.java` | @Extension, started via `@Initializer(after=EXTENSIONS_AUGMENTED)` |
| **Global config** | `servers/GiteaServers.java` + `config.jelly` | All knobs: port, HMAC, bearer, CIDRs, rate, PEM, proxy, polling, external URL |
| **Untouched upstream** | ~95 classes (SCM, traits, events, UI) | DO NOT EDIT (see §7) |

### Build / runtime

| Layer | Tech |
|---|---|
| Build tool | Maven 3.9 + `maven-hpi-plugin` (`.hpi` artifact) |
| Native build | `exec-maven-plugin` invokes `cargo build --release` at `generate-resources` |
| CI | Jenkinsfile (Build Rust → Build Plugin stages) |
| Container | `docker/Dockerfile` — 3-stage multi-arch (rust → maven → jenkins:lts-jdk21) |
| JDK | Eclipse Temurin 21 (build + runtime) |
| OS target | Linux x86_64 + Linux aarch64 (single `.hpi` bundles both `.so`) |

---

## 2. Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│                          Jenkins Controller (JVM, JDK 21)                │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────────┐  │
│  │  ~95 Java-классов (НЕ ТРОГАТЬ — upstream @ ae31972)                │  │
│  │  GiteaSCMSource, GiteaSCMNavigator, GiteaWebhookListener,         │  │
│  │  13 traits, 41 POJO в client/api/, PersonalAccessTokenImpl,       │  │
│  │  GiteaServer(s), 16 Jelly templates, 7 SCMEvent subclasses        │  │
│  └──────────────────────────────┬─────────────────────────────────────┘  │
│                                 │ uses (via ServiceLoader SPI)            │
│                                 ▼                                        │
│           interface GiteaConnection (35 methods, AutoCloseable)          │
│                                 ▲                                        │
│                                 │ implements                             │
│  ┌──────────────────────────────┴─────────────────────────────────────┐  │
│  │  RustGiteaConnection.java  ← JNI shim                              │  │
│  │    static { NativeLibraryLoader.load("gitea_rust"); }              │  │
│  │    static ObjectMapper MAPPER (Jackson, parse JSON → POJO)         │  │
│  │    38 private static native методов (nativeFetch*, nativeCreate*)  │  │
│  │    + nativeSetTrustedCertificates / SetProxy / StartPolling / ...  │  │
│  └──────────────────────────────┬─────────────────────────────────────┘  │
│                                 │ JNI (extern "system")                  │
│  ┌──────────────────────────────▼─────────────────────────────────────┐  │
│  │  libgitea_rust.so (cdylib, ~5 MB, 41 JNI symbols, linux/amd64 +    │  │
│  │  linux/aarch64 — both bundled in single .hpi)                      │  │
│  │                                                                     │  │
│  │  • jni.rs (35 exports for HTTP API)                                │  │
│  │  • jni_webhook.rs (3: nativeStart/Stop/RegisterDispatcherClass)   │  │
│  │  • jni_polling.rs (2: nativeStartPolling/StopPolling)              │  │
│  │  • jni_log.rs (1: nativeInstallLogBridge)                          │  │
│  │  • client.rs — GiteaClient (33 async Gitea API methods)            │  │
│  │  • server.rs — axum HTTP server на :8081                           │  │
│  │  • pool.rs — connection pool (TTL+LRU, max 32)                     │  │
│  │  • log_bridge.rs — tracing→JUL bridge                              │  │
│  │  • tls.rs / tls_store.rs — custom PEM trust store                  │  │
│  │  • proxy.rs — outbound HTTP/HTTPS/SOCKS5 proxy                     │  │
│  │  • polling.rs — adaptive polling scheduler with ETag               │  │
│  │  • rate_limiter.rs — per-IP token bucket                          │  │
│  │  • events.rs — 6 Gitea event types (serde)                        │  │
│  │  • runtime.rs — Lazy<tokio::Runtime> (process-global)             │  │
│  └────────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  Ports:                                                                  │
│    :8080  Jenkins UI (Stapler/Jetty)                                     │
│    :8081  Rust webhook server (axum) — отдельный port, не Jenkins HTTP   │
│    :50000 Jenkins agent protocol                                         │
└──────────────────────────────────────────────────────────────────────────┘
         ↑                                   ↑
         │ HTTPS POST + HMAC-SHA256          │ outbound HTTPS (reqwest)
         │ from Gitea                        │ to Gitea API
         │                                   │
   ┌─────┴──────────┐                ┌───────┴─────────┐
   │   Gitea server │ ←────────────  │  Gitea server   │
   │   (webhook)    │   register     │   (REST API)    │
   └────────────────┘   hook via     └─────────────────┘
                        createHook
```

### Request flow (outbound — Jenkins → Gitea API)

```
Java method (e.g. GiteaSCMSource.fetchBranches)
  → RustGiteaConnection.fetchBranches(username, name)
    → nativeFetchBranches(serverUrl, authType, authSecret, owner, repo)  [JNI]
      → RT.block_on(async { client.fetch_branches(...).await })  [tokio]
        → reqwest GET /api/v1/repos/{owner}/{repo}/branches
          → pagination via Link header
          → null-stripping for race conditions in Gitea
        → returns raw JSON as String
      → env.new_string(json) → jstring
    → return jstring to Java
  → MAPPER.readerForListOf(GiteaBranch.class).readValue(json)
  → List<GiteaBranch> POJO
```

### Request flow (inbound — Gitea webhook → Jenkins)

```
Gitea POST /gitea-webhook/post
  + X-Gitea-Signature: <HMAC-SHA256>
  + X-Gitea-Event: push|pull_request|create|delete|release|repository
  + X-Gitea-Delivery: <uuid>
  + Authorization: Bearer <token>  (optional, if configured)
  → axum handler in server.rs
    → IP allowlist check (CIDR)              [403 if blocked]
    → rate limit (per-IP token bucket)       [429 if exhausted]
    → bearer token verify (optional)         [401 if mismatch]
    → HMAC-SHA256 verify                     [401 if mismatch]
    → X-Gitea-Delivery dedup (LRU 2048)      [200 + skip if duplicate]
    → invoke_callback(event_type, payload)
      → JNI attach_current_thread
      → RustWebhookDispatcher.handleEvent(type, json)  [via GlobalRef]
        → MAPPER.readValue(json, GiteaPushEvent.class)
        → new GiteaPushSCMEvent(event)
        → SCMHeadEvent.fireNow(scmEvent)
          → Jenkins SCMEvent bus
            → GiteaSCMSource consumes → triggers Multibranch Pipeline scan
```

---

## 3. Security architecture (critical for corporate Jenkins)

This section is the priority for any agent adapting the plugin to a corporate Jenkins with pre-existing security constraints.

### 3.1 Threat model

| Threat | Mitigation | Where |
|---|---|---|
| Webhook spoofing (forged POST) | **HMAC-SHA256** mandatory if secret set | `server.rs::verify_hmac` |
| Replay attack (capture + resend) | **X-Gitea-Delivery LRU dedup** (2048 entries, ~5 min effective window) | `server.rs::DELIVERY_CACHE` |
| Webhook source spoofing | **IP CIDR allowlist** checked before HMAC | `server.rs` + `GiteaServers.webhookAllowedCidrs` |
| DoS / brute-force HMAC | **Per-IP token bucket** (default 60/min) | `rate_limiter.rs` |
| TLS interception (corporate proxy) | Outbound uses **rustls + webpki-roots + custom PEM** | `tls.rs`, `GiteaServers.trustedCertificatesPem` |
| Self-signed Gitea cert rejection | Operator pastes corporate CA **PEM** in UI | same |
| Credential leak in logs | HMAC secret + bearer never logged at INFO+; `Auth::Token`/`Basic` hashed in pool keys | `pool.rs::key_for`, `GiteaServers.configure()` |
| Plugin ClassLoader leak | `GlobalRef` to dispatcher class instead of `find_class` (system ClassLoader can't see plugin classes) | `jni_webhook.rs::DISPATCHER_CLASS` |
| Native lib architecture mismatch | Cross-platform loader: tries `<os>/<arch>/` with fallback (aarch64→amd64 via Rosetta) | `NativeLibraryLoader.java` |
| Hot-reload thread leak | Documented limitation — restart Jenkins after upgrade | `docs/PRODUCTION.md` |

### 3.2 Security customization points for corporate Jenkins

When adapting to a corporate Jenkins, the typical integration points are:

| Corporate requirement | Where to customize | How |
|---|---|---|
| **Corporate root CA** (self-signed Gitea, internal CA) | `GiteaServers.trustedCertificatesPem` | UI field, OR auto-populate from `$JAVA_HOME/lib/security/cacerts` (read JKS via `keytool -list` + convert, or add JNI bridge that reads `Jenkins.get().proxy.trustStore`) |
| **Corporate HTTP proxy** | `GiteaServers.proxyUrl` or `Jenkins.get().proxy` | Already integrated — global proxy fallback in `buildProxyJson()` |
| **SSO / SAML headers** injected by reverse proxy | Add new bearer token check OR extend `RustWebhookDispatcher.handleEvent` to verify SSO headers via Jenkins `Authentication` API | New Java-side check in `dispatch()` |
| **Audit logging** to corporate SIEM | Add `tracing::info!` in `invoke_callback` → already routes to Jenkins JUL → forward via Logstash plugin | No code change — configure Jenkins log exporter |
| **Stricter CSP** | Jenkins global CSP (`hudson.model.DirectoryBrowserSupport.CSP`) — unrelated to plugin | `JAVA_OPTS=-Dhudson.model.DirectoryBrowserSupport.CSP=...` |
| **Network isolation** (mTLS between Jenkins↔Gitea) | `trustedCertificatesPem` covers server cert; client cert (mTLS outbound) is **not yet supported** — see TODO | Future enhancement |
| **Role-based access to webhook config** | `GiteaServers.configure()` requires `Jenkins.ADMINISTER` by default (inherited from `GlobalConfiguration`) | Override `configure()` to add custom permission check |
| **CVE scanning of bundled `.so`** | Run `cargo audit` in CI; bake results into release notes | Add `cargo-audit` step to `Jenkinsfile` |
| **Secret rotation** (HMAC, bearer, Gitea tokens) | No built-in; operator rotates manually + restarts | Future: integrate with Jenkins `Credentials` plugin rotation API |
| **Webhook URL obfuscation** (don't expose internal hostname) | `webhookExternalUrl` field — operator pastes public URL | Already supported |

### 3.3 Corporate hardening checklist (for agent to apply when adapting)

Before deploying to a corporate Jenkins, ensure:

- [ ] **HMAC secret** is ≥ 32 random bytes (not a dictionary word)
- [ ] **Bearer token** set as defence-in-depth (different value from HMAC)
- [ ] **Allowed CIDRs** populated with Gitea server IP range only
- [ ] **Rate limit** ≤ 120/min per IP (default 60 is fine)
- [ ] **TLS trusted PEM** populated if Gitea uses internal CA
- [ ] **Polling interval** = 0 (webhooks reliable) OR ≥ 300 (fallback only)
- [ ] **Webhook external URL** set if behind reverse proxy
- [ ] Jenkins **System Log Recorder** for `org.jenkinsci.plugin.gitea` at INFO
- [ ] Prometheus scraper configured for `/gitea-webhook/metrics`
- [ ] Kubernetes liveness probe on `/gitea-webhook/health`
- [ ] `cargo audit` clean (no known Rust CVEs)
- [ ] Plugin pinned in `gitea.jpi.pinned` (prevents auto-update)

---

## 4. Project layout

```
GiteaJenkinsPluginRework/
├── AGENTS.md                              ← ты здесь
├── IMPLEMENTATION_PLAN.md                 ← оригинальный план (исторический)
├── README.md                              ← пользовательская документация
├── CHANGES.md                             ← breaking changes по версиям
├── CONTRIBUTING.md                        ← как добавлять фичи
├── docs/
│   └── PRODUCTION.md                      ← ops playbook (nginx, firewall, monitoring)
├── pom.xml                                ← Maven (packaging=hpi)
├── Jenkinsfile                            ← CI pipeline
├── docker-compose.yml                     ← локальный Jenkins (port 8080+8081+50000)
├── docker/
│   ├── Dockerfile                         ← 3-stage multi-arch (rust-amd64 + rust-arm64 → maven → jenkins:lts-jdk21)
│   ├── plugins.txt                        ← workflow-multibranch, branch-api, git, ...
│   └── README.md
├── agent-skills/                          ← каталог skills для AI-агентов (см. §10)
│   ├── README.md
│   ├── patterns/                          ← 8 архитектурных patterns из этого проекта
│   ├── core/                              ← базовые dev skills (TDD, verification)
│   ├── rust/                              ← Rust patterns
│   ├── jenkins/                           ← Java/Jenkins/Docker/e2e skills
│   ├── security/                          ← security-review, security-scan
│   └── watchmen/                          ← watchmen curator (brief/setup skills)
├── src/                                   ← Java часть
│   ├── main/java/org/jenkinsci/plugin/gitea/
│   │   ├── client/impl/                   ← НАШИ файлы (JNI shim)
│   │   │   ├── RustGiteaConnection.java       (38 native methods + JSON parsing)
│   │   │   ├── RustGiteaConnectionFactory.java (SPI implementation)
│   │   │   └── NativeLibraryLoader.java       (cross-platform .so extraction)
│   │   ├── webhook/                       ← НАШИ файлы (webhook layer)
│   │   │   ├── RustWebhookDispatcher.java     (@Extension, JNI callback target)
│   │   │   ├── WebhookServerStarter.java      (@Initializer, boot-time start)
│   │   │   └── RustLogReceiver.java           (@Extension, tracing→JUL bridge)
│   │   ├── servers/GiteaServers.java     ← НАШИ доработки (webhookPort, PEM, proxy, polling, externalUrl)
│   │   ├── GiteaWebhookListener.java     ← НАШИ доработки (buildHookUrl с external URL)
│   │   ├── GiteaSCMSource.java           ← НЕ ТРОГАТЬ (upstream)
│   │   ├── GiteaSCMNavigator.java        ← НЕ ТРОГАТЬ
│   │   ├── GiteaPushSCMEvent.java + 6 siblings  ← НЕ ТРОГАТЬ
│   │   ├── *Trait.java (13 штук)         ← НЕ ТРОГАТЬ
│   │   └── client/api/                    ← 41 POJO, НЕ ТРОГАТЬ
│   ├── main/resources/
│   │   ├── META-INF/services/...GiteaConnectionFactory  ← 1 строка: RustGiteaConnectionFactory
│   │   └── org/jenkinsci/plugin/gitea/servers/GiteaServers/config.jelly
│   └── test/java/.../client/impl/
│       └── RustGiteaConnectionSmokeTest.java
└── rust/
    └── gitea-client/                      ← Rust crate
        ├── Cargo.toml                     ← crate-type = ["cdylib", "rlib"]
        ├── src/
        │   ├── lib.rs                     ← pub модули
        │   ├── auth.rs                    ← Auth {None, Token, Basic}
        │   ├── client.rs                  ← GiteaClient (33 async methods)
        │   ├── error.rs                   ← GiteaError (HttpStatus/FileNotFound/...)
        │   ├── runtime.rs                 ← Lazy<tokio::Runtime>
        │   ├── jni.rs                     ← 35 JNI exports для HTTP API
        │   ├── jni_webhook.rs             ← 3 JNI exports для webhook lifecycle
        │   ├── jni_polling.rs             ← 2 JNI exports для polling
        │   ├── jni_log.rs                 ← 1 JNI export для log bridge
        │   ├── server.rs                  ← axum webhook server (:8081)
        │   ├── events.rs                  ← 6 Gitea event types (serde)
        │   ├── pool.rs                    ← connection pool (TTL+LRU)
        │   ├── tls.rs / tls_store.rs      ← custom PEM trust
        │   ├── proxy.rs                   ← outbound proxy
        │   ├── polling.rs                 ← adaptive ETag polling
        │   ├── rate_limiter.rs            ← per-IP token bucket
        │   └── log_bridge.rs              ← tracing→JUL bridge
        └── tests/
            ├── integration.rs             ← 49 wiremock-тестов Gitea API
            ├── webhook.rs                 ← 20 e2e webhook тестов
            └── jni_symbols.rs             ← проверка presence всех JNI symbols
```

---

## 5. Build / test / deploy commands

### Local dev cycle

```bash
# Rust changes (fast, ~30s)
cd rust/gitea-client && cargo test && cd ../..

# Java changes (~5s with -o offline)
mvn compile -DskipTests -Dban-junit4-imports.skip=true -Dexec.skip=true -o

# Full .hpi build (slow, ~3 min online)
mvn -B clean package \
    -DskipTests \
    -Dban-junit4-imports.skip=true \
    -Dexec.skip=true            # skip cargo (use existing .so)
```

### Docker

```bash
# Multi-arch build (amd64 + arm64 .so in single .hpi)
docker compose build

# Run Jenkins + webhook server
docker compose up -d

# UI:         http://localhost:8080   (no auth — setup wizard disabled)
# Webhook:    http://localhost:8081/gitea-webhook/post
# Health:     http://localhost:8081/gitea-webhook/health
# Metrics:    http://localhost:8081/gitea-webhook/metrics

docker compose logs -f jenkins
docker compose down -v   # nuke volume for fresh state
```

### Testing webhook end-to-end

```bash
# Without HMAC (default config)
curl -X POST http://localhost:8081/gitea-webhook/post \
  -H "Content-Type: application/json" \
  -H "X-Gitea-Event: push" \
  -d '{"ref":"refs/heads/main","repository":{"name":"t","full_name":"a/t","html_url":"https://g/t","owner":{"login":"a"}},"sender":{"login":"x"}}'

# With HMAC (after setting webhookSecret in UI)
PAYLOAD='...'
SIG=$(echo -n "$PAYLOAD" | openssl dgst -sha256 -hmac "secret" -hex | awk '{print $NF}')
curl -X POST http://localhost:8081/gitea-webhook/post \
  -H "X-Gitea-Event: push" \
  -H "X-Gitea-Signature: $SIG" \
  -H "Authorization: Bearer $TOKEN" \
  -d "$PAYLOAD"
```

---

## 6. Architectural decisions (DO NOT change)

These decisions are load-bearing. Changing them breaks compatibility or security.

| Decision | Value | Why |
|---|---|---|
| JNI boundary format | Rust returns **JSON string**, Java parses via Jackson | Avoids duplicating 41 POJOs in Rust; Jackson already on classpath |
| Auth header for Token | `Authorization: token <T>` (NOT `Bearer`) | Gitea-specific quirk; `Bearer` returns 403 |
| 404 → `[]` | For `fetchPullRequests`/`fetchIssues`/`fetchReleases` | These endpoints may be disabled on server; empty list is correct |
| 404 → `FileNotFound` | For `fetchFile` | Matches upstream `FileNotFoundException` |
| Pagination | Parse `Link: <...>; rel="next"` + concat JSON arrays + drop `null` entries | Gitea occasionally emits null on race; upstream strips them |
| `fetchOwner` | Double-fetch: `/orgs/{name}` → fallback `/users/{name}` on 404 | Mirrors upstream |
| URL-encoding | `percent_encode_path_segment` (does NOT encode `.`) | Gitea rejects `%2E` in tag names |
| Tokio runtime | 1 per process (`once_cell::Lazy`), `block_on` from JNI | Hot-reload not supported — threads persist after plugin unload |
| Webhook server port | Separate from Jenkins HTTP (default 8081) | Keeps axum outside Stapler security perimeter; HMAC is sole auth |
| Platform | Linux x86_64 + aarch64 prod; macOS arm64 dev | Single `.hpi` bundles both `.so` |
| SPI file | 1 line: `RustGiteaConnectionFactory` | Replaces upstream `DefaultGiteaConnectionFactory` via ServiceLoader |

---

## 7. What NOT to do

1. **Don't edit the ~95 untouched Java classes.** Anything outside `client/impl/`, `webhook/`, `servers/GiteaServers.java`, `GiteaWebhookListener.java` is upstream and must stay byte-identical. The ServiceLoader SPI is the only sanctioned extension point.
2. **Don't use `Bearer` for token auth.** Only `Authorization: token <T>`. Gitea returns 403 otherwise.
3. **Don't remove `NativeLibraryLoader`** or break its double-load protection — both `RustGiteaConnection.<clinit>` and `RustWebhookDispatcher.<clinit>` call it.
4. **Don't add new POJO types in `client/api/`.** If Rust needs a new type, return JSON and let Java parse into existing POJO. Adding POJOs means coordinating Jackson annotations across two codebases.
5. **Don't build without `-Dban-junit4-imports.skip=true`.** The enforcer plugin from parent POM fails the build on JUnit 4 imports in `RustGiteaConnectionSmokeTest`.
6. **Don't use `--no-verify` / `--amend` on commits.** Always create new commits with HEREDOC messages.
7. **Don't `cargo build` inside Maven without `-Dexec.skip=true`** in Docker — the `.so` is already produced by stage 1.
8. **Don't change `meta-inf/services/...GiteaConnectionFactory`** to point at multiple factories — ServiceLoader picks the first one and the order is undefined.
9. **Don't call `env.find_class(...)` from tokio worker threads** for plugin classes — use `GlobalRef` registered at `<clinit>` time. System ClassLoader can't see plugin classes.
10. **Don't add `reqwest::Client::new()` per request.** Use `pool::acquire()` — TLS handshake is expensive.

---

## 8. Known problems + fixes (lookup table for agents)

| Symptom | Root cause | Fix |
|---|---|---|
| `UnsatisfiedLinkError: nativeStart` at plugin load | `.so` architecture ≠ JVM arch | Rebuild Dockerfile with `--platform` matching Jenkins arch; check `jar tf gitea.jpi \| grep .so` shows correct `META-INF/native/linux/<arch>/libgitea_rust.so` |
| `ClassNotFoundException: RustWebhookDispatcher` on webhook POST | JNI `find_class` uses system ClassLoader | `nativeRegisterDispatcherClass` must run in `<clinit>` — check static initializer |
| `.so` is 71 KB instead of 5 MB | Stub-then-rebuild pattern in Dockerfile confused cargo's incremental build | Single COPY + single `cargo build --release` in Stage 1 |
| Maven fails with `Premature end of Content-Length` from `repo.jenkins-ci.org` | Flaky mirror | `-Dmaven.wagon.http.retryHandler.count=10` + `-Djava.net.preferIPv4Stack=true` + cache mount |
| `maven-hpi-plugin` drops `.so` from `.hpi` | It filters binary files from `src/main/resources` | `jar uf target/gitea.hpi WEB-INF/classes/META-INF/native/...` after `mvn package` |
| 401 on every webhook | HMAC secret mismatch between Gitea and Jenkins | Re-enter secret in `Manage Jenkins → System → Gitea Servers` |
| 429 on webhook storm | Rate limit too low for high-traffic Gitea | Raise `webhookRateLimitPerMinute` to ~600 |
| Webhook returns 200 but no build triggers | Event type not supported OR `GiteaSCMSource` doesn't match `repository.full_name` | Check Jenkins System Log for `org.jenkinsci.plugin.gitea` at FINE |
| Plugin works in dev, fails in corp Jenkins | Corporate CA not trusted | Set `trustedCertificatesPem` to corporate CA PEM |
| Hot-reload leaks tokio threads | Tokio runtime persists after plugin unload | Restart Jenkins after every plugin update (documented limitation) |
| `tls::tests::garbage_pem_is_rejected` fails | reqwest 0.12.28 lenient on plain bytes | Use PEM block with invalid DER content (valid base64, garbage bytes) |
| Push from `feature/foo` branch 404s | `/` not URL-encoded in path segment | `percent_encode_path_segment` encodes `/` → `%2F`, keeps `.` |

---

## 9. Agent workflow (how to use this repo with AI agents)

### 9.1 Before starting work

1. **Read this file in full** (`AGENTS.md`).
2. Read `docs/PRODUCTION.md` if the task touches deployment/security.
3. Read `agent-skills/README.md` and pick relevant pattern skills.
4. Check `git log --oneline -20` to see recent direction.
5. Verify `mvn compile` and `cargo build --release` pass before making changes.

### 9.2 Per-task protocol

```
1. TaskCreate — break work into ≤5 subtasks if non-trivial
2. Read affected files (Read tool, not cat)
3. Edit / Write — small atomic changes
4. Verify:
   a. cargo test (if Rust touched) — must be 140+ passing
   b. mvn compile (if Java touched) — must BUILD SUCCESS
   c. docker compose build (if Dockerfile touched) — must succeed
5. Git commit with HEREDOC message — see §11 for format
6. Mark task completed
```

### 9.3 Background subagents

For parallelisable work (e.g. independent stages of a feature), launch subagents:

- **One task per subagent** — never mix Rust + Java in one prompt
- **Acceptance criteria explicit** — every prompt ends with "Critical criteria"
- **"Don't touch" section** — list files the agent must NOT edit
- **Verification in prompt** — `cargo build` / `mvn compile` commands verbatim
- **Git commit instructions** — HEREDOC, no `--amend`, no `--no-verify`

See `agent-skills/patterns/parallel-multi-stage-orchestration/SKILL.md` for the playbook.

### 9.4 Skill loading

When the user types `/<skill-name>` (e.g. `/webhook-jni-callback-server`), the agent loads that skill. Available skills:

- `agent-skills/patterns/*` — 8 architectural patterns from this project
- `agent-skills/core/*` — TDD, verification, prompt optimization
- `agent-skills/rust/*` — Rust idioms + testing
- `agent-skills/jenkins/*` — Java/Docker/e2e
- `agent-skills/security/*` — security-review, security-scan

For watchmen auto-discovery, see `agent-skills/watchmen/`.

---

## 10. Agent skills catalog (`agent-skills/`)

Each skill is a `SKILL.md` with YAML frontmatter and 5 sections (When to use / Pattern / Pitfalls / Reference files / Triggers). Skills auto-discovered by watchmen are added under `patterns/`.

| Directory | Purpose | Count |
|---|---|---|
| `patterns/` | Architectural patterns distilled from this project | 8 |
| `core/` | Base dev skills (TDD, verification, agent harness, prompt-opt) | 7 |
| `rust/` | Rust-specific patterns + testing | 2 |
| `jenkins/` | Java/Docker/hexagonal/e2e | 5 |
| `security/` | Security review + scan | 2 |
| `watchmen/` | Watchmen curator plugin integration | 2 |

**To add a new skill:** drop a `SKILL.md` in the appropriate subdir. Watchmen will pick it up on the next curator run and surface it via `/watchmen:brief`.

---

## 11. Git conventions

### Commit message format

```
<type>(<scope>): <subject>

<body — wrap at 72 chars>

<footer>
```

Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `research(results)`, `research(init)`, `research(protocol)`.

Scopes: `rust`, `java`, `jni`, `webhook`, `tls`, `proxy`, `polling`, `docker`, `ci`, `agents`, `security`.

Always end with:
```
Co-Authored-By: <Agent-Name> <email>
```

### Tagging

Annotated tags only:
```
git tag -a v1.X.0 -m "v1.X.0 — <one-line summary>"
```

---

## 12. Where to find upstream

| What | Where |
|---|---|
| Upstream source | https://github.com/jenkinsci/gitea-plugin @ `ae31972` |
| Fork (this repo) | https://github.com/kk7453603/gitea-jenkins-plugin-rs |
| Plugin on JenkinsCI | https://plugins.jenkins.io/gitea/ |
| Gitea API docs | https://docs.gitea.io/en-us/api-usage/ |
| Jenkins plugin parent POM | https://github.com/jenkinsci/plugin-pom |

---

## 13. Versioning

SemVer. v1.x tracks upstream compatibility — drop-in replacement for `jenkinsci/gitea-plugin`. v2.x (future) may introduce breaking schema changes.

| Version | Status | Notes |
|---|---|---|
| v1.0.0 | released | Initial production-ready (HTTP + webhook + auto-start + Jenkins proxy + TLS + polling + auth extensions) |
| v1.1.0 | released | Production hardening (connection pool, health, metrics, idempotency, log bridge, cross-platform, external URL) |
| v1.2.0 | planned | Corporate Jenkins integrations (see §3.3) |

---

## 14. Open architectural questions (TODO for future agents)

- **mTLS outbound** (client cert to Gitea) — currently only server-cert trust
- **Connection pool per-credential** — currently key includes auth hash; may need re-keying for credential rotation
- **Cluster mode** — Tokio runtime + HMAC secret are per-process; multi-controller Jenkins needs shared state (Redis?)
- **Webhook retry queue** — currently relies on Gitea retry; if Gitea gives up, events are lost. Local durable queue?
- **Cross-platform** — Windows Jenkins still unsupported (no `.dll` bundled)
- **`cargo-audit` in CI** — known Rust CVE scanning, not yet wired into Jenkinsfile
- **Plugin performance benchmarks** — no automated benchmarks for webhook throughput / latency under load

---

If you're an agent reading this and the task isn't covered above, **ask the human** before improvising. The cost of a wrong architectural choice here is paid by every future contributor.
