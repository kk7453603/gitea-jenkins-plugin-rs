# Proxy Migration — Porting corporate proxy configurations

Corporate Jenkins deployments often have complex proxy requirements. This guide covers the common patterns and how to map them to this plugin's configuration.

---

## 1. What this plugin already supports

Before adding corporate proxy logic, check if the existing support is sufficient:

| Feature | Where | How to configure |
|---|---|---|
| **Static proxy URL** | `GiteaServers.proxyUrl` UI field | `http://corp-proxy:3128` |
| **Proxy auth (Basic)** | `proxyUsername` + `proxyPassword` UI fields | Standard Basic auth to proxy |
| **Proxy bypass list** | `noProxyHosts` UI field (comma-separated) | `internal.gitea.corp,localhost,127.0.0.1` |
| **Jenkins global proxy fallback** | `buildProxyJson()` in GiteaServers | Automatic — if `proxyUrl` empty, reads `Jenkins.get().proxy` |
| **Environment variable fallback** | `reqwest` default behavior | `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` env vars |
| **SOCKS5 proxy** | `reqwest` with `socks` feature | `socks5://corp-proxy:1080` or `socks5h://` |
| **HTTPS proxy** | `reqwest` | `https://corp-proxy:443` |

**99% of corporate proxy needs are covered by these features.** Do not add custom proxy code unless the corporate setup truly requires something beyond standard HTTP/HTTPS/SOCKS5.

---

## 2. Common corporate proxy patterns

### Pattern A: Single corporate proxy for everything

**Corporate setup:** All outbound HTTPS goes through `http://proxy.corp:3128`.

**Migration:** Set in UI:
- `Proxy URL`: `http://proxy.corp:3128`
- `No proxy hosts`: `localhost,127.0.0.1,.internal.corp`

Or set Jenkins global proxy in `Manage Jenkins → System → Proxy`. The plugin falls back to this automatically.

**Effort:** 5 minutes. No code change.

---

### Pattern B: Multiple Gitea servers with different proxies

**Corporate setup:** Internal Gitea uses no proxy, public Gitea.com uses corp proxy.

**Problem:** `GiteaServers.proxyUrl` is global — applies to all servers.

**Solution options:**

#### Option B1: Use Jenkins global proxy + `noProxyHosts`

If internal Gitea hostname is known:
- Set Jenkins global proxy to `http://proxy.corp:3128`
- Set `noProxyHosts` to include internal Gitea hostname

**Limitation:** `noProxyHosts` is also global. Can't have different proxies per Gitea server.

#### Option B2: Add per-server proxy override

If you need true per-server proxy routing, extend `GiteaServer` (note: this is a single server entry, not `GiteaServers` global config):

```java
// In GiteaServer.java (the per-server class, not GiteaServers):
private String proxyOverride = "";  // empty = use global

@Restricted(NoExternalUse.class)
public String getProxyOverride() {
    return proxyOverride == null ? "" : proxyOverride;
}
```

Then in `buildProxyJson()` (in `GiteaServers`), iterate per-server and use override if set. This requires changes to the JNI bridge (per-server proxy instead of global).

**Effort:** 2-3 hours. Requires extending JNI signature.

#### Option B3: Multiple GiteaServers instances

Configure two `GiteaServer` entries pointing to the same Gitea URL but with different proxy settings via Jenkins global + `noProxyHosts` overrides.

**Limitation:** Awkward, confuses users.

**Recommended:** Option B1 for simple cases. Option B2 if truly needed.

---

### Pattern C: Proxy with authentication

**Corporate setup:** Proxy requires Basic auth with rotating credentials.

**Migration:**
- `Proxy URL`: `http://proxy.corp:3128`
- `Proxy Username`: corp username
- `Proxy Password`: corp password

Credentials are stored in `config.xml` in plaintext. For encryption-at-rest, use Jenkins filesystem encryption (LUKS) or a credentials vault.

**For rotating credentials:** This plugin does not yet support automatic proxy credential rotation. See [`JNI-EXTENSIONS.md` §custom-auth](./JNI-EXTENSIONS.md) if you need to implement this.

---

### Pattern D: PAC (Proxy Auto-Configuration) file

**Corporate setup:** Corporation uses a `.pac` file to route proxy decisions.

**Problem:** `reqwest` does not support PAC files natively. PAC requires JavaScript execution.

**Solutions:**

1. **Manual resolution:** Read the PAC file, determine which proxy is used for Gitea, configure that proxy directly.

2. **External resolver:** Run a small sidecar process that resolves PAC and exposes the proxy URL via HTTP. Add a custom JNI bridge that queries this resolver before each request.

3. **Defer to system:** Some corporate environments set `HTTP_PROXY`/`HTTPS_PROXY` env vars based on PAC. If so, leave `proxyUrl` empty and let `reqwest` use the env vars.

**Recommended:** Option 3 if possible. Option 1 for static PAC. Option 2 only if absolutely necessary.

---

### Pattern E: Proxy with TLS inspection (man-in-the-middle)

**Corporate setup:** Corporate proxy intercepts HTTPS, re-signs with corporate CA. Without trust, the plugin gets `CertificateValidationException`.

**Migration:**
- Export corporate CA root certificate as PEM
- Paste into `Trusted certificates (PEM)` UI field

This is the most common corporate proxy issue. The plugin's `tls.rs` already supports adding arbitrary PEM CAs on top of the Mozilla bundle.

**Effort:** 5 minutes (assuming you have the corp CA PEM).

---

### Pattern F: SOCKS5 proxy with DNS resolution via proxy

**Corporate setup:** Internal DNS not resolvable from Jenkins. Must use SOCKS5h (DNS via proxy).

