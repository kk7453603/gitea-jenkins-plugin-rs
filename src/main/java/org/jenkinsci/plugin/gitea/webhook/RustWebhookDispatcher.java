/*
 * The MIT License
 *
 * Copyright (c) 2024, CloudBees, Inc.
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
package org.jenkinsci.plugin.gitea.webhook;

import com.fasterxml.jackson.databind.ObjectMapper;
import edu.umd.cs.findbugs.annotations.NonNull;
import hudson.Extension;
import java.io.IOException;
import java.util.logging.Level;
import java.util.logging.Logger;
import jenkins.scm.api.SCMEvent;
import jenkins.scm.api.SCMHeadEvent;
import jenkins.scm.api.SCMSourceEvent;
import org.jenkinsci.plugin.gitea.GiteaCreateSCMEvent;
import org.jenkinsci.plugin.gitea.GiteaDeleteSCMEvent;
import org.jenkinsci.plugin.gitea.GiteaPullSCMEvent;
import org.jenkinsci.plugin.gitea.GiteaPushSCMEvent;
import org.jenkinsci.plugin.gitea.GiteaReleaseSCMEvent;
import org.jenkinsci.plugin.gitea.GiteaRepositorySCMEvent;
import org.jenkinsci.plugin.gitea.client.api.GiteaCreateEvent;
import org.jenkinsci.plugin.gitea.client.api.GiteaDeleteEvent;
import org.jenkinsci.plugin.gitea.client.api.GiteaPullRequestEvent;
import org.jenkinsci.plugin.gitea.client.api.GiteaPushEvent;
import org.jenkinsci.plugin.gitea.client.api.GiteaReleaseEvent;
import org.jenkinsci.plugin.gitea.client.api.GiteaRepositoryEvent;
import org.jenkinsci.plugin.gitea.client.impl.NativeLibraryLoader;
import org.kohsuke.accmod.Restricted;
import org.kohsuke.accmod.restrictions.NoExternalUse;

/**
 * Receives webhook events from the native Rust HTTP server (via JNI callback)
 * and dispatches them into the Jenkins {@link SCMEvent} bus.
 *
 * <p>The Rust side ({@code rust/gitea-client/src/server.rs}) runs an axum HTTP
 * listener on the port configured in {@code GiteaServers.webhookPort}. For each
 * incoming {@code POST /gitea-webhook/post} request, after optional HMAC
 * verification, it calls back into the JVM through
 * {@code RustWebhookDispatcher.handleEvent(String type, String json)} where
 * {@code type} is the lowercased value of the {@code X-Gitea-Event} header
 * ({@code "push"}, {@code "pull_request"}, {@code "create"}, {@code "delete"},
 * {@code "release"}, {@code "repository"}) and {@code json} is the raw request
 * body.</p>
 *
 * <p>This class is registered as a Jenkins {@link Extension} so that the native
 * library is loaded eagerly when the plugin starts. The actual server
 * lifecycle is driven by
 * {@link #configure(int, String, String, String, int)} which is invoked from
 * {@code WebhookServerStarter} (an {@link hudson.model.AsyncPeriodicWork}) on
 * Jenkins startup, and again whenever the global Gitea configuration is
 * saved.</p>
 *
 * <h2>Thread-safety</h2>
 *
 * The JNI callback may be invoked from any tokio worker thread that the Rust
 * runtime has attached to the JVM. The {@link #handleEvent} method is
 * stateless aside from the shared {@link ObjectMapper} (which is thread-safe
 * for reading) and the dispatcher's own logging, so no additional
 * synchronisation is required.
 */
@Extension
public class RustWebhookDispatcher {

    /**
     * Logger.
     */
    private static final Logger LOGGER = Logger.getLogger(RustWebhookDispatcher.class.getName());

    /**
     * Shared Jackson mapper for parsing webhook payloads into the existing
     * {@code GiteaXxxEvent} POJOs. {@code ObjectMapper} is thread-safe once
     * configured (we use the default configuration, which matches the upstream
     * POJO annotations).
     */
    private static final ObjectMapper MAPPER = new ObjectMapper();

    /**
     * Origin tag stamped on every {@link SCMEvent} we fire, so consumers can
     * distinguish Rust-delivered webhooks from other sources in logs.
     */
    private static final String ORIGIN = "Rust webhook server";

    static {
        try {
            NativeLibraryLoader.load("gitea_rust");
        } catch (UnsatisfiedLinkError e) {
            LOGGER.log(Level.SEVERE, "Failed to load libgitea_rust required by RustWebhookDispatcher", e);
            throw e;
        }
    }

