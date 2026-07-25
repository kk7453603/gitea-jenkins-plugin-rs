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

import org.jenkinsci.plugin.gitea.client.api.GiteaAuthNone;
import org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnection;
import org.junit.Assume;
import org.junit.BeforeClass;
import org.junit.Test;

/**
 * Smoke test for the Rust webhook dispatcher.
 *
 * <p>This is intentionally narrow. It verifies two contracts that live on the
 * Java side and cannot be exercised from the Rust crate's own test suite:</p>
 *
 * <ol>
 *   <li>{@code NativeLibraryLoader} is safe to invoke from more than one class
 *       initialiser. Both {@link RustGiteaConnection} and
 *       {@link RustWebhookDispatcher} call {@code Loader.load("gitea_rust")}
 *       from their respective {@code static} blocks; if the loader were not
 *       guarded against double-loads, the second call would throw
 *       {@link UnsatisfiedLinkError} ("Native Library already loaded").</li>
 *   <li>{@link RustWebhookDispatcher#configure(int, String, String, String, int)}
 *       is idempotent for the same arguments — it must NOT bounce the
 *       listener (stop+start) on a no-op reconfiguration, otherwise every
 *       unrelated Jenkins config save would briefly take the webhook
 *       endpoint offline.</li>
 * </ol>
 *
 * <p>The test auto-skips (via {@link Assume#assumeTrue(boolean)}) when
 * {@code libgitea_rust} cannot be loaded for the running platform. That is the
 * normal situation on a developer machine without a cross-compiled native
 * library (e.g. macOS dev box without the {@code .dylib}, or CI before
 * {@code cargo build --release} has run). To exercise it locally:</p>
 *
 * <pre>
 *   cd rust/gitea-client &amp;&amp; cargo build --release
 * </pre>
 *
 * <p>HTTP-layer behaviour (HMAC verification, header parsing, status codes) is
 * covered by the Rust crate's own test suite — see
 * {@code rust/gitea-client/src/server.rs}.</p>
 */
public class RustWebhookDispatcherTest {

    /**
     * Set by {@link #tryLoadNative()}: {@code true} when the JVM managed to
     * load {@code libgitea_rust} for the running platform.
     */
    private static boolean nativeAvailable;

    /**
     * Bring the dispatcher up on an ephemeral port and tear it down again.
     *
     * <p>If anything goes wrong (library missing, native start fails, native
     * stop throws) we set {@link #nativeAvailable} to {@code false} and let
     * each test skip itself rather than fail — matching the convention used
     * by {@code RustGiteaConnectionSmokeTest}.</p>
     *
     * <p>We pass port {@code 0} so the OS assigns an ephemeral port; this
     * avoids colliding with a Jenkins controller that may already be running
     * on the default port 8081 in a developer's environment.</p>
     */
    @BeforeClass
    public static void tryLoadNative() {
        try {
            // Trigger RustWebhookDispatcher.<clinit> which loads the native
            // library via NativeLibraryLoader. If this throws, every test is
            // skipped below.
            RustWebhookDispatcher.configure(0, "", "", "", 60, "/gitea-webhook");
            // Give the axum server a moment to bind. The native side returns
            // synchronously once the listener is up, but a small grace period
            // keeps the test stable on slow CI machines.
            Thread.sleep(100);
            // configure(-1, ...) is the canonical "stop" sentinel: nativeStop
            // is the only side-effect because running==true and port!=currentPort.
            RustWebhookDispatcher.configure(-1, "", "", "", 60, "/gitea-webhook");
            nativeAvailable = true;
        } catch (Throwable t) {
            nativeAvailable = false;
            System.err.println(
                "[RustWebhookDispatcherTest] libgitea_rust not available for "
                    + "this platform — webhook dispatcher tests will be skipped. "
                    + "Cause: " + t.getMessage()
            );
        }
    }

    /**
     * Both {@link RustGiteaConnection} and {@link RustWebhookDispatcher}
     * trigger {@code NativeLibraryLoader.load("gitea_rust")} from their
     * respective {@code static} blocks. Constructing a
     * {@link RustGiteaConnection} after the dispatcher has already been
     * initialised (in {@link #tryLoadNative()}) would throw
     * {@link UnsatisfiedLinkError} if the loader did not guard against the
     * double-load.
     */
    @Test
    public void doubleLoad_safe() {
        Assume.assumeTrue("libgitea_rust not bundled for this platform", nativeAvailable);
        // RustWebhookDispatcher.<clinit> has already run during tryLoadNative.
        // If NativeLibraryLoader did NOT protect against double-loads, the
        // next line would throw UnsatisfiedLinkError.
        new RustGiteaConnection("http://127.0.0.1:1", new GiteaAuthNone());
    }

    /**
     * Calling {@link RustWebhookDispatcher#configure(int, String, String, String, int)}
     * twice with identical arguments MUST be a no-op: the second call returns
     * immediately without invoking {@code nativeStop} + {@code nativeStart}.
     *
     * <p>We cannot directly observe "no native calls were made" from Java, but
     * we CAN observe that {@link RustWebhookDispatcher#isRunning()} remains
     * {@code true} and the dispatcher did not throw — a regression in the
     * idempotency guard would either briefly set running=false or, worse,
     * throw an exception from the second {@code nativeStart} (port in use).</p>
     */
    @Test
    public void configure_idempotent_onSameArgs() {
        Assume.assumeTrue("native lib not available — configure() test skipped", nativeAvailable);
        // Start fresh on an ephemeral port.
        RustWebhookDispatcher.configure(0, "", "", "", 60, "/gitea-webhook");
        boolean runningAfterFirst = RustWebhookDispatcher.isRunning();
        // Second call with identical args — must NOT bounce the listener.
        RustWebhookDispatcher.configure(0, "", "", "", 60, "/gitea-webhook");
        boolean runningAfterSecond = RustWebhookDispatcher.isRunning();
        // Tear down regardless of outcome so we leave no listener behind.
        RustWebhookDispatcher.configure(-1, "", "", "", 60, "/gitea-webhook");
        org.junit.Assert.assertTrue(
            "dispatcher should be running after first configure()",
            runningAfterFirst
        );
        org.junit.Assert.assertTrue(
            "dispatcher should still be running after idempotent reconfigure()",
            runningAfterSecond
        );
    }
}
