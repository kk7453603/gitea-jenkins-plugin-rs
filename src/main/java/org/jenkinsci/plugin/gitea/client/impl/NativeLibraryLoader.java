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
package org.jenkinsci.plugin.gitea.client.impl;

import edu.umd.cs.findbugs.annotations.SuppressFBWarnings;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.Collections;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Extracts the bundled native library from the classpath and loads it via
 * {@link System#load(String)}.
 *
 * <p>The native library lives in the plugin jar under
 * {@code /META-INF/native/linux/amd64/lib<name>.so}. We cannot use
 * {@link System#loadLibrary(String)} because that relies on
 * {@code java.library.path}, which we do not control on the Jenkins
 * controller. Instead we copy the {@code .so} to a temporary file on disk
 * (marked {@code deleteOnExit}) and load it by absolute path.</p>
 *
 * <p>This loader is intentionally Linux x86_64-only for the MVP. On any
 * other architecture the resource will be absent and we throw an
 * {@link UnsatisfiedLinkError} with a clear message. See
 * {@code IMPLEMENTATION_PLAN.md} for the cross-platform story.</p>
 */
public final class NativeLibraryLoader {

    /**
     * Names of libraries that have already been loaded by this classloader.
     *
     * <p>The Jenkins plugin now has two static initializers that both call
     * {@link #load} for {@code "gitea_rust"}:
     * {@code RustGiteaConnection.&lt;clinit&gt;} (the API client shim) and
     * {@code RustWebhookDispatcher.&lt;clinit&gt;} (the webhook listener).
     * On some JVMs a second {@link System#load(String)} of the same absolute
     * path throws {@link UnsatisfiedLinkError} ("Native library already
     * loaded"), which would crash whichever class happens to initialise
     * second. We therefore short-circuit any repeat load request for the
     * same logical library name within this classloader.</p>
     *
     * <p>The set is keyed by the logical library name (e.g.
     * {@code "gitea_rust"}), not by the on-disk temp path, because the path
     * changes between calls (we generate a fresh temp file each time).</p>
     */
    private static final Set<String> LOADED =
            Collections.newSetFromMap(new ConcurrentHashMap<String, Boolean>());

    /**
     * Extract the library named {@code libName} from
     * {@code /META-INF/native/linux/amd64/} and load it.
     *
     * <p>Idempotent: a second call with the same {@code libName} is a no-op.
     * This matters because both {@code RustGiteaConnection} and
     * {@code RustWebhookDispatcher} invoke {@code load("gitea_rust")} from
     * their respective static initialisers, and class initialisation order is
     * not guaranteed.</p>
     *
     * @param libName the library name without platform-specific prefix/suffix
     *                (e.g. {@code "gitea_rust"} → {@code libgitea_rust.so}).
     * @throws UnsatisfiedLinkError if the library resource is missing or
     *                              cannot be loaded.
     */
    @SuppressFBWarnings(value = "RV_RETURN_VALUE_IGNORED_BAD_PRACTICE",
            justification = "deleteOnExit return value is intentionally ignored.")
    public static void load(String libName) {
        // `Set.add` returns false if the element was already present, which
        // atomically collapses concurrent first-time loads onto a single
        // winner — exactly the semantics we want.
        if (!LOADED.add(libName)) {
            return;
        }
        String mappedName = System.mapLibraryName(libName); // libgitea_rust.so on Linux
        String resourcePath = "/META-INF/native/linux/amd64/" + mappedName;
        try (InputStream in = NativeLibraryLoader.class.getResourceAsStream(resourcePath)) {
            if (in == null) {
                // Roll back the "loaded" marker so a later retry (e.g. after a
                // classpath fix) can actually attempt the load again.
                LOADED.remove(libName);
                throw new UnsatisfiedLinkError(
                        "Missing native library: " + resourcePath
                                + " (the Rust core is only bundled for linux/amd64 in this build)");
            }
            Path tmp = Files.createTempFile("gitea-rust-", ".so");
            // best-effort cleanup on JVM shutdown; the file is small.
            tmp.toFile().deleteOnExit();
            Files.copy(in, tmp, StandardCopyOption.REPLACE_EXISTING);
            System.load(tmp.toString());
        } catch (IOException e) {
            LOADED.remove(libName);
            throw new ExceptionInInitializerError(e);
        } catch (UnsatisfiedLinkError e) {
            // If the JVM refuses to load (e.g. wrong ELF architecture), roll
            // back so a future call with a corrected library can succeed.
            LOADED.remove(libName);
            throw e;
        }
    }

    private NativeLibraryLoader() {
    }
}
