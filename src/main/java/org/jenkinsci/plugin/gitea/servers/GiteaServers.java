/*
 * The MIT License
 *
 * Copyright (c) 2017, CloudBees, Inc.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */
package org.jenkinsci.plugin.gitea.servers;

import edu.umd.cs.findbugs.annotations.CheckForNull;
import edu.umd.cs.findbugs.annotations.NonNull;
import hudson.Extension;
import hudson.ExtensionList;
import hudson.Util;
import hudson.util.ListBoxModel;
import java.net.URI;
import java.net.URISyntaxException;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashSet;
import java.util.Iterator;
import java.util.List;
import java.util.ListIterator;
import java.util.Locale;
import java.util.Set;
import java.util.logging.Level;
import java.util.logging.Logger;
import jenkins.model.GlobalConfiguration;
import jenkins.model.Jenkins;
import net.sf.json.JSONObject;
import org.apache.commons.lang.StringUtils;
import org.kohsuke.accmod.Restricted;
import org.kohsuke.accmod.restrictions.NoExternalUse;
import org.kohsuke.stapler.StaplerRequest2;
import org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection;
import org.jenkinsci.plugin.gitea.webhook.RustWebhookDispatcher;

/**
 * Represents the global configuration of Gitea servers.
 */
@Extension
public class GiteaServers extends GlobalConfiguration {

    /**
     * Default port the Rust webhook server binds to. Matches the value
     * referenced in {@code config.jelly} and the integration tests.
     */
    public static final int DEFAULT_WEBHOOK_PORT = 8081;

    private static final Logger LOGGER = Logger.getLogger(GiteaServers.class.getName());

    /**
     * The list of {@link GiteaServer}, this is subject to the constraint that there can only ever be
     * one entry for each {@link GiteaServer#getServerUrl()}.
     */
    private List<GiteaServer> servers;

    /**
     * TCP port the Rust webhook server listens on. Stored in the global
     * Jenkins config so operators can change it without touching system
     * properties.
     */
    private int webhookPort = DEFAULT_WEBHOOK_PORT;

    /**
     * Shared HMAC-SHA256 secret used to authenticate incoming webhook
     * deliveries. May be empty to disable verification (strongly
     * discouraged — see {@code AGENTS.md}).
     *
     * <p>Stored in plaintext in {@code config.xml}. This mirrors how
     * Jenkins itself stores other shared secrets (e.g. CSRF issuer, agent
     * secrets) — encryption-at-rest for global config fields is an
     * open Jenkins enhancement and is out of scope for this plugin. The
     * field is never logged at INFO+ by this class; the UI form uses
     * {@code <f:password/>} so the value is masked in the browser.</p>
     */
    private String webhookSecret = "";

    /**
     * Optional static bearer token (stage 16) checked against the
     * inbound {@code Authorization: Bearer …} header on every webhook
     * delivery. Empty string (the default) disables the check — useful
     * when Gitea is configured to rotate the HMAC secret but a static
     * credential is acceptable, or when an extra defence-in-depth layer
     * is desired on top of HMAC.
     *
     * <p>Stored in plaintext in {@code config.xml}, masked in the UI via
     * {@code <f:password/>}. Never logged at INFO+ by this class.</p>
     */
    private String webhookBearerToken = "";

    /**
     * Comma-separated CIDR allowlist (stage 16). Empty string (the
     * default) means "accept webhooks from any source IP". Non-empty
     * values are forwarded to the Rust server verbatim; entries that fail
     * to parse on the Rust side are skipped with a WARN log so the
     * operator can spot typos.
     *
     * <p>Example value: {@code "10.0.0.0/8,192.168.0.0/16,127.0.0.0/8"}.
     * Both IPv4 and IPv6 CIDRs are supported.</p>
     */
    private String webhookAllowedCidrs = "";

    /**
     * Per-IP token bucket capacity and refill rate, expressed in
     * requests per minute (stage 16). The Rust server divides this by
     * 60 to get the per-second refill rate. The default of {@code 60}
     * yields 1 request/sec sustained, with a burst of up to 60.
     *
     * <p>Values &le; 0 are clamped to 1 on the Rust side so each client
     * can always send at least one probe request.</p>
     */
    private int webhookRateLimitPerMinute = 60;

