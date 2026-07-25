# Corporate Migration Checklist

Step-by-step workflow for migrating a corporate-customized Gitea plugin fork to this Rust-accelerated version. Designed for AI agents (pi, qwen-3.5-397b) and human operators.

**Estimated total time:** 4-12 hours depending on customization complexity.

---

## Phase 0: Discovery (30-60 min)

Before writing any code, understand what the corporate fork actually does.

### 0.1 Locate the corporate fork source

```bash
# Where is the corporate plugin source?
git clone <corporate-repo> /tmp/corp-gitea-plugin
cd /tmp/corp-gitea-plugin

# What's the upstream base?
git log --oneline | grep -iE "import|initial|upstream" | head -5
git remote -v
```

### 0.2 Diff corporate vs upstream

```bash
# Clone upstream at the same base commit (check pom.xml for version)
git clone --depth 1 -b <upstream-tag> https://github.com/jenkinsci/gitea-plugin.git /tmp/upstream-gitea-plugin

# Diff
diff -r /tmp/upstream-gitea-plugin/src /tmp/corp-gitea-plugin/src > /tmp/corp-changes.diff
wc -l /tmp/corp-changes.diff
```

### 0.3 Inventory customizations

Categorize every change into one of these buckets:

| Category | Examples | Migration guide |
|---|---|---|
| **Header injection (inbound)** | Custom webhook auth header, signature scheme | [`HEADER-MIGRATION.md`](./HEADER-MIGRATION.md) §2 |
| **Header injection (outbound)** | Add `X-Service-Name` to API calls | [`HEADER-MIGRATION.md`](./HEADER-MIGRATION.md) §3 |
| **Proxy customization** | Per-server proxy, PAC files, custom auth | [`PROXY-MIGRATION.md`](./PROXY-MIGRATION.md) |
| **TLS customization** | Custom trust store, mTLS client cert | [`../docs/ARCHITECTURE.md` §3](../docs/ARCHITECTURE.md) + [`JNI-EXTENSIONS.md` §8](./JNI-EXTENSIONS.md) |
| **Auth scheme** | OAuth flow, JWT, SAML header | [`JNI-EXTENSIONS.md` §7](./JNI-EXTENSIONS.md) |
| **Audit log** | SIEM forwarding, custom log format | [`examples/audit-sink.md`](./examples/audit-sink.md) |
| **Webhook URL** | Custom path prefix, port | Already supported — `GiteaServers.webhookPath` |
| **Rate limit override** | Per-org limits | [`HEADER-MIGRATION.md` §5](./HEADER-MIGRATION.md) |
| **SCM behavior** | Custom traits, discovery | **DO NOT MIGRATE** — these are in the ~95 untouched Java classes |

### 0.4 Decision matrix

For each customization, decide:

| Already supported? | → Configure via UI, no code |
| Close to existing? | → Extend (small code change) |
| Completely new? | → New JNI bridge (see JNI-EXTENSIONS.md) |
| Out of scope? | → Move to reverse proxy or separate plugin |

---

## Phase 1: Setup (30 min)

### 1.1 Clone this repo

```bash
git clone https://github.com/kk7453603/gitea-jenkins-plugin-rs.git
cd gitea-jenkins-plugin-rs
```

### 1.2 Read the required files

In this order:
1. [`../AGENTS.md`](../AGENTS.md) — project-wide operating manual
2. [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) — C4 + sequence diagrams
3. [`AGENTS.md`](./AGENTS.md) (this directory) — corporate migration constraints
4. Relevant: `HEADER-MIGRATION.md`, `PROXY-MIGRATION.md`, `JNI-EXTENSIONS.md`

### 1.3 Verify build works

```bash
cd rust/gitea-client && cargo test && cd ../..
mvn -B compile test-compile -DskipTests -Dban-junit4-imports.skip=true -Dexec.skip=true -o
docker compose build
```

All three MUST pass before any customization. If any fails, the plugin is broken at HEAD — fix that first.

---

## Phase 2: Implement customizations (2-8 hours)

For each customization from Phase 0:

### 2.1 Check if it's already supported

| Customization | Already supported? | How |
|---|---|---|
| Static header check | Almost — see HEADER-MIGRATION §2 | Add to `server.rs` |
| Custom webhook path | YES | `GiteaServers.webhookPath` UI field |
| External webhook URL | YES | `GiteaServers.webhookExternalUrl` UI field |
| HMAC secret | YES | `GiteaServers.webhookSecret` UI field |
| Bearer token | YES | `GiteaServers.webhookBearerToken` UI field |
| IP CIDR allowlist | YES | `GiteaServers.webhookAllowedCidrs` UI field |
| Rate limit | YES | `GiteaServers.webhookRateLimitPerMinute` UI field |
| Trusted PEM | YES | `GiteaServers.trustedCertificatesPem` UI field |
| Proxy URL | YES | `GiteaServers.proxyUrl` UI field |
| Jenkins global proxy | YES (auto fallback) | `Manage Jenkins → System → Proxy` |
| Custom path prefix | YES | `GiteaServers.webhookPath` |

### 2.2 For each unsupported customization

