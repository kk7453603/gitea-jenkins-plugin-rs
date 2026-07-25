# Corporate Migration — Operating Manual for AI Agents

> **You are an AI agent** (pi, qwen-3.5-397b, claude, or other) tasked with migrating a **corporate-customized** fork of `jenkinsci/gitea-plugin` to this Rust-accelerated version. This file is your entry point. Read it fully before any code change.

This directory contains everything you need to:
1. Understand the **architecture boundary** between JNI-integration code (DO NOT TOUCH) and extension points (SAFE TO MODIFY)
2. Migrate corporate customizations (header injection, proxy routing, audit sinks, custom auth)
3. Add new JNI bridges without breaking existing ones

---

## 0. The 5-minute read order

Before any work, read these files **in this order**:

| # | File | Why | Time |
|---|---|---|---|
| 1 | **This file** (`corporate-migration/AGENTS.md`) | Understand your constraints and workflow | 5 min |
| 2 | [`../AGENTS.md`](../AGENTS.md) (root) | Project-wide operating manual, 14 sections | 15 min |
| 3 | [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) | C4 + sequence diagrams + header pipeline | 20 min |
| 4 | [`JNI-EXTENSIONS.md`](./JNI-EXTENSIONS.md) (this dir) | How to safely add new JNI bridges | 10 min |
| 5 | [`HEADER-MIGRATION.md`](./HEADER-MIGRATION.md) | How to port corporate header customizations | 10 min |
| 6 | [`PROXY-MIGRATION.md`](./PROXY-MIGRATION.md) | How to port corporate proxy configurations | 5 min |
| 7 | [`CHECKLIST.md`](./CHECKLIST.md) | Step-by-step migration workflow | 5 min |

**Total onboarding time: ~70 minutes.** After that you should be able to plan the migration.

---

## 1. What this plugin IS and IS NOT

### ✅ This plugin IS

- A **drop-in replacement** for upstream `jenkinsci/gitea-plugin` (API-compatible)
- A **Rust+JNI rewrite** of the HTTP client and webhook receiver
- **Multi-arch** (linux/amd64 + linux/aarch64 in single `.hpi`)
- **Configurable** via Jenkins UI (`Manage Jenkins → System → Gitea Servers`)

### ❌ This plugin is NOT

- A general-purpose HTTP gateway (it only talks to one Gitea server)
- A reverse proxy (use nginx/envoy for that)
- An authentication broker (use Jenkins Credentials + Gitea tokens)
- A logging pipeline (use Jenkins Logstash plugin for SIEM forwarding)

If your corporate fork added features outside the Gitea plugin scope, those features belong in a separate Jenkins plugin or in the reverse proxy layer.

---

## 2. The CRITICAL boundary — DO NOT TOUCH vs SAFE TO MODIFY

This is the most important section. Read it twice.

### 🚫 DO NOT TOUCH (architecture load-bearing)

These files implement the JNI bridge contract. **Any change here breaks every call from Java to Rust.**

| File(s) | Why it's load-bearing |
|---|---|
| `rust/gitea-client/src/lib.rs` | Module wiring — `pub mod` declarations |
| `rust/gitea-client/src/jni.rs` (35 exports) | Java↔Rust method name mapping. Symbol names MUST match `RustGiteaConnection.nativeXxx` |
| `rust/gitea-client/src/runtime.rs` | Global `tokio::Runtime` — `block_on` from JNI |
| `rust/gitea-client/src/jni_webhook.rs::DISPATCHER_CLASS` | `GlobalRef` to `RustWebhookDispatcher` — without this, tokio workers can't find the Java class |
| `src/main/java/.../client/impl/RustGiteaConnection.java` (38 native methods) | JNI symbol names + signature contract |
| `src/main/java/.../client/impl/NativeLibraryLoader.java` | `.so` extraction — break this and nothing loads |
| `src/main/resources/META-INF/services/...GiteaConnectionFactory` | 1 line — ServiceLoader SPI. If wrong, plugin is dead. |
| `src/main/java/.../webhook/RustWebhookDispatcher.<clinit>` | Static init order: load lib → register class → install log bridge |

### ✅ SAFE TO MODIFY (extension points)