    /**
     * Additional CA certificates in PEM format. Appended on top of the
     * Mozilla CA bundle that the native Rust HTTP client trusts by default
     * — use this for self-signed Gitea instances or corporate CAs whose
     * roots are not in the Mozilla bundle.
     *
     * <p>The PEM may contain any number of {@code -----BEGIN CERTIFICATE-----}
     * blocks. Empty string (the default) means "trust only the Mozilla
     * CA bundle", i.e. the pre-stage-12 behaviour.</p>
     *
     * <p><strong>Hot-reload caveat:</strong> the PEM is pushed into the
     * native client once via
     * {@link org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection#nativeSetTrustedCertificates(byte[])}
     * during {@link #configure(com.fasterxml.stapler.StaplerRequest2, org.json.JSONObject)}.
     * The native side stores it in a write-once slot, so changing the PEM
     * requires a Jenkins restart to take effect (see {@code AGENTS.md}
     * "known limitations").</p>
     */
    private String trustedCertificatesPem = "";

    /**
     * Outbound HTTP/HTTPS/SOCKS5 proxy URL for all Gitea API requests made
     * by the native Rust client. Empty string (the default) means "no
     * explicit proxy" — the native side then falls back to the
     * {@code HTTP_PROXY} / {@code HTTPS_PROXY} / {@code NO_PROXY}
     * environment variables (the default {@code reqwest} behaviour).
     *
     * <p>Accepts schemes: {@code http://}, {@code https://},
     * {@code socks5://}, {@code socks5h://}.</p>
     *
     * <p><strong>Hot-reload caveat:</strong> like {@link #trustedCertificatesPem},
     * this value is pushed into the native client once via
     * {@link org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection#nativeSetProxy(String)}
     * during {@link #configure(com.fasterxml.stapler.StaplerRequest2, org.json.JSONObject)}.
     * The native side stores it in a write-once slot, so changing the URL
     * requires a Jenkins restart to take effect (see {@code AGENTS.md}
     * "known limitations").</p>
     */
    private String proxyUrl = "";

    /**
     * Optional Basic-auth username for the outbound proxy. Empty string
     * disables Basic auth. See {@link #proxyUrl}.
     */
    private String proxyUsername = "";

    /**
     * Optional Basic-auth password for the outbound proxy. The UI form uses
     * {@code <f:password/>} so the value is masked in the browser. This
     * field is never logged at INFO+ by this class. See {@link #proxyUrl}.
     */
    private String proxyPassword = "";

    /**
     * Comma-separated host patterns that bypass the proxy, e.g.
     * {@code "localhost,127.0.0.1,.internal.corp.com"}. The leading-dot
     * form matches the whole subdomain (mirrors cURL / Jenkins semantics).
     * Empty string means "no exclusions". See {@link #proxyUrl}.
     */
    private String noProxyHosts = "";

    /**
     * Adaptive polling interval in seconds — stage 10. When webhooks are
     * unavailable (Gitea behind a firewall, test instances), the native
     * Rust layer periodically polls {@code /repos/.../branches} using
     * ETag-based conditional requests and fires the same JNI callback as
     * the webhook layer when a change is detected.
     *
     * <p>{@code 0} (the default) disables polling — webhook-only mode.
     * Recommended values when enabled: 300–3600 (5 min – 1 h). The native
     * side clamps the effective interval to a 60 s floor.</p>
     *
     * <p>Targets are derived automatically from the configured
     * {@link #getServers()} list (each entry's server URL + resolved
     * credentials). Repositories are not enumerated here — the Java
     * dispatcher treats the synthetic push event as a generic "something
     * changed on this server" signal and relies on the standard
     * {@code SCMTrigger}-driven fetch to pick up the actual branch/PR
     * state.</p>
     */
    private int pollingIntervalSeconds = 0;

    /**
     * Constructor.
     */
    public GiteaServers() {
        load();
    }

    /**
     * Gets the {@link GiteaServers} singleton.
     *
     * @return the {@link GiteaServers} singleton.
     */
    public static GiteaServers get() {
        return ExtensionList.lookup(GlobalConfiguration.class).get(GiteaServers.class);
    }