Follow the appropriate guide:
- **Header**: `HEADER-MIGRATION.md` §2 or §3
- **Proxy**: `PROXY-MIGRATION.md` §2 Pattern B/D/F/G
- **Auth**: `JNI-EXTENSIONS.md` §7
- **mTLS**: `JNI-EXTENSIONS.md` §8
- **Audit**: `examples/audit-sink.md`

### 2.3 Implementation order

Implement in this order to minimize integration risk:

1. **Config fields first** (Java only — no Rust change) — easiest to revert
2. **Rust modules** (no JNI change) — `corp_headers.rs`, etc.
3. **JNI bridges** (Rust + Java) — highest risk, do last
4. **Tests** — for each step

### 2.4 Verify after each step

```bash
# After Rust changes:
cd rust/gitea-client && cargo test && cd ../..

# After Java changes:
mvn -B compile test-compile -DskipTests -Dban-junit4-imports.skip=true -Dexec.skip=true -o

# After UI changes (jelly):
mvn -B package -DskipTests -Dban-junit4-imports.skip=true -Dexec.skip=true
# Then upload target/gitea.hpi to a test Jenkins and verify UI renders
```

---

## Phase 3: Integration test (1-2 hours)

### 3.1 Build the customized .hpi

```bash
docker compose build
# Extract .hpi from image:
docker run --rm -v "$(pwd):/out" jenkins-gitea-rust:local \
    cp /usr/share/jenkins/ref/plugins/gitea.jpi /out/target/gitea.hpi
```

### 3.2 Set up test Jenkins

```bash
# Use a separate Jenkins instance for testing — NOT production
docker compose -f docker-compose.test.yml up -d  # if you have a test compose file
# OR deploy to a staging Jenkins controller
```

### 3.3 Configure corporate settings

In Jenkins UI (`Manage Jenkins → System → Gitea Servers`):
- Standard fields: webhookPort, webhookSecret, etc.
- Custom fields (the ones you added in Phase 2)

### 3.4 Run smoke test

```bash
./tools/smoke-test.sh http://test-jenkins:8081 <hmac-secret>
```

All 5 tests MUST pass.

### 3.5 Test corporate customizations

For each customization, write a test scenario:

| Customization | Test |
|---|---|
| Custom header check | POST without header → 401; POST with header → 200 |
| Custom proxy | Verify outbound call uses proxy (check proxy logs) |
| mTLS | Verify Gitea receives client cert |
| Audit sink | Trigger webhook, verify SIEM receives event |

### 3.6 Verify Multibranch Pipeline triggers

```bash
# Push a commit to a tracked repo
git commit --allow-empty -m "test trigger" && git push

# Watch Jenkins System Log for:
# - handleEvent("push", ...) from RustWebhookDispatcher
# - GiteaSCMSource consuming the event
# - Build triggering
```

---

## Phase 4: Production deployment (30-60 min)

### 4.1 Backup current state

```bash
# On production Jenkins controller:
export JENKINS_HOME=/var/lib/jenkins
./tools/migrate-from-upstream.sh  # backs up config.xml + .jpi
```

### 4.2 Schedule maintenance window

The plugin requires Jenkins restart. Typical downtime: 30-60s.

### 4.3 Install customized .hpi

```bash
sudo systemctl stop jenkins
sudo cp target/gitea.hpi $JENKINS_HOME/plugins/gitea.jpi
sudo chown jenkins:jenkins $JENKINS_HOME/plugins/gitea.jpi
sudo systemctl start jenkins
```

### 4.4 Configure corporate settings (production)

Same as Phase 3.3 but with production values.

### 4.5 Verify

```bash
./tools/smoke-test.sh http://prod-jenkins:8081 <prod-hmac-secret>
```

Watch Jenkins System Log for ~10 minutes after deploy. Watch Prometheus metrics for error rate.

### 4.6 Rollback if needed

```bash
export BACKUP_DIR=tools/_backup/<timestamp>
./tools/rollback-to-upstream.sh
```

---

## Phase 5: Documentation (30 min)

### 5.1 Document the corporate customizations

Create a file `corporate-migration/CORP-CUSTOMIZATIONS.md` describing:

- What was customized
- Why (corporate requirement)
- Which files were changed
- How to revert each customization

### 5.2 Update corporate plugin's CHANGELOG

Add an entry pointing to this fork + the customization file.

### 5.3 Train ops team

Walk the ops team through:
- New UI fields and their meaning
- Smoke test command
- Rollback procedure
- Where to find logs (Jenkins System Log → `org.jenkinsci.plugin.gitea.*`)

---

## Quick reference: time estimates

| Customization complexity | Time |
|---|---|
| Configuration only (no code) | 30 min |
| 1-2 simple header checks | 2 hours |
| Custom auth scheme (OAuth) | 4-6 hours |
| mTLS client cert | 3-4 hours |
| Per-host proxy routing | 3-5 hours |
| Full corporate fork migration (5+ customizations) | 1-2 days |

---

## Emergency stops

If at any point:

- `cargo test` fails AND you can't fix in 30 min → revert to last known good commit
- `mvn compile` fails AND you can't fix in 30 min → revert
- Smoke test fails in production → rollback immediately, investigate in staging
- Jenkins won't start after plugin install → move `.jpi` aside, restart, investigate

**Never** debug in production. Always have a staging Jenkins.