| Where | What you can do |
|---|---|
| `rust/gitea-client/src/server.rs` — **header pipeline** (after rate limit, before HMAC) | Add new header checks (e.g. `X-Corp-Token`) |
| `rust/gitea-client/src/proxy.rs` | Add new proxy routing logic (per-host, per-credential) |
| `rust/gitea-client/src/log_bridge.rs` | Add new JUL loggers or transform messages |
| `rust/gitea-client/src/tls.rs` | Add per-host trust store (rare) |
| `rust/gitea-client/src/pool.rs` | Add per-credential client variants (rare) |
| `src/main/java/.../servers/GiteaServers.java` | Add new config fields (UI binding via Jelly) |
| `src/main/resources/.../GiteaServers/config.jelly` | Add new UI controls |
| New file: `rust/gitea-client/src/<custom>.rs` | New module for corporate logic |
| New file: `src/main/java/.../webhook/CorporateWebhookFilter.java` | Pre-dispatch hook in Java |

### ⚠️ ADD NEW (requires both Rust AND Java changes)

If you need a **new** JNI bridge (e.g. corporate auth callback, audit sink):

1. Add Rust export in a NEW file `rust/gitea-client/src/jni_corp.rs`
2. Add Java native method declaration in `RustGiteaConnection.java` or a NEW class
3. Read [`JNI-EXTENSIONS.md`](./JNI-EXTENSIONS.md) for the safe pattern

---

## 3. Your constraints as a corporate migration agent

### Constraint 1: Never break the ServiceLoader SPI

The file `META-INF/services/org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory` MUST contain exactly one line:

```
org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnectionFactory
```

Do not add corporate factories here. If you need a corporate wrapper, subclass `RustGiteaConnectionFactory` and put the FQCN here. But the cleaner path is to add the customization **inside** `RustGiteaConnection` or in a separate filter — see below.

### Constraint 2: Never break JNI symbol names

The 38 `nativeXxx` methods in `RustGiteaConnection.java` MUST keep their exact names. JNI name mangling is `Java_<package>_<Class>_<method>` — if you rename, the `.so` becomes unreachable.

### Constraint 3: Never remove `GlobalRef` registrations

`nativeRegisterDispatcherClass(RustWebhookDispatcher.class)` and `nativeInstallLogBridge()` are called from `<clinit>`. They MUST run before any native method that uses tokio worker threads. Do not reorder these calls.

### Constraint 4: Preserve POJO types

The 41 POJOs in `src/main/java/.../client/api/` are Jackson-annotated. Rust returns JSON strings; Java parses them. If a corporate customization needs a new field, add it to the Rust JSON output AND update the POJO with `@JsonProperty`. **Do not** create new POJO types — they won't be visible to upstream code.

### Constraint 5: Test before commit

After any change:

```bash
cd rust/gitea-client && cargo test && cd ../..
mvn -B compile test-compile -DskipTests -Dban-junit4-imports.skip=true -Dexec.skip=true -o
```

Both MUST pass. If `cargo test` fails, you broke the Rust side. If `mvn compile` fails, you broke the Java side.

---

## 4. Migration workflow

```
corporate plugin source code
        │
        ▼
[1] INVENTORY corporate customizations
    What did corp change vs upstream?
    - Header injection? → HEADER-MIGRATION.md
    - Custom proxy? → PROXY-MIGRATION.md
    - Custom audit log? → AUDIT-MIGRATION.md (TBD)
    - Custom auth scheme? → JNI-EXTENSIONS.md §"custom auth"
        │
        ▼
[2] CHECK existing features
    Does this plugin already support it?
    - HMAC, bearer, CIDR, rate limit — already in server.rs
    - Proxy (with corp proxy fallback) — already in proxy.rs + GiteaServers
    - TLS corp CA — already in tls.rs + trustedCertificatesPem
    - Custom webhook path — already in GiteaServers.webhookPath
        │
        ▼
[3] MAP each customization
    For each corp feature:
    - If already supported → just configure via UI
    - If close to existing → extend (e.g. add header to pipeline)
    - If completely new → add new module (JNI-EXTENSIONS.md)
        │
        ▼
[4] IMPLEMENT
    Rust side: rust/gitea-client/src/<custom>.rs
    Java side: new field in GiteaServers + Jelly entry
    JNI bridge: new file jni_<custom>.rs (if needed)
        │
        ▼
[5] TEST
    cargo test (Rust)
    mvn compile test-compile (Java)
    docker compose build (full image)
    ./tools/smoke-test.sh
        │
        ▼
[6] DEPLOY
    ./tools/migrate-from-upstream.sh
    Configure new UI fields
    Run smoke test
    Rollback if broken: ./tools/rollback-to-upstream.sh
```

---

## 5. Common corporate customization patterns → where they go

