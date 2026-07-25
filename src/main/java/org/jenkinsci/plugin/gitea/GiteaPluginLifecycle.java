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
package org.jenkinsci.plugin.gitea;

import hudson.Plugin;
import java.util.logging.Level;
import java.util.logging.Logger;
import org.jenkinsci.plugin.gitea.webhook.RustWebhookDispatcher;

/**
 * Plugin lifecycle wrapper — calls {@link RustWebhookDispatcher#nativeStop()}
 * when Jenkins unloads the plugin OR the JVM is shutting down.
 *
 * <p>This was the single biggest hot-reload issue in v1.0/1.1: the Tokio
 * runtime inside {@code libgitea_rust.so} was created via
 * {@code once_cell::Lazy} and never explicitly shut down, so a plugin
 * reload (Jenkins → "Plugins → Reload") left the worker threads alive,
 * the listening socket open, and the JVM holding a stale native handle.
 * Subsequent reloads then failed with "Address already in use" or
 * "Native library already loaded".</p>
 *
 * <p>This class fixes it by being a {@link Plugin} — Jenkins invokes
 * {@link #stop()} during plugin unmount (hot-reload or Jenkins shutdown).
 * We also register a {@link Runtime#addShutdownHook(Thread) JVM shutdown
 * hook} as a belt-and-suspenders for the case where Jenkins is killed
 * with SIGKILL and bypasses {@link Plugin#stop()}.</p>
 *
 * <p>Registered in {@code META-INF/MANIFEST.MF} as {@code Plugin-Class}.
 * Jenkins core instantiates this class via reflection on plugin load
 * and calls {@link #start()} / {@link #stop()}.</p>
 */
public class GiteaPluginLifecycle extends Plugin {

    private static final Logger LOGGER = Logger.getLogger(GiteaPluginLifecycle.class.getName());

    /** JVM shutdown hook thread — kept as a field so we can remove it cleanly on plugin unload. */
    private Thread jvmShutdownHook;

    /**
     * Called by Jenkins when the plugin is loaded. We register a JVM-level
     * shutdown hook in addition to {@link #stop()} because Jenkins does
     * not always call {@link Plugin#stop()} during a hard exit (SIGKILL,
     * OOM killer, container stop with timeout).
     */
    @Override
    public void start() throws Exception {
        super.start();
        jvmShutdownHook = new Thread(this::performNativeStop, "gitea-plugin-jvm-shutdown-hook");
        try {
            Runtime.getRuntime().addShutdownHook(jvmShutdownHook);
            LOGGER.fine("Gitea plugin lifecycle started, JVM shutdown hook registered");
        } catch (IllegalStateException e) {
            // JVM is already shutting down — rare but possible if the plugin
            // is loaded during shutdown. Log and continue; stop() will be
            // called by Jenkins if it gets the chance.
            LOGGER.log(Level.FINE, "Cannot add JVM shutdown hook (VM shutting down?)", e);
        } catch (SecurityException e) {
            LOGGER.log(Level.WARNING, "Security manager prevented shutdown hook registration", e);
        }
    }

    /**
     * Called by Jenkins when the plugin is being unloaded (hot-reload or
     * Jenkins shutdown). Triggers native server stop + tokio runtime
     * cleanup so the next plugin load starts from a clean slate.
     */
    @Override
    public void stop() throws Exception {
        performNativeStop();
        if (jvmShutdownHook != null) {
            try {
                Runtime.getRuntime().removeShutdownHook(jvmShutdownHook);
            } catch (IllegalStateException | SecurityException ignored) {
                // VM already shutting down OR security manager blocked removal — fine.
            }
            jvmShutdownHook = null;
        }
        super.stop();
    }

    /**
     * Best-effort native cleanup. Catches all throwables because a JNI
     * failure (e.g. {@code UnsatisfiedLinkError} if the native lib was
     * already unloaded) must not abort the broader plugin/Jenkins
     * shutdown sequence.
     */
    private void performNativeStop() {
        try {
            RustWebhookDispatcher.nativeStop();
            LOGGER.fine("RustWebhookDispatcher.nativeStop() completed during plugin lifecycle");
        } catch (Throwable t) {
            LOGGER.log(Level.WARNING,
                    "RustWebhookDispatcher.nativeStop() failed during lifecycle event — "
                            + "tokio threads may leak until JVM exit",
                    t);
        }
    }
}
