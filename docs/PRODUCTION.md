# Production Deployment Guide

Operational playbook for deploying the Rust-accelerated Gitea plugin to a production Jenkins controller. Read this before going live — most fields look optional in the UI but have real security and reliability implications.

## TL;DR Checklist

Before enabling the plugin in production, complete every item:

- [ ] Jenkins controller is Linux x86_64 or Linux aarch64 (kernel ≥ 5.x)
- [ ] Port 8081 (or your configured `webhookPort`) is open on the controller firewall and reachable from the Gitea instance
- [ ] **HMAC secret** configured in `Manage Jenkins → System → Gitea Servers` (random 32+ byte string)
- [ ] Gitea webhook settings use the same HMAC secret + URL `<scheme>://<jenkins-host>:<webhookPort>/gitea-webhook/post`
- [ ] **Bearer token** set (defence-in-depth on top of HMAC)
- [ ] **Allowed CIDRs** populated with the Gitea server's source IP range
- [ ] **Rate limit** set to ≤120/min per IP (default 60 is fine)
- [ ] **Polling interval** left at 0 (disabled) if webhooks are reliable, or set to 300+ sec as fallback
- [ ] Jenkins **System Log Recorder** created for `org.jenkinsci.plugin.gitea` at INFO
- [ ] Prometheus scraper configured to scrape `http://<jenkins>:8081/gitea-webhook/metrics` every 30s
- [ ] Liveness probe (Kubernetes) hitting `GET /gitea-webhook/health` every 10s
- [ ] Backup strategy for `$JENKINS_HOME/config.xml` and `$JENKINS_HOME/plugins/gitea.jpi`

## Architecture for operators

```
                 ┌────────────────────┐
                 │  Gitea server      │
                 │  (HTTPS + HMAC)    │
                 └──────────┬─────────┘
                            │ POST /gitea-webhook/post
                            │ + X-Gitea-Signature: HMAC-SHA256
                            │ + X-Gitea-Event: push|pull_request|...
                            │ + X-Gitea-Delivery: <uuid>
                            ▼
       ┌────────────────────────────────────────────────────┐
       │  Jenkins controller (JVM)                          │
       │                                                    │
       │  :8080  Jenkins UI (Stapler / Jetty)               │
       │  :8081  Rust webhook server (axum, separate port)  │
       │  :50000 Jenkins agent protocol                     │
       │                                                    │
       │  libgitea_rust.so (5 MB, 41 JNI symbols)           │
       │   ├── axum HTTP server + HMAC + rate limit         │
       │   ├── reqwest HTTP client + connection pool        │
       │   ├── tokio runtime (1 per process)                │
       │   └── Prometheus / health endpoints                │
       └────────────────────┬───────────────────────────────┘
                            │ outbound HTTPS
                            ▼
                       Gitea API
```

## Reverse-proxy setup (nginx)

The most common production topology puts Jenkins behind nginx/Traefik/AWS ALB. The Rust webhook server listens on its own port and is **not** part of Jenkins' Jetty — so the reverse proxy needs two upstreams.

```nginx
# nginx.conf — minimal reverse proxy for Jenkins + Rust webhook
upstream jenkins_ui {
    server 127.0.0.1:8080;
}
upstream gitea_rust_webhook {
    server 127.0.0.1:8081;
    keepalive 32;
}

server {
    listen 443 ssl http2;
    server_name jenkins.internal.corp;

    ssl_certificate     /etc/ssl/jenkins.crt;
    ssl_certificate_key /etc/ssl/jenkins.key;

    # TLS hardening
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_prefer_server_ciphers on;

    # Jenkins UI traffic
    location / {
        proxy_pass http://jenkins_ui;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_read_timeout 3600;
    }

    # Rust webhook endpoint — exposed on a separate path so the HMAC
    # verification runs before any Jenkins auth. Set webhookExternalUrl
    # in Gitea Servers config to: https://jenkins.internal.corp/gitea-webhook/post
    location /gitea-webhook/ {
        proxy_pass http://gitea_rust_webhook;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        # Health/metrics endpoints are open (no HMAC required for /health,
        # restrict /metrics via nginx basic auth or network policy).
        location /gitea-webhook/metrics {
            auth_basic "Prometheus";
            auth_basic_user_file /etc/nginx/.htpasswd_prom;
            proxy_pass http://gitea_rust_webhook;
        }
    }
}
```