    /**
     * Fix a serverUrl.
     *
     * @param serverUrl the server URL.
     * @return the normalized server URL.
     */
    @NonNull
    public static String normalizeServerUrl(@CheckForNull String serverUrl) {
        serverUrl = StringUtils.defaultString(serverUrl);
        try {
            URI uri = new URI(serverUrl).normalize();
            String scheme = uri.getScheme();
            if ("http".equals(scheme) || "https".equals(scheme)) {
                // we only expect http / https, but also these are the only ones where we know the authority
                // is server based, i.e. [userinfo@]server[:port]
                // DNS names must be US-ASCII and are case insensitive, so we force all to lowercase

                String host = uri.getHost() == null ? null : uri.getHost().toLowerCase(Locale.ENGLISH);
                int port = uri.getPort();
                if ("http".equals(scheme) && port == 80) {
                    port = -1;
                } else if ("https".equals(scheme) && port == 443) {
                    port = -1;
                }
                serverUrl = new URI(
                        scheme,
                        uri.getUserInfo(),
                        host,
                        port,
                        uri.getPath(),
                        uri.getQuery(),
                        uri.getFragment()
                ).toASCIIString();
            }
        } catch (URISyntaxException e) {
            // ignore, this was a best effort tidy-up
        }
        return serverUrl.replaceAll("/$", "");
    }

    /**
     * Checks if the supplied event url is for the specified server url (after consulting
     *
     * @param serverUrl the {@link GiteaServer#getServerUrl()}
     * @param eventUrl  the event url.
     * @return {@code true} if the event is a matching
     * {@link GiteaServer#getAliasUrl()} for registered {@link GiteaServer} instances)
     * @since 1.0.5
     */
    public static boolean isEventFor(String serverUrl, String eventUrl) {
        try {
            for (boolean alias : new boolean[]{false, true}) {
                URI serverUri;
                if (alias) {
                    GiteaServer server = GiteaServers.get().findServer(serverUrl);
                    if (server != null && StringUtils.isNotBlank(server.getAliasUrl())) {
                        serverUri = new URI(server.getAliasUrl());
                    } else {
                        continue;
                    }
                } else {
                    serverUri = new URI(serverUrl);
                }
                URI eventUri = new URI(eventUrl);
                if (!StringUtils.equalsIgnoreCase(serverUri.getHost(), eventUri.getHost())) {
                    continue;
                }
                if ("http".equals(serverUri.getScheme())) {
                    int serverPort = serverUri.getPort();
                    if (serverPort == -1) {
                        serverPort = 80;
                    }
                    if ("http".equals(eventUri.getScheme())) {
                        int eventPort = eventUri.getPort();
                        if (eventPort == -1) {
                            eventPort = 80;
                        }
                        if (serverPort != eventPort) {
                            continue;
                        }
                    } else if (!"https".equals(eventUri.getScheme())) {
                        continue;
                    }
                } else if ("https".equals(serverUri.getScheme())) {
                    int serverPort = serverUri.getPort();
                    if (serverPort == -1) {
                        serverPort = 443;
                    }
                    if ("https".equals(eventUri.getScheme())) {
                        int eventPort = eventUri.getPort();
                        if (eventPort == -1) {
                            eventPort = 443;
                        }
                        if (serverPort != eventPort) {
                            continue;
                        }
                    } else if (!"http".equals(eventUri.getScheme())) {
                        // may be the same just over plain
                        continue;
                    }
                }
                String serverPath = StringUtils.defaultIfBlank(serverUri.getPath(), "");
                String eventPath = StringUtils.defaultIfBlank(eventUri.getPath(), "/");
                if (eventPath.startsWith(serverPath + "/")) {
                    return true;
                }
            }
        } catch (URISyntaxException e) {
            return false;
        }
        return false;
    }

    /**
     * Returns {@code true} if and only if there is more than one configured endpoint.
     *
     * @return {@code true} if and only if there is more than one configured endpoint.
     */
    public boolean isEndpointSelectable() {
        return getServers().size() > 1;
    }

    /**
     * Populates a {@link ListBoxModel} with the endpoints.
     *
     * @return A {@link ListBoxModel} with all the endpoints
     */
    public ListBoxModel getServerItems() {
        ListBoxModel result = new ListBoxModel();
        for (GiteaServer endpoint : getServers()) {
            String serverUrl = endpoint.getServerUrl();
            String displayName = endpoint.getDisplayName();
            result.add(StringUtils.isBlank(displayName) ? serverUrl : displayName + " (" + serverUrl + ")", serverUrl);
        }
        return result;
    }

