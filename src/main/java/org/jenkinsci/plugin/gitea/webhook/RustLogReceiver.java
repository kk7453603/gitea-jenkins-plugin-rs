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
 * AUTHORS OR COPYRIGHT HOLDERS BE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */
package org.jenkinsci.plugin.gitea.webhook;

import hudson.Extension;
import java.util.logging.Level;
import java.util.logging.Logger;

/**
 * Receives {@code tracing} events forwarded from the native Rust core and
 * writes them into the standard Jenkins {@code java.util.logging} hierarchy.
 *
 * <p>Each event arrives as a {@code (level, target, message)} tuple via the
 * JNI callback {@link #handleLog(String, String, String)}. We map:</p>
 *
 * <ul>
 *   <li>{@code ERROR} → {@link Level#SEVERE}</li>
 *   <li>{@code WARN}  → {@link Level#WARNING}</li>
 *   <li>{@code INFO}  → {@link Level#INFO}</li>
 *   <li>anything else → {@link Level#FINE}</li>
 * </ul>
 *
 * <p>The target (typically a Rust module path like {@code gitea_client::server})
 * is appended to the prefix {@code org.jenkinsci.plugin.gitea.} so the
 * Jenkins System Log UI can filter Rust logs alongside their Java
 * counterparts under a single namespace.</p>
 *
 * <h2>Configuration</h2>
 *
 * <p>Operators create a {@code Log Recorder} in {@code Manage Jenkins →
 * System Log → New Log Recorder} with name {@code Rust core} and logger
 * {@code org.jenkinsci.plugin.gitea}, then pick a level (INFO is the
 * recommended default — DEBUG/TRACE are dropped at the Rust layer to
 * avoid flooding).</p>
 *
 * <h2>Performance</h2>
 *
 * <p>The Rust side attaches the calling tokio worker thread to the JVM on
 * each forwarded event, then calls this method synchronously. There is no
 * background queue — events are delivered inline. This is acceptable
 * because we only forward INFO/WARN/ERROR (DEBUG/TRACE are filtered at the
 * Rust layer) and the typical event rate is &lt; 10 events/sec.</p>
 */
@Extension
public class RustLogReceiver {

    /** Common prefix for all loggers used by the Rust core. */
    public static final String LOG_NAMESPACE = "org.jenkinsci.plugin.gitea";

    /**
     * JNI callback target — invoked from {@code log_bridge.rs::forward_to_java}.
     *
     * @param level  one of {@code "ERROR"}, {@code "WARN"}, {@code "INFO"}.
     *               Anything else (e.g. a future {@code "DEBUG"} if the
     *               Rust filter is relaxed) maps to {@link Level#FINE}.
     * @param target Rust module path that emitted the event (e.g.
     *               {@code "gitea_client::server"}). Used to namespace the
     *               JUL logger so operators can filter per-module.
     * @param message human-readable event text. May be multi-line.
     */
    public static void handleLog(String level, String target, String message) {
        // Sanitize the target — replace Rust "::" with "." so it matches
        // the JUL dotted namespace convention. This means a Rust event
        // from `gitea_client::server` ends up under
        // `org.jenkinsci.plugin.gitea.gitea_client.server`, which is
        // ugly but consistent and filterable.
        String safeTarget = target == null ? "" : target.replace("::", ".");
        String loggerName = safeTarget.isEmpty()
                ? LOG_NAMESPACE
                : LOG_NAMESPACE + "." + safeTarget;
        Logger logger = Logger.getLogger(loggerName);

        Level julLevel;
        String lvl = level == null ? "" : level;
        switch (lvl) {
            case "ERROR":
                julLevel = Level.SEVERE;
                break;
            case "WARN":
                julLevel = Level.WARNING;
                break;
            case "INFO":
                julLevel = Level.INFO;
                break;
            default:
                julLevel = Level.FINE;
                break;
        }

        // Log with no thrown exception — the Rust side is responsible for
        // surfacing the underlying error string in the message body (the
        // tracing macro does this automatically via the `error = %e` field).
        logger.log(julLevel, message == null ? "" : message);
    }
}