    /**
     * Whether the native webhook server is currently running. Volatile because
     * {@link #configure} is called from the Jenkins config-save thread while
     * {@link #handleEvent} may already be firing on tokio worker threads.
     */
    private static volatile boolean running = false;

    /**
     * The port the current server is bound to (or {@code -1} if not running).
     */
    private static volatile int currentPort = -1;

    /**
     * The HMAC secret currently configured (empty string means "no
     * verification"). Stored in plaintext in memory only — never logged.
     */
    private static volatile String currentSecret = null;

    /**
     * The optional bearer token currently configured (empty string means
     * "no bearer check"). Stored in plaintext in memory only — never
     * logged. See {@link #configure(int, String, String, String, int)}.
     */
    private static volatile String currentBearer = null;

    /**
     * The comma-separated CIDR allowlist currently configured, or empty
     * string when no allowlist is in effect. Used to detect no-op
     * reconfigurations.
     */
    private static volatile String currentCidrs = null;

    /**
     * The per-IP rate limit (requests per minute) currently in effect.
     */
    private static volatile int currentRateLimit = -1;

    /**
     * Reconfigure the webhook server. Idempotent: if the server is already
     * running with the same settings this method is a no-op.
     *
     * <p>If any of the parameters changed the previous server is shut down
     * (via {@code nativeStop}) and a new one is started on the requested
     * port.</p>
     *
     * @param port               the TCP port to listen on (1-65535).
     * @param hmacSecret         the shared HMAC secret, or {@code null}/empty
     *                           to disable verification (insecure; the Rust
     *                           layer will log a warning).
     * @param bearerToken        optional static bearer token checked against
     *                           the {@code Authorization: Bearer …} header.
     *                           Empty / {@code null} disables the check.
     * @param allowedCidrs       comma-separated CIDR list (e.g.
     *                           {@code "10.0.0.0/8,192.168.0.0/16"}).
     *                           Empty / {@code null} means "allow all".
     * @param rateLimitPerMinute per-IP token bucket capacity & refill rate.
     *                           Values &le; 0 are clamped to 1 on the Rust
     *                           side.
     */
    public static synchronized void configure(
            int port,
            String hmacSecret,
            String bearerToken,
            String allowedCidrs,
            int rateLimitPerMinute) {
        String secret = (hmacSecret == null || hmacSecret.isEmpty()) ? "" : hmacSecret;
        String bearer = (bearerToken == null || bearerToken.isEmpty()) ? "" : bearerToken;
        String cidrs = (allowedCidrs == null) ? "" : allowedCidrs;
        if (running
                && port == currentPort
                && safeEq(secret, currentSecret)
                && safeEq(bearer, currentBearer)
                && safeEq(cidrs, currentCidrs)
                && rateLimitPerMinute == currentRateLimit) {
            // No change — avoid bouncing the listener on every save.
            return;
        }
        if (running) {
            try {
                nativeStop();
            } catch (Throwable t) {
                LOGGER.log(Level.WARNING, "nativeStop failed during reconfigure", t);
            }
            running = false;
            currentPort = -1;
            currentSecret = null;
            currentBearer = null;
            currentCidrs = null;
            currentRateLimit = -1;
        }
        try {
            nativeStart(port, secret, bearer, cidrs, rateLimitPerMinute);
            running = true;
            currentPort = port;
            currentSecret = secret;
            currentBearer = bearer;
            currentCidrs = cidrs;
            currentRateLimit = rateLimitPerMinute;
            LOGGER.log(Level.INFO, "Gitea webhook server started on port {0}", port);
        } catch (Throwable t) {
            // We deliberately do NOT log the secret or bearer here.
            LOGGER.log(Level.SEVERE, "nativeStart failed for port " + port, t);
        }
    }

    /**
     * Whether the webhook server believes itself to be running. For diagnostics
     * and tests only — the actual liveness is owned by the Rust side.
     *
     * @return {@code true} if {@link #configure} last successfully started the
     * server and no {@link #nativeStop} has been issued since.
     */
    public static boolean isRunning() {
        return running;
    }

    /**
     * The port the running server is bound to, or {@code -1} if not running.
     *
     * @return the current port.
     */
    public static int getCurrentPort() {
        return currentPort;
    }