    /**
     * {@inheritDoc}
     *
     * <p>After the standard Stapler bind, this method also (re)starts the
     * Rust webhook server so that changes to the port or secret take effect
     * without requiring a Jenkins restart. The dispatcher is idempotent: if
     * neither value changed since the last call, the listener is left
     * untouched.</p>
     */
    @Override
    public boolean configure(StaplerRequest2 req, JSONObject json) throws FormException {
        req.bindJSON(this, json);
        // Bounce the native server if the port, secret, bearer, CIDR list,
        // or rate limit changed. We do this in a try/catch so a bind failure
        // (e.g. port in use) does not prevent the rest of the global config
        // from being saved.
        try {
            RustWebhookDispatcher.configure(
                    getWebhookPort(),
                    getWebhookSecret(),
                    getWebhookBearerToken(),
                    getWebhookAllowedCidrs(),
                    getWebhookRateLimitPerMinute()
            );
        } catch (Throwable t) {
            // The dispatcher itself logs the underlying native error; here we
            // just record that the global-config-save path hit it.
            LOGGER.log(Level.WARNING, "RustWebhookDispatcher.configure failed during save", t);
        }
        // Stage 12 — push the additional trust material into the native
        // Rust HTTP client. Idempotent: the native side stores it in a
        // write-once OnceCell, so only the FIRST non-empty value wins;
        // subsequent saves are silently ignored. This is consistent with
        // the plugin's broader hot-reload limitation (see AGENTS.md).
        try {
            String pem = getTrustedCertificatesPem();
            if (pem != null && !pem.isEmpty()) {
                RustGiteaConnection.nativeSetTrustedCertificates(pem.getBytes(java.nio.charset.StandardCharsets.UTF_8));
            }
        } catch (Throwable t) {
            // The native method is infallible at the JNI boundary, but
            // UnsatisfiedLinkError or a missing native lib would land here.
            // Log and continue — saving the rest of the global config is
            // more important than blocking on TLS setup.
            LOGGER.log(Level.WARNING, "nativeSetTrustedCertificates failed during save", t);
        }
        // Stage 13 — push the HTTP proxy configuration into the native
        // Rust client. Idempotent: the native side stores it in a
        // write-once OnceCell, so only the FIRST non-empty value wins;
        // subsequent saves are silently ignored. This is consistent with
        // the plugin's broader hot-reload limitation (see AGENTS.md).
        // Always invoked (even with an empty URL) so that an unset proxy
        // falls back to env vars on the Rust side.
        try {
            RustGiteaConnection.nativeSetProxy(buildProxyJson());
        } catch (Throwable t) {
            // The native method is infallible at the JNI boundary, but
            // UnsatisfiedLinkError or a missing native lib would land here.
            // Log and continue — saving the rest of the global config is
            // more important than blocking on proxy setup.
            LOGGER.log(Level.WARNING, "nativeSetProxy failed during save", t);
        }
        // Stage 10 — start or stop the adaptive polling loop. Unlike
        // TLS/proxy this slot IS hot-reloadable on the native side (the
        // previous JoinHandle is aborted on each start), so changes take
        // effect on save without a controller restart.
        int pollInterval = getPollingIntervalSeconds();
        if (pollInterval > 0) {
            String pollJson = buildPollConfigJson();
            try {
                RustGiteaConnection.nativeStartPolling(pollJson);
            } catch (Throwable t) {
                LOGGER.log(Level.WARNING, "nativeStartPolling failed during save", t);
            }
        } else {
            try {
                RustGiteaConnection.nativeStopPolling();
            } catch (Throwable t) {
                // The native method is infallible at the JNI boundary;
                // log at FINE since "no polling running" is the common case.
                LOGGER.log(Level.FINE, "nativeStopPolling failed during save", t);
            }
        }
        return true;
    }