| Corp customization | Where in this plugin | Migration effort |
|---|---|---|
| **Add header `X-Corp-Token` to webhook auth** | `server.rs` header pipeline (after bearer, before HMAC) | 30 min — see [`examples/custom-header-injection.md`](./examples/custom-header-injection.md) |
| **Route different repos through different proxies** | `proxy.rs` per-host routing | 1-2 hours — see [`examples/multi-proxy.md`](./examples/multi-proxy.md) |
| **Custom OAuth flow for Gitea token refresh** | New `auth.rs` variant + new JNI export | 3-4 hours — see [`JNI-EXTENSIONS.md` §custom-auth](./JNI-EXTENSIONS.md) |
| **Forward webhooks to corp SIEM (Splunk)** | `log_bridge.rs` add new sink + Java filter | 1 hour — see [`examples/audit-sink.md`](./examples/audit-sink.md) |
| **Strip internal IPs from logs** | `log_bridge.rs` message sanitization | 30 min |
| **Custom rate limit per organization** | `rate_limiter.rs` extend key to include org header | 1 hour |
| **Require client cert (mTLS) for outbound** | `tls.rs` add client cert PEM | 2-3 hours — see [`JNI-EXTENSIONS.md` §mtls](./JNI-EXTENSIONS.md) |
| **Custom webhook path prefix per tenant** | `server.rs` route prefix (already supported via `webhookPath`) | 5 min — just configure UI |
| **Audit every webhook to separate file** | Java filter + Jenkins Log Recorder | 30 min — see [`examples/audit-sink.md`](./examples/audit-sink.md) |

---

## 6. How to ask for help

If after reading all files in this directory you still don't know how to migrate something:

1. Check `../docs/ARCHITECTURE.md` §8 "Known problems + fixes" — symptom → root cause → fix table
2. Check `../AGENTS.md` §8 — same table, project-wide
3. Run `cargo doc --open` in `rust/gitea-client/` to see Rust API docs
4. Open the relevant source file and read its top-level doc comment — every module has one

If still stuck, escalate to a human. Do NOT improvise JNI signature changes — a wrong choice here is paid by every future contributor.

---

## 7. Anti-patterns to avoid

| ❌ Don't | ✅ Do instead |
|---|---|
| Modify `jni.rs` to add corporate logic | Create new `jni_corp.rs` module |
| Replace `RustGiteaConnection` with corporate subclass | Add corporate logic in `RustGiteaConnection` constructor or in a filter |
| Bypass `GiteaServers` config and hardcode values | Add new `@Restricted(NoExternalUse.class)` field with getter/setter |
| Add `println!` in Rust for debugging | Use `tracing::info!/warn!/error!` — it routes to Jenkins System Log |
| Use `env.find_class(...)` from tokio worker | Use `GlobalRef` registered in `<clinit>` (see `jni_webhook.rs`) |
| Mutate webhook headers between Gitea and Jenkins | Read-only pipeline — accept or reject, never mutate |
| Create new POJO types in `client/api/` | Extend Rust JSON output, parse into existing POJO |
| Skip `cargo test` because "it's just a small change" | Always run both `cargo test` AND `mvn compile` before commit |

---

## 8. Success criteria

Your migration is complete when:

- [ ] All corporate customizations are mapped to extension points (not core changes)
- [ ] `cargo test` passes (70+ unit + 20 webhook tests green)
- [ ] `mvn compile test-compile` passes
- [ ] `docker compose build` succeeds and produces a multi-arch `.hpi`
- [ ] `./tools/smoke-test.sh` passes all 5 endpoint tests
- [ ] Jenkins UI shows new corporate config fields
- [ ] Webhook delivery from Gitea triggers expected behavior (custom header check, custom audit log, etc.)
- [ ] Rollback procedure tested — plugin can be uninstalled without breaking Jenkins

If any criterion fails, do not deploy. Investigate root cause using `../AGENTS.md` §8 known-problems table.

---

## 9. File index

```
corporate-migration/
├── AGENTS.md                      ← ты здесь
├── JNI-EXTENSIONS.md              ← How to add new JNI bridges safely
├── HEADER-MIGRATION.md            ← Porting corporate header customizations
├── PROXY-MIGRATION.md             ← Porting corporate proxy configurations
├── AUDIT-MIGRATION.md             ← Porting audit log integrations (TBD)
├── CHECKLIST.md                   ← Step-by-step migration workflow
└── examples/
    ├── custom-header-injection.md ← Template: add X-Corp-Token check
    ├── custom-auth.md             ← Template: OAuth/JWT auth scheme
    ├── multi-proxy.md             ← Template: per-host proxy routing
    └── audit-sink.md              ← Template: webhook event → SIEM
```