**Migration:**
- `Proxy URL`: `socks5h://proxy.corp:1080`

The `h` suffix tells `reqwest` to resolve DNS through the proxy. Standard `socks5://` resolves locally.

---

### Pattern G: Per-repo proxy routing (advanced)

**Corporate setup:** Some repos are public (no proxy), some are behind corp firewall (proxy).

**This is not supported.** The plugin uses one proxy per Jenkins controller. If you need per-repo proxy, options are:

1. Run two Jenkins controllers (one with proxy, one without)
2. Use Jenkins global `noProxyHosts` with repo hostnames
3. Build a custom JNI bridge that reads repo name from the request and selects proxy — see [`examples/multi-proxy.md`](./examples/multi-proxy.md)

---

## 3. How the plugin resolves the proxy

The resolution order (first match wins):

```
1. GiteaServers.proxyUrl (explicit UI config)
   │  empty? ↓
2. Jenkins.get().proxy (Jenkins global proxy, "Manage Jenkins → System → Proxy")
   │  null or empty? ↓
3. HTTP_PROXY / HTTPS_PROXY env vars (reqwest default)
   │  not set? ↓
4. No proxy — direct connection
```

`noProxyHosts` (from either GiteaServers or Jenkins global) applies to all of these — hosts in the bypass list never use the proxy.

---

## 4. Debugging proxy issues

### Check what proxy is actually used

In Jenkins Script Console (`Manage Jenkins → Script Console`):

```groovy
def s = org.jenkinsci.plugin.gitea.servers.GiteaServers.get();
println "Proxy URL: " + s.getProxyUrl();
println "Proxy user: " + s.getProxyUsername();
println "No proxy: " + s.getNoProxyHosts();
def jp = jenkins.model.Jenkins.get().proxy;
if (jp != null) {
    println "Jenkins global proxy: " + jp.getName() + ":" + jp.getPort();
}
```

### Check Rust-side tracing logs

The Rust side logs proxy resolution at DEBUG. Set logger `org.jenkinsci.plugin.gitea.gitea_client.proxy` to FINEST in Jenkins System Log.

### Test direct connection from Jenkins host

```bash
# On Jenkins controller:
curl -v --proxy http://corp-proxy:3128 https://gitea.corp/api/v1/version
# Should return JSON
```

If this fails, the proxy is unreachable or requires auth.

### Test from inside Docker container (if Jenkins runs in Docker)

```bash
docker compose exec jenkins curl -v --proxy http://corp-proxy:3128 https://gitea.corp/api/v1/version
```

Docker networking may require `--network=host` or explicit proxy configuration.

---

## 5. Common pitfalls

### Pitfall 1: Proxy env vars in Jenkins service

If you set `HTTPS_PROXY` in `/etc/systemd/system/jenkins.service`, it applies to **all** outbound traffic from Jenkins — including the Rust plugin. This is usually what you want, but be aware:

- The Rust plugin reads these env vars as a fallback (after Jenkins global and GiteaServers UI config)
- Other Jenkins plugins also read these env vars

### Pitfall 2: Proxy auth character encoding

If your proxy password contains special characters (`@`, `:`, `/`), URL-encode them:

```bash
# Wrong: password "p@ss:w0rd" breaks URL parsing
http://user:p@ss:w0rd@proxy:3128

# Right: URL-encoded
http://user:p%40ss%3Aw0rd@proxy:3128
```

### Pitfall 3: Trusting `X-Forwarded-For`

If you're behind a corp proxy that adds `X-Forwarded-For`, the plugin's IP CIDR check sees the proxy IP, not the original client. Document this in your security review.

### Pitfall 4: MITM proxy re-signing

Corp TLS inspection proxies re-sign all HTTPS traffic. The plugin will reject these unless you trust the corp CA via `trustedCertificatesPem`. This is by design — silent trust would defeat TLS.

### Pitfall 5: SOCKS5 vs SOCKS5h

- `socks5://` — DNS resolved locally, then SOCKS5 connects to the IP
- `socks5h://` — DNS resolved by the proxy (use when local DNS can't resolve the target)

Most corp setups need `socks5h://` because internal hostnames are only in the corp DNS.

---

## 6. Migration checklist for corporate proxy

- [ ] Identify the corp proxy URL(s) — `http://`, `https://`, `socks5://`, or `socks5h://`
- [ ] Identify auth method — none / Basic / NTLM / Kerberos (NTLM and Kerberos not supported)
- [ ] Identify the corp CA (if TLS inspection is used) — export as PEM
- [ ] Identify the bypass list — internal hostnames that should not go through proxy
- [ ] Configure in Jenkins UI:
    - `Manage Jenkins → System → Proxy` (Jenkins global) OR
    - `Manage Jenkins → System → Gitea Servers → HTTP Proxy` (per-plugin)
- [ ] Test: `curl` from Jenkins host with same proxy config
- [ ] Verify: Jenkins System Log shows successful Gitea API calls (no proxy errors)
- [ ] Document: which Gitea servers use which proxy, for ops team

---

## 7. When existing proxy support is not enough

If the corporate proxy setup truly requires something beyond what's supported:

1. Document the exact requirement (which proxy, which auth, which routing)
2. Check [`examples/multi-proxy.md`](./examples/multi-proxy.md) — template for per-host proxy routing
3. If that doesn't fit, open a design discussion before writing code. Proxy code is security-sensitive — wrong choices leak credentials or break TLS.

Most "we need custom proxy" requests turn out to be:
- A config issue (use Jenkins global proxy)
- A trust issue (use `trustedCertificatesPem`)
- A DNS issue (use `socks5h://`)

Actual code changes are rare.
