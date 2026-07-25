# Migration Guide — upstream → Rust+JNI fork

This guide walks through migrating an existing Jenkins installation from
`jenkinsci/gitea-plugin` (upstream) to this Rust-accelerated fork. The
process is reversible — see [Rollback](#rollback) below.

## TL;DR

```bash
# On Jenkins controller:
export JENKINS_HOME=/var/lib/jenkins
./tools/migrate-from-upstream.sh

# Follow the printed checklist. After Jenkins restart:
./tools/smoke-test.sh http://jenkins:8081 <your-hmac-secret>
```

---

## Pre-migration checklist

| Item | Notes |
|---|---|
| Jenkins controller running Linux x86_64 or aarch64 | Windows not supported in v1.x |
| `JENKINS_HOME` accessible from the migration script | Typically `/var/lib/jenkins` |
| Write access to `$JENKINS_HOME/plugins/` | Usually requires `sudo` |
| Maintenance window scheduled | Jenkins needs restart (~30-60s downtime) |
| Existing Gitea webhooks recorded | URL will change — operator needs to update them in Gitea |
| Backup of `$JENKINS_HOME/config.xml` | The script does this automatically, but worth verifying |
| Rust toolchain OR Docker available | Needed to build the `.hpi` if not already built |

---

## What changes for users

### Webhook URL (BIG change)

The biggest user-visible difference: the webhook receiver moves from
Jenkins' HTTP server to a **separate Rust-controlled port** (default
`:8081`).

| Aspect | Upstream | This fork |
|---|---|---|
| Webhook URL | `http://<jenkins>/gitea-webhook/post` | `http://<jenkins>:8081/gitea-webhook/post` |
| Auth | Jenkins user auth + crumb | HMAC-SHA256 (+ optional bearer token + IP CIDR) |
| Path | Fixed `/gitea-webhook/post` | Configurable prefix (default `/gitea-webhook`, override via `webhookPath` field) |
| Reverse proxy | Standard Jenkins reverse proxy | Separate upstream block for `:8081` (see `docs/PRODUCTION.md`) |

If Jenkins is behind nginx/cloudflare, also set `webhookExternalUrl` in
`Gitea Servers` UI — the plugin registers that URL with Gitea verbatim.

### Gitea API auth

No change — existing Personal Access Tokens continue to work. The
upstream `GiteaAuth` SPI is unchanged.

### Multibranch Pipeline behaviour

No change. The fork is API-compatible — `GiteaSCMSource`,
`GiteaSCMNavigator`, all 13 discovery traits, and the 41 event POJOs
are byte-identical with upstream `ae31972`.

---

## Step-by-step migration

### 1. Build the `.hpi` (if not already)

```bash
# Option A: local build (requires cargo + JDK 21 + maven)
cd rust/gitea-client && cargo build --release && cd ../..
mvn -B clean package \
    -DskipTests \
    -Dban-junit4-imports.skip=true \
    -Dexec.skip=true
# Result: target/gitea.hpi

# Option B: Docker build (multi-arch amd64 + arm64)
docker compose build
docker run --rm \
    -v "$(pwd):/src" -w /src \
    jenkins-gitea-rust:local \
    cp /usr/share/jenkins/ref/plugins/gitea.jpi target/gitea.hpi
```

### 2. Run the migration script

```bash
export JENKINS_HOME=/var/lib/jenkins
./tools/migrate-from-upstream.sh
```

The script:
1. Backs up `$JENKINS_HOME/config.xml` and existing `gitea.jpi` to
   `tools/_backup/<timestamp>/`
2. Builds the new `.hpi` if missing
3. Prints a migration checklist with the new webhook URL

### 3. Stop Jenkins + install plugin

```bash
sudo systemctl stop jenkins

# Either via UI: Manage Jenkins → Plugins → Advanced → Upload Plugin
# Or via filesystem:
sudo cp target/gitea.hpi $JENKINS_HOME/plugins/gitea.jpi
sudo chown jenkins:jenkins $JENKINS_HOME/plugins/gitea.jpi
```

### 4. Start Jenkins

```bash
sudo systemctl start jenkins
```

The Rust webhook server auto-starts on Jenkins boot via `@Initializer`
(`WebhookServerStarter.java`). Check logs:

```bash
sudo tail -f /var/log/jenkins/jenkins.log | grep -i "webhook"
# Should see: "Gitea webhook server started on port 8,081 (path: /gitea-webhook)"
```

### 5. Configure Gitea Servers UI

Navigate to **Manage Jenkins → System → Gitea Servers**. Existing
servers are preserved (config.xml back-compat). Fill in the new fields:

- **HMAC secret** — random 32-byte string (e.g. `openssl rand -hex 32`)
- **Bearer token** — optional defence-in-depth (different value from HMAC)
- **Allowed CIDRs** — IP range of your Gitea servers (e.g. `10.0.0.0/8`)
- **Rate limit** — default 60/min per IP is fine for most setups
- **Trusted PEM** — paste corporate CA if Gitea uses self-signed cert
- **Webhook path prefix** — leave as `/gitea-webhook` unless reverse
  proxy requires otherwise
- **External webhook URL** — set if behind reverse proxy

Save. The Rust server restarts with new settings.

### 6. Update Gitea webhook URL

In **Gitea → repository Settings → Webhooks**, edit each existing
webhook:

| Field | Old value | New value |
|---|---|---|
| Target URL | `http://<jenkins>/gitea-webhook/post` | `http://<jenkins>:8081/gitea-webhook/post` |
| Content-Type | `application/json` | `application/json` (unchanged) |
| Secret | (none or Jenkins crumb) | The HMAC secret from step 5 |
| Trigger events | unchanged | unchanged |

Click **Test Delivery** — Gitea should get `200 OK` back.

### 7. Smoke test

```bash
./tools/smoke-test.sh http://<jenkins>:8081 <hmac-secret> [bearer-token]
```

Expected output:
```
✓ GET /health → 200
✓ GET /metrics → 200 + gitea_webhook_requests_total present
✓ POST without X-Gitea-Event → 400
✓ POST with wrong HMAC → 401
✓ POST valid push event → 200
✓ Webhook endpoint healthy.
```

### 8. Verify Multibranch Pipeline triggers

Push a commit to any tracked branch in a repo with a `Jenkinsfile`.
Check:

1. Jenkins System Log shows `handleEvent("push", ...)` from
   `RustWebhookDispatcher`
2. The Multibranch Pipeline job triggers a build
3. `/metrics` shows `gitea_webhook_requests_total{status="ok"}` counter
   incrementing

---

## Rollback

If something goes wrong, roll back to upstream:

```bash
export JENKINS_HOME=/var/lib/jenkins
export BACKUP_DIR=/path/to/tools/_backup/20260725-124800  # from migration script
./tools/rollback-to-upstream.sh
```

The script:
1. Stops Jenkins
2. Restores the backed-up upstream `gitea.jpi`
3. Restores the backed-up `config.xml` (drops our new fields)
4. Removes the `gitea.jpi.pinned` marker
5. Restarts Jenkins

Then update Gitea webhook URLs back to the upstream format
(`http://<jenkins>/gitea-webhook/post` without port suffix).

---

## Common migration issues

### "Address already in use" on Jenkins startup

The webhook port (`:8081`) is bound by another process. Either:
- Change `webhookPort` in UI before restart, OR
- Free the port: `sudo lsof -i :8081` + `sudo kill <pid>`

### `UnsatisfiedLinkError: nativeStart`

Architecture mismatch. Check:
```bash
uname -m  # must be x86_64 or aarch64
unzip -p /var/lib/jenkins/plugins/gitea.jpi WEB-INF/lib/gitea.jar > /tmp/g.jar
unzip -l /tmp/g.jar | grep -E '\.so'
# Should list META-INF/native/linux/{amd64,aarch64}/libgitea_rust.so
```

If only one arch is bundled, rebuild with the multi-stage Dockerfile
(see `docker/Dockerfile`).

### Existing webhooks fail with 401 after migration

The HMAC secret is mandatory if you set it in Jenkins UI. Either:
- Set the same secret in Gitea webhook settings, OR
- Clear the secret in Jenkins (not recommended — disables auth)

### Plugin loads but `handleEvent` never fires

Check:
1. `org.jenkinsci.plugin.gitea.webhook` logger at FINE in Jenkins
   System Log
2. Confirm `RustLogReceiver` registered (look for tracing events from
   Rust — should appear in the same logger)
3. Verify webhook source IP is in `Allowed CIDRs`

---

## What you keep from upstream

- All `GiteaSCMSource` / `GiteaSCMNavigator` configurations
- All 13 discovery traits (Branch, Fork PR, Origin PR, Tag, Release,
  SSH checkout, Webhook registration, Exclude archived, etc.)
- All 41 POJO types in `client/api/` (Jackson annotations intact)
- All 16 Jelly UI templates
- PersonalAccessTokenImpl credential type
- GiteaServer / GiteaServers global config (existing fields)
- All webhook event POJOs (`GiteaPushEvent`, `GiteaPullRequestEvent`, etc.)

The fork is a **drop-in replacement** at the API level — only the
implementation behind the SPI changes.