    /**
     * Build the JSON document consumed by
     * {@link org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection#nativeSetProxy(String)}.
     *
     * <p>The shape is fixed on the Rust side ({@code ProxyConfig} struct
     * with camelCase serde rename). We assemble it by hand (rather than
     * going through {@code JSONObject}) because the consumer expects plain
     * JSON and the Jackson dependency is not guaranteed to be on the
     * classpath at this call site.</p>
     */
    private String buildProxyJson() {
        StringBuilder sb = new StringBuilder(96);
        sb.append('{');
        sb.append("\"url\":").append(quote(getProxyUrl())).append(',');
        sb.append("\"username\":").append(quote(getProxyUsername())).append(',');
        sb.append("\"password\":").append(quote(getProxyPassword())).append(',');
        sb.append("\"noProxyHosts\":").append(quote(getNoProxyHosts()));
        sb.append('}');
        return sb.toString();
    }

    /**
     * Minimal JSON string escaping. Handles the characters that matter for
     * proxy URLs / usernames / passwords: backslash, double-quote, and the
     * usual control chars. Sufficient because the consumer (Rust
     * {@code serde_json}) is strict about these.
     */
    private static String quote(String s) {
        if (s == null) {
            return "\"\"";
        }
        StringBuilder sb = new StringBuilder(s.length() + 8);
        sb.append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '\\':
                    sb.append("\\\\");
                    break;
                case '"':
                    sb.append("\\\"");
                    break;
                case '\n':
                    sb.append("\\n");
                    break;
                case '\r':
                    sb.append("\\r");
                    break;
                case '\t':
                    sb.append("\\t");
                    break;
                default:
                    if (c < 0x20) {
                        sb.append(String.format("\\u%04x", (int) c));
                    } else {
                        sb.append(c);
                    }
            }
        }
        sb.append('"');
        return sb.toString();
    }

    /**
     * TCP port the Rust webhook server listens on.
     *
     * @return the configured port (default {@value #DEFAULT_WEBHOOK_PORT}).
     */
    public int getWebhookPort() {
        return webhookPort;
    }

    /**
     * Set the Rust webhook server port. Persisted via global config.
     *
     * @param webhookPort the new port (1-65535).
     */
    @Restricted(NoExternalUse.class)
    public void setWebhookPort(int webhookPort) {
        this.webhookPort = webhookPort;
    }

    /**
     * The shared HMAC secret used to verify incoming webhooks. Empty string
     * disables verification. Never logged at INFO+ by this class.
     *
     * @return the secret (possibly empty, never {@code null}).
     */
    @Restricted(NoExternalUse.class)
    public String getWebhookSecret() {
        return webhookSecret == null ? "" : webhookSecret;
    }

    /**
     * Set the HMAC secret. Persisted via global config.
     *
     * @param webhookSecret the new secret; {@code null} is normalised to the
     *                      empty string.
     */
    @Restricted(NoExternalUse.class)
    public void setWebhookSecret(String webhookSecret) {
        this.webhookSecret = webhookSecret == null ? "" : webhookSecret;
    }

    /**
     * Optional bearer token checked on every inbound webhook (stage 16).
     * Empty string disables the check. Never logged at INFO+.
     *
     * @return the configured bearer token (possibly empty, never {@code null}).
     */
    @Restricted(NoExternalUse.class)
    public String getWebhookBearerToken() {
        return webhookBearerToken == null ? "" : webhookBearerToken;
    }

    /**
     * Set the optional bearer token. Persisted via global config.
     *
     * @param webhookBearerToken the new token; {@code null} is normalised
     *                           to the empty string (which disables the
     *                           check).
     */
    @Restricted(NoExternalUse.class)
    public void setWebhookBearerToken(String webhookBearerToken) {
        this.webhookBearerToken = webhookBearerToken == null ? "" : webhookBearerToken;
    }

    /**
     * Comma-separated CIDR allowlist (stage 16). Empty string means
     * "accept webhooks from any source IP". See {@link #webhookAllowedCidrs}.
     *
     * @return the configured CIDR list (never {@code null}).
     */
    @Restricted(NoExternalUse.class)
    public String getWebhookAllowedCidrs() {
        return webhookAllowedCidrs == null ? "" : webhookAllowedCidrs;
    }

    /**
     * Set the comma-separated CIDR allowlist. Persisted via global config.
     *
     * @param webhookAllowedCidrs the new list, e.g.
     *                            {@code "10.0.0.0/8,192.168.0.0/16"};
     *                            {@code null} is normalised to the empty
     *                            string.
     */
    @Restricted(NoExternalUse.class)
    public void setWebhookAllowedCidrs(String webhookAllowedCidrs) {
        this.webhookAllowedCidrs = webhookAllowedCidrs == null ? "" : webhookAllowedCidrs;
    }

    /**
     * Per-IP rate limit (requests per minute). See
     * {@link #webhookRateLimitPerMinute}.
     *
     * @return the configured rate limit (default {@code 60}).
     */
    @Restricted(NoExternalUse.class)
    public int getWebhookRateLimitPerMinute() {
        return webhookRateLimitPerMinute;
    }

    /**
     * Set the per-IP rate limit. Persisted via global config.
     *
     * @param webhookRateLimitPerMinute the new limit; values &le; 0 are
     *                                  clamped to 1 on the Rust side.
     */
    @Restricted(NoExternalUse.class)
    public void setWebhookRateLimitPerMinute(int webhookRateLimitPerMinute) {
        this.webhookRateLimitPerMinute = webhookRateLimitPerMinute;
    }

    /**
     * Additional CA certificates in PEM format, or empty string to trust
     * only the Mozilla CA bundle. See {@link #trustedCertificatesPem}.
     *
     * @return the configured PEM (never {@code null}).
     */
    @Restricted(NoExternalUse.class)
    public String getTrustedCertificatesPem() {
        return trustedCertificatesPem == null ? "" : trustedCertificatesPem;
    }

    /**
     * Set additional CA certificates in PEM format. Persisted via global
     * config. {@code null} is normalised to the empty string.
     *
     * @param trustedCertificatesPem the PEM bytes; may be {@code null} or empty.
     */
    @Restricted(NoExternalUse.class)
    public void setTrustedCertificatesPem(String trustedCertificatesPem) {
        this.trustedCertificatesPem = trustedCertificatesPem == null ? "" : trustedCertificatesPem;
    }

    /**
     * Outbound proxy URL, or empty string to fall back to env vars.
     * See {@link #proxyUrl}.
     *
     * @return the proxy URL (never {@code null}).
     */
    @Restricted(NoExternalUse.class)
    public String getProxyUrl() {
        return proxyUrl == null ? "" : proxyUrl;
    }

    /**
     * Set the outbound proxy URL. Persisted via global config.
     * {@code null} is normalised to the empty string.
     *
     * @param proxyUrl the new URL; may be {@code null} or empty.
     */
    @Restricted(NoExternalUse.class)
    public void setProxyUrl(String proxyUrl) {
        this.proxyUrl = proxyUrl == null ? "" : proxyUrl;
    }

    /**
     * Outbound proxy Basic-auth username. Empty string disables auth.
     *
     * @return the username (never {@code null}).
     */
    @Restricted(NoExternalUse.class)
    public String getProxyUsername() {
        return proxyUsername == null ? "" : proxyUsername;
    }

    /**
     * Set the outbound proxy Basic-auth username.
     *
     * @param proxyUsername the username; {@code null} is normalised to the
     *                      empty string.
     */
    @Restricted(NoExternalUse.class)
    public void setProxyUsername(String proxyUsername) {
        this.proxyUsername = proxyUsername == null ? "" : proxyUsername;
    }

    /**
     * Outbound proxy Basic-auth password. Never logged at INFO+ by this
     * class. See {@link #proxyPassword}.
     *
     * @return the password (never {@code null}).
     */
    @Restricted(NoExternalUse.class)
    public String getProxyPassword() {
        return proxyPassword == null ? "" : proxyPassword;
    }

    /**
     * Set the outbound proxy Basic-auth password.
     *
     * @param proxyPassword the password; {@code null} is normalised to the
     *                      empty string.
     */
    @Restricted(NoExternalUse.class)
    public void setProxyPassword(String proxyPassword) {
        this.proxyPassword = proxyPassword == null ? "" : proxyPassword;
    }

    /**
     * Comma-separated host patterns that bypass the proxy.
     * See {@link #noProxyHosts}.
     *
     * @return the no-proxy list (never {@code null}).
     */
    @Restricted(NoExternalUse.class)
    public String getNoProxyHosts() {
        return noProxyHosts == null ? "" : noProxyHosts;
    }

    /**
     * Set the comma-separated no-proxy host list.
     *
     * @param noProxyHosts the list; {@code null} is normalised to the empty
     *                     string.
     */
    @Restricted(NoExternalUse.class)
    public void setNoProxyHosts(String noProxyHosts) {
        this.noProxyHosts = noProxyHosts == null ? "" : noProxyHosts;
    }

    /**
     * Adaptive polling interval in seconds. {@code 0} disables polling.
     * See {@link #pollingIntervalSeconds}.
     *
     * @return the configured interval (default {@code 0}).
     */
    @Restricted(NoExternalUse.class)
    public int getPollingIntervalSeconds() {
        return pollingIntervalSeconds;
    }

    /**
     * Set the adaptive polling interval. Persisted via global config.
     *
     * @param pollingIntervalSeconds the new interval; {@code 0} disables.
     */
    @Restricted(NoExternalUse.class)
    public void setPollingIntervalSeconds(int pollingIntervalSeconds) {
        this.pollingIntervalSeconds = pollingIntervalSeconds;
    }

    /**
     * Build the JSON document consumed by
     * {@link org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection#nativeStartPolling(String)}.
     *
     * <p>Targets are derived from {@link #getServers()}: each configured
     * server contributes a single target whose {@code owner}/{@code repo}
     * are left blank, because the Rust polling loop sends a server-scoped
     * "branches" probe. The credentials are resolved via the same
     * {@code AuthenticationTokens} conversion used by the main API
     * client. Servers without a server URL or with unresolvable
     * credentials contribute an anonymous ({@code authType=0}) target —
     * Gitea public repos still respond to anonymous polling.</p>
     */
    private String buildPollConfigJson() {
        // We assemble JSON by hand to avoid pulling Jackson into this
        // call site (consistent with buildProxyJson()).
        StringBuilder targets = new StringBuilder();
        for (GiteaServer endpoint : getServers()) {
            String serverUrl = endpoint.getServerUrl();
            if (serverUrl == null || serverUrl.isEmpty()) {
                continue;
            }
            if (targets.length() > 0) {
                targets.append(',');
            }
            targets.append('{');
            targets.append("\"serverUrl\":").append(quote(serverUrl)).append(',');
            // Resolve the GiteaAuth to the (authType, authSecret) pair
            // that the native decode_auth expects. We mirror the encoding
            // in RustGiteaConnection's constructor: Token=1, Basic=2.
            int authType = 0;
            String authSecret = "";
            try {
                org.jenkinsci.plugin.gitea.client.api.GiteaAuth auth =
                        jenkins.authentication.tokens.api.AuthenticationTokens.convert(
                                org.jenkinsci.plugin.gitea.client.api.GiteaAuth.class,
                                endpoint.credentials());
                if (auth instanceof org.jenkinsci.plugin.gitea.client.api.GiteaAuthToken) {
                    authType = 1;
                    authSecret = ((org.jenkinsci.plugin.gitea.client.api.GiteaAuthToken) auth).getToken();
                } else if (auth instanceof org.jenkinsci.plugin.gitea.client.api.GiteaAuthUser) {
                    org.jenkinsci.plugin.gitea.client.api.GiteaAuthUser user =
                            (org.jenkinsci.plugin.gitea.client.api.GiteaAuthUser) auth;
                    authType = 2;
                    authSecret = user.getUsername() + ":" + user.getPassword();
                }
            } catch (Throwable t) {
                // Credentials resolution can fail at runtime (missing
                // creds, decryption errors). Fall back to anonymous —
                // the poll will still succeed for public repositories.
                LOGGER.log(Level.FINE, "Could not resolve GiteaAuth for polling target " + serverUrl, t);
            }
            targets.append("\"authType\":").append(authType).append(',');
            targets.append("\"authSecret\":").append(quote(authSecret)).append(',');
            // Server-scoped probe: leave owner/repo empty. The native
            // polling loop treats these as "skip" markers (an empty
            // owner causes the /repos//branches URL to 404 and the
            // target is logged-and-skipped). The stage-10 design doc
            // leaves repository enumeration to a future enhancement.
            targets.append("\"owner\":").append(quote("")).append(',');
            targets.append("\"repo\":").append(quote(""));
            targets.append('}');
        }
        StringBuilder sb = new StringBuilder(64);
        sb.append('{');
        sb.append("\"intervalSeconds\":").append(getPollingIntervalSeconds()).append(',');
        sb.append("\"targets\":[").append(targets).append(']');
        sb.append('}');
        return sb.toString();
    }

    /**
     * Gets the list of endpoints.
     *
     * @return the list of endpoints
     */
    @NonNull
    public synchronized List<GiteaServer> getServers() {
        return servers == null || servers.isEmpty()
                ? Collections.<GiteaServer>emptyList()
                : Collections.unmodifiableList(servers);
    }

    /**
     * Sets the list of endpoints.
     *
     * @param servers the list of endpoints.
     */
    public synchronized void setServers(@CheckForNull List<? extends GiteaServer> servers) {
        Jenkins.get().checkPermission(Jenkins.ADMINISTER);
        List<GiteaServer> eps = new ArrayList<>(Util.fixNull(servers));
        // remove duplicates and empty urls
        Set<String> serverUrls = new HashSet<>();
        for (ListIterator<GiteaServer> iterator = eps.listIterator(); iterator.hasNext(); ) {
            GiteaServer endpoint = iterator.next();
            String serverUrl = endpoint.getServerUrl();
            if (StringUtils.isBlank(serverUrl) || serverUrls.contains(serverUrl)) {
                iterator.remove();
                continue;
            }
            serverUrls.add(serverUrl);
        }
        this.servers = eps;
        save();
    }

    /**
     * Adds an endpoint.
     *
     * @param endpoint the endpoint to add.
     * @return {@code true} if the list of endpoints was modified
     */
    public synchronized boolean addServer(@NonNull GiteaServer endpoint) {
        List<GiteaServer> endpoints = new ArrayList<>(getServers());
        for (GiteaServer ep : endpoints) {
            if (ep.getServerUrl().equals(endpoint.getServerUrl())) {
                return false;
            }
        }
        endpoints.add(endpoint);
        setServers(endpoints);
        return true;
    }

    /**
     * Updates an existing endpoint (or adds if missing).
     *
     * @param endpoint the endpoint to update.
     */
    public synchronized void updateServer(@NonNull GiteaServer endpoint) {
        List<GiteaServer> endpoints = new ArrayList<>(getServers());
        boolean found = false;
        for (int i = 0; i < endpoints.size(); i++) {
            GiteaServer ep = endpoints.get(i);
            if (ep.getServerUrl().equals(endpoint.getServerUrl())) {
                endpoints.set(i, endpoint);
                found = true;
                break;
            }
        }
        if (!found) {
            endpoints.add(endpoint);
        }
        setServers(endpoints);
    }

    /**
     * Removes an endpoint.
     *
     * @param endpoint the endpoint to remove.
     * @return {@code true} if the list of endpoints was modified
     */
    public boolean removeServer(@NonNull GiteaServer endpoint) {
        return removeServer(endpoint.getServerUrl());
    }

    /**
     * Removes an endpoint.
     *
     * @param serverUrl the server URL to remove.
     * @return {@code true} if the list of endpoints was modified
     */
    public synchronized boolean removeServer(@CheckForNull String serverUrl) {
        serverUrl = normalizeServerUrl(serverUrl);
        boolean modified = false;
        List<GiteaServer> endpoints = new ArrayList<>(getServers());
        for (Iterator<GiteaServer> iterator = endpoints.iterator(); iterator.hasNext(); ) {
            if (serverUrl.equals(iterator.next().getServerUrl())) {
                iterator.remove();
                modified = true;
            }
        }
        setServers(endpoints);
        return modified;
    }

    /**
     * Checks to see if the supplied server URL is defined in the global configuration.
     *
     * @param serverUrl the server url to check.
     * @return the global configuration for the specified server url or {@code null} if not defined.
     */
    @CheckForNull
    public synchronized GiteaServer findServer(@CheckForNull String serverUrl) {
        serverUrl = normalizeServerUrl(serverUrl);
        for (GiteaServer endpoint : getServers()) {
            if (serverUrl.equals(endpoint.getServerUrl())) {
                return endpoint;
            }
        }
        return null;
    }

}
