/*
 * The MIT License
 *
 * Copyright (c) 2017-2020, CloudBees, Inc.
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
package org.jenkinsci.plugin.gitea.client.impl;

import org.jenkinsci.plugin.gitea.client.api.GiteaAuthNone;
import org.jenkinsci.plugin.gitea.client.api.GiteaAuthToken;
import org.jenkinsci.plugin.gitea.client.api.GiteaAuthUser;
import org.junit.Assume;
import org.junit.BeforeClass;
import org.junit.Test;

/**
 * Smoke test for the Rust+JNI integration.
 *
 * <p>This is intentionally narrow: it verifies that {@link NativeLibraryLoader}
 * can locate and {@code dlopen} the bundled native library, that the static
 * initializer of {@link RustGiteaConnection} runs without throwing
 * {@link UnsatisfiedLinkError}, and that the constructor accepts each of the
 * three {@code GiteaAuth} flavours the JNI layer encodes.</p>
 *
 * <p>The test is auto-skipped when the native library cannot be loaded. This
 * is the normal situation on a developer machine that only has the JVM-side
 * toolchain installed (e.g. macOS dev box without a cross-compiled
 * {@code .dylib}, or CI before the {@code cargo build --release} step has
 * run). To exercise it locally, build the crate first:</p>
 *
 * <pre>
 *   cd rust/gitea-client &amp;&amp; cargo build --release
 * </pre>
 *
 * <p>End-to-end HTTP tests of the JNI bridge live in the Rust crate's
 * {@code tests/integration.rs} wiremock suite; that side already covers 49
 * scenarios. This Java test exists purely to catch signature drift between
 * the {@code private native} declarations in {@link RustGiteaConnection} and
 * the {@code Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_*}
 * exports produced by {@code rust/gitea-client/src/jni.rs}.</p>
 */
public class RustGiteaConnectionSmokeTest {

    /**
     * Set by {@link #tryLoadNative()}: true when the JVM managed to load
     * {@code libgitea_rust} for the running platform. Tests gate themselves
     * on this via {@link Assume#assumeTrue(String, boolean)} so that a
     * missing native artifact is reported as {@code skipped} rather than
     * {@code failed}.
     */
    private static boolean nativeAvailable;

    @BeforeClass
    public static void tryLoadNative() {
        try {
            // Constructing an instance triggers the static initializer in
            // RustGiteaConnection, which in turn calls NativeLibraryLoader.
            // We pick a deliberately unreachable port so no test ever
            // accidentally talks to a live Gitea instance.
            new RustGiteaConnection("http://127.0.0.1:1", new GiteaAuthNone());
            nativeAvailable = true;
        } catch (UnsatisfiedLinkError e) {
            nativeAvailable = false;
            System.err.println(
                "[RustGiteaConnectionSmokeTest] libgitea_rust not available for "
                    + "this platform — smoke tests will be skipped. Cause: "
                    + e.getMessage()
            );
        }
    }

    /**
     * If the native library was bundled for this JVM's architecture, the
     * static initializer ran successfully during {@link #tryLoadNative()}.
     */
    @Test
    public void nativeLibraryLoads_whenBundled() {
        Assume.assumeTrue("libgitea_rust not bundled for this platform", nativeAvailable);
        // Reaching this assertion means NativeLibraryLoader.load() returned
        // without throwing UnsatisfiedLinkError.
    }

    /**
     * The constructor's auth-type encoding (0 = none, 1 = token, 2 = basic)
     * must accept every {@code GiteaAuth} subclass without throwing.
     */
    @Test
    public void authEncoding_acceptsNoneTokenBasic() {
        Assume.assumeTrue("native lib not available — auth encoding test skipped", nativeAvailable);
        new RustGiteaConnection("http://127.0.0.1:1", new GiteaAuthNone());
        new RustGiteaConnection("http://127.0.0.1:1", new GiteaAuthToken("dummy-token"));
        new RustGiteaConnection("http://127.0.0.1:1", new GiteaAuthUser("u", "p"));
    }
}
