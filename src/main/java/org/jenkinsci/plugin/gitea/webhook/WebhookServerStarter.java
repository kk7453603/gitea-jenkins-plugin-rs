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

import hudson.Extension;
import hudson.model.AsyncPeriodicWork;
import hudson.model.TaskListener;
import java.io.IOException;
import org.jenkinsci.plugin.gitea.servers.GiteaServers;

/**
 * One-shot {@link AsyncPeriodicWork} that starts the Rust webhook server
 * shortly after Jenkins finishes booting.
 *
 * <p>We use {@link AsyncPeriodicWork} with a recurrence period of
 * {@link Long#MAX_VALUE} so the task runs exactly once, in a background
 * thread, after Jenkins has finished loading its global configuration. The
 * first execution reads the webhook settings from {@link GiteaServers}
 * and hands them to
 * {@link RustWebhookDispatcher#configure(int, String, String, String, int)}.</p>
 *
 * <p>Subsequent saves of the Gitea global config call
 * {@link RustWebhookDispatcher#configure(int, String, String, String, int)}
 * directly from {@link GiteaServers#configure}, which restarts the server
 * on the new port/secret if any setting changed.</p>
 *
 * <h2>Why not start from a static initialiser?</h2>
 *
 * {@link RustWebhookDispatcher}'s static block only loads the native library;
 * it must not call {@code nativeStart} because {@link GiteaServers} may not
 * have been loaded yet (its {@link GiteaServers#load()} happens during
 * {@code Jenkins.start()}), and binding a socket before configuration is
 * loaded would either bind on the wrong port or fail outright.
 */
@Extension
public class WebhookServerStarter extends AsyncPeriodicWork {

    /**
     * Builds a new starter. {@link AsyncPeriodicWork}'s constructor takes the
     * human-readable name used in thread dumps and the periodic-work log.
     */
    public WebhookServerStarter() {
        super("Gitea webhook server starter");
    }

    /**
     * Run once, never again. {@link Long#MAX_VALUE} effectively disables
     * rescheduling; {@link AsyncPeriodicWork} will not invoke this task a
     * second time within any reasonable Jenkins uptime.
     *
     * @return {@link Long#MAX_VALUE}.
     */
    @Override
    public long getRecurrencePeriod() {
        return Long.MAX_VALUE;
    }

    /**
     * Read the Gitea global configuration and hand it to the dispatcher.
     *
     * <p>Failures here are logged but never rethrown — we do not want a
     * misconfigured port to prevent Jenkins from finishing its startup.</p>
     *
     * @param listener unused — we log through {@code java.util.logging}.
     * @throws IOException never, but the parent signature declares it.
     * @throws InterruptedException never, but the parent signature declares it.
     */
    @Override
    protected void execute(TaskListener listener) throws IOException, InterruptedException {
        try {
            GiteaServers servers = GiteaServers.get();
            if (servers == null) {
                // Jenkins is shutting down before GiteaServers was registered.
                return;
            }
            RustWebhookDispatcher.configure(
                    servers.getWebhookPort(),
                    servers.getWebhookSecret(),
                    servers.getWebhookBearerToken(),
                    servers.getWebhookAllowedCidrs(),
                    servers.getWebhookRateLimitPerMinute()
            );
        } catch (Throwable t) {
            // Log and swallow — see class javadoc.
            try {
                listener.error("Failed to start Gitea webhook server: " + t);
            } catch (Throwable ignored) {
                // listener may itself be broken; fall through to logger.
            }
        }
    }
}
