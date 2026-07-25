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
import hudson.init.InitMilestone;
import hudson.init.Initializer;
import java.util.logging.Level;
import java.util.logging.Logger;
import org.jenkinsci.plugin.gitea.servers.GiteaServers;

/**
 * Starts the Rust webhook server on Jenkins boot, after all extensions
 * have been augmented and the global {@link GiteaServers} config has been
 * loaded from disk.
 *
 * <h2>Why {@code @Initializer} and not {@code AsyncPeriodicWork}</h2>
 *
 * <p>Previously this used {@code AsyncPeriodicWork} with a
 * {@code Long.MAX_VALUE} recurrence period. That pattern is unreliable
 * on real Jenkins controllers — the work is scheduled but the first
 * execution can lag minutes behind boot, and on some setups (heavy
 * plugin list, slow Jenkins startup) it is skipped entirely. The result
 * was that the webhook server had to be started manually through
 * Script Console on every restart.</p>
 *
 * <p>{@link Initializer} annotated methods are invoked synchronously by
 * Jenkins during its startup sequence, at the
 * {@link InitMilestone#EXTENSIONS_AUGMENTED EXTENSIONS_AUGMENTED}
 * milestone — guaranteed to run after {@link GiteaServers#load()} and
 * before any jobs are loaded. That gives us a deterministic,
 * fast-starting hook for binding the webhook socket.</p>
 *
 * <h2>Subsequent reconfiguration</h2>
 *
 * After boot, every save of the Gitea global config calls
 * {@link RustWebhookDispatcher#configure(int, String, String, String, int)}
 * directly from {@link GiteaServers#configure}, which restarts the
 * server on the new port/secret if any setting changed. This class
 * only handles the initial boot-time start.
 */
@Extension
public class WebhookServerStarter {

    private static final Logger LOGGER = Logger.getLogger(WebhookServerStarter.class.getName());

    /**
     * Boot-time hook. Runs once after Jenkins has loaded global config and
     * augmented all extensions. Failures are logged and swallowed — a
     * misconfigured port must not abort Jenkins startup. The operator can
     * fix the config in {@code Manage Jenkins → System → Gitea Servers}
     * and save it; that re-triggers
     * {@link RustWebhookDispatcher#configure(int, String, String, String, int)}.
     */
    @Initializer(after = InitMilestone.EXTENSIONS_AUGMENTED, before = InitMilestone.JOB_LOADED)
    public static void start() {
        try {
            GiteaServers servers = GiteaServers.get();
            if (servers == null) {
                // Jenkins is shutting down before GiteaServers was registered.
                LOGGER.warning("GiteaServers not available — webhook server will not start automatically. "
                        + "It will start on the next global config save.");
                return;
            }
            RustWebhookDispatcher.configure(
                    servers.getWebhookPort(),
                    servers.getWebhookSecret(),
                    servers.getWebhookBearerToken(),
                    servers.getWebhookAllowedCidrs(),
                    servers.getWebhookRateLimitPerMinute(),
                    servers.getWebhookPath()
            );
        } catch (Throwable t) {
            LOGGER.log(Level.SEVERE,
                    "Failed to auto-start Gitea webhook server on boot — "
                            + "fix the Gitea Servers config and save it to retry.",
                    t);
        }
    }
}