In `Manage Jenkins → System → Gitea Servers`, set:
- `External webhook URL`: `https://jenkins.internal.corp/gitea-webhook/post`

This URL is registered with Gitea verbatim — Gitea will call it directly.

## Firewall rules

| Source | Destination | Port | Protocol | Purpose |
|---|---|---|---|---|
| Gitea server IP | Jenkins controller | 8081 (or configured `webhookPort`) | TCP | Webhook delivery |
| Jenkins controller | Gitea server | 443 (or 3000) | TCP | Outbound API calls |
| Monitoring scraper | Jenkins controller | 8081 | TCP | Prometheus `/gitea-webhook/metrics` |
| Operator VPN | Jenkins controller | 8081 | TCP | Health-check probe + manual `curl` |

**Important:** the webhook endpoint is open by default (HMAC is the only auth layer). If Gitea runs on a known IP range, restrict to that range using the **Allowed CIDRs** field — the Rust side enforces it before the HMAC check.

## mTLS (mutual TLS)

For environments where Gitea requires client-cert authentication, set `trustedCertificatesPem` to your corporate CA PEM. The Rust `reqwest` client will validate the Gitea server cert against Mozilla CA + your PEM.

Mutual TLS (client cert sent *by* Jenkins to Gitea) is **not** supported in v1.x — open a feature request if you need it.

## Backup strategy

Back up these paths daily:

```
$JENKINS_HOME/config.xml                     # global config (includes GiteaServers)
$JENKINS_HOME/plugins/gitea.jpi              # the plugin itself
$JENKINS_HOME/jobs/*/config.xml              # Multibranch Pipeline configs (GiteaSCMSource)
$JENKINS_HOME/credentials.xml                # Gitea tokens
```

The webhook port, HMAC secret, and bearer token all live in `config.xml` **in plaintext**. This matches Jenkins' behaviour for other global secrets (CSRF issuer, agent secrets). For encryption-at-rest, use the filesystem-level encryption (LUKS on Linux, KMS in cloud).

## Monitoring

### Prometheus metrics

Scrape `http://<jenkins>:8081/gitea-webhook/metrics` every 30s. Available series:

| Metric | Labels | Meaning |
|---|---|---|
| `gitea_webhook_requests_total` | `event_type, status` | Counter of all webhook deliveries. `status` ∈ `ok\|bad_request\|unauthorized\|rate_limited\|forbidden\|duplicate\|error` |
| `gitea_webhook_callback_latency_seconds` | `event_type` | JNI callback → Java dispatch latency histogram |

Recommended Grafana alerts (PromQL):

```promql
# Webhook error rate > 5% over 5 min
sum(rate(gitea_webhook_requests_total{status=~"error|unauthorized|forbidden"}[5m]))
  /
sum(rate(gitea_webhook_requests_total[5m])) > 0.05

# Sustained rate-limit rejections (>10/min sustained)
rate(gitea_webhook_requests_total{status="rate_limited"}[1m]) > 0.16

# Callback latency p99 > 500ms
histogram_quantile(0.99, rate(gitea_webhook_callback_latency_seconds_bucket[5m])) > 0.5
```

### Jenkins System Log

Create a **Log Recorder** in `Manage Jenkins → System Log → New Log Recorder`:

- Name: `Gitea plugin`
- Loggers:
  - `org.jenkinsci.plugin.gitea` (INFO — covers Java side)
  - `org.jenkinsci.plugin.gitea.gitea_client` (INFO — Rust HTTP client tracing events)
  - `org.jenkinsci.plugin.gitea.gitea_client.server` (FINE — webhook dispatch detail)

Rust logs (via the tracing → JUL bridge, see `RustLogReceiver`) show up here alongside Java logs.

### Health endpoint

`GET http://<jenkins>:8081/gitea-webhook/health` returns `200 {"status":"ok"}` without auth. Use as Kubernetes liveness probe:

```yaml
livenessProbe:
  httpGet:
    path: /gitea-webhook/health
    port: 8081
  initialDelaySeconds: 30
  periodSeconds: 10
readinessProbe:
  httpGet:
    path: /gitea-webhook/health
    port: 8081
  initialDelaySeconds: 5
  periodSeconds: 5
```