    /**
     * JNI callback entry point. The Rust HTTP handler invokes this with the
     * raw payload of every authenticated webhook.
     *
     * <p>This method MUST NOT throw — any exception would propagate into the
     * JNI boundary and be reported as a Java exception pending on a native
     * frame, which is hard to diagnose. Failures are logged and swallowed.</p>
     *
     * @param type the lowercased {@code X-Gitea-Event} header value, e.g.
     *             {@code "push"}.
     * @param json the raw request body as a UTF-8 string.
     */
    @Restricted(NoExternalUse.class)
    public static void handleEvent(@NonNull String type, @NonNull String json) {
        try {
            dispatch(type, json);
        } catch (Throwable t) {
            // Never let an exception escape into JNI — see javadoc.
            LOGGER.log(Level.WARNING, "Webhook dispatch failed for type=" + type, t);
        }
    }

    /**
     * Map a webhook payload to the corresponding {@link SCMEvent} subclass and
     * fire it on the Jenkins SCM event bus.
     *
     * <p>The mapping mirrors the upstream {@code GiteaWebhookHandler} subclasses
     * that used to live inside each {@code GiteaXxxSCMEvent.HandlerImpl}. The
     * fired events are exactly the same as before — only the transport has
     * changed (Rust HTTP listener instead of Stapler).</p>
     *
     * @param type the event type (lowercase, matching the
     *             {@code X-Gitea-Event} header).
     * @param json the raw payload.
     * @throws IOException if Jackson fails to parse the payload (will be
     *                     caught and logged by {@link #handleEvent}).
     */
    private static void dispatch(String type, String json) throws IOException {
        switch (type) {
            case "push":
                GiteaPushEvent push = MAPPER.readValue(json, GiteaPushEvent.class);
                SCMHeadEvent.fireNow(new GiteaPushSCMEvent(push, ORIGIN));
                break;
            case "pull_request":
                GiteaPullRequestEvent pr = MAPPER.readValue(json, GiteaPullRequestEvent.class);
                SCMHeadEvent.fireNow(new GiteaPullSCMEvent(pr, ORIGIN));
                break;
            case "create":
                GiteaCreateEvent create = MAPPER.readValue(json, GiteaCreateEvent.class);
                SCMHeadEvent.fireNow(new GiteaCreateSCMEvent(create, ORIGIN));
                break;
            case "delete":
                GiteaDeleteEvent delete = MAPPER.readValue(json, GiteaDeleteEvent.class);
                SCMHeadEvent.fireNow(new GiteaDeleteSCMEvent(delete, ORIGIN));
                break;
            case "release":
                GiteaReleaseEvent release = MAPPER.readValue(json, GiteaReleaseEvent.class);
                SCMHeadEvent.fireNow(new GiteaReleaseSCMEvent(release, ORIGIN));
                break;
            case "repository":
                GiteaRepositoryEvent repo = MAPPER.readValue(json, GiteaRepositoryEvent.class);
                // Repository events affect the set of sources (not individual
                // heads), so they go on the SCMSourceEvent bus.
                SCMSourceEvent.fireNow(new GiteaRepositorySCMEvent(repo, ORIGIN));
                break;
            default:
                // Gitea may emit event types we do not handle (issues, commits,
                // etc.) — log at FINE so operators can see them if needed.
                LOGGER.log(Level.FINE, "Ignoring unsupported Gitea webhook event type: {0}", type);
        }
    }

    /**
     * Null-safe string equality. Used to detect no-op reconfigurations.
     */
    private static boolean safeEq(String a, String b) {
        return a == null ? b == null : a.equals(b);
    }

    /**
     * Start the native Rust webhook server. Implemented in
     * {@code rust/gitea-client/src/jni_webhook.rs} as
     * {@code Java_org_jenkinsci_plugin_gitea_webhook_RustWebhookDispatcher_nativeStart}.
     *
     * @param port               the TCP port to listen on.
     * @param hmacSecret         the shared HMAC secret, or empty string to
     *                           disable verification.
     * @param bearerToken        optional static bearer token checked against
     *                           the {@code Authorization: Bearer …} header;
     *                           empty string disables the check.
     * @param allowedCidrs       comma-separated CIDR allowlist, or empty
     *                           string for "allow all".
     * @param rateLimitPerMinute per-IP token bucket capacity & refill rate.
     */
    private static native void nativeStart(
        int port,
        String hmacSecret,
        String bearerToken,
        String allowedCidrs,
        int rateLimitPerMinute
    );

    /**
     * Stop the native Rust webhook server. Idempotent.
     */
    private static native void nativeStop();
}