## Scaling

| Dimension | Limit | Mitigation |
|---|---|---|
| Webhook throughput | ~1000 req/s per controller | Tokio multi-thread runtime uses all cores |
| Concurrent Gitea API calls | Bounded by connection pool size (32) | Tune in `rust/gitea-client/src/pool.rs` |
| Number of `GiteaSCMSource` | No hard limit; each costs ~1 KB RAM | None — Multibranch projects scale linearly |
| Polling interval | Min 60s (clamped) | Use webhooks as primary, polling only as fallback |

## Hot-reload

**Not supported.** When you update the plugin via Jenkins UI's "Upload Plugin" or restart-less upgrade, the Tokio runtime threads from the previous version persist until JVM exit. Symptoms:

- Old webhook port stays bound after upgrade
- `UnsatisfiedLinkError` if the new plugin's `.so` differs from the old one

**Workaround:** Always restart the Jenkins controller after a plugin upgrade. Schedule this as a maintenance window — typical downtime is 30-60s for a Jenkins LTS controller.

## Troubleshooting

### Webhook returns 401

- HMAC secret mismatch between Gitea and Jenkins → re-enter in `Gitea Servers` config
- Bearer token mismatch → check `Authorization: Bearer <token>` header is being sent by Gitea
- System clock skew > 30s can affect Gitea's HMAC computation (rare)

### Webhook returns 403 (Forbidden)

Source IP not in `Allowed CIDRs`. Add the Gitea server's IP range, or set `Allowed CIDRs` to empty (`0.0.0.0/0` is the implicit default when empty — not recommended).

### Webhook returns 429 (Too Many Requests)

Per-IP token bucket exhausted. The bucket refills at `rateLimitPerMinute / 60` tokens per second. Sustained webhook storm from a single Gitea server can trip this — increase `rateLimitPerMinute` to ~600 for high-traffic instances.

### Webhook returns 200 but no build triggers

- Check `Manage Jenkins → System Log → Gitea plugin` for `handleEvent` exceptions
- Confirm the Multibranch Pipeline's `GiteaSCMSource` matches the webhook's `repository.full_name`
- Polling may be needed if the webhook event type isn't supported (e.g. `issues`)

### `UnsatisfiedLinkError` at plugin load

The `.so` shipped in the `.hpi` does not match the JVM architecture. Check:
- `uname -m` on the controller (must be `x86_64` or `aarch64`)
- The bundled paths in the `.hpi`: `jar tf gitea.jpi | grep -E '\.so'` — should show `META-INF/native/linux/{amd64,aarch64}/libgitea_rust.so`

### Rust log events don't appear in Jenkins System Log

The log bridge is installed in `RustWebhookDispatcher.<clinit>`. If the native lib failed to load, the bridge is never installed. Check `org.jenkinsci.plugin.gitea.webhook.RustWebhookDispatcher` logger at SEVERE for "Failed to load libgitea_rust".

## Disaster recovery

If the plugin somehow wedges Jenkins (e.g. native lib crashes the JVM):

1. Stop Jenkins: `systemctl stop jenkins`
2. Move the plugin aside: `mv $JENKINS_HOME/plugins/gitea.jpi /tmp/`
3. Start Jenkins — the plugin's `@Extension` components are absent, no Rust code runs, and you can fix the rest of the config via UI
4. Restore the plugin once the underlying issue is fixed

## Known limitations (v1.x)

- ❌ Hot-reload (see above)
- ❌ mTLS client-cert outbound auth
- ❌ Windows Jenkins controllers (Linux only)
- ❌ Webhook retry queue (Gitea retry will hit the dedup cache and be silently skipped if already processed)
- ❌ Cluster mode (single-controller only — the Tokio runtime and HMAC secret are not shared across nodes)

## Version compatibility

| Plugin version | Jenkins LTS | JDK | Rust toolchain |
|---|---|---|---|
| 1.x | 2.479.3+ | 21 | 1.86+ |
| 2.x (planned) | TBD | 21 | 1.90+ |

For breaking changes between versions, see `CHANGES.md`.
