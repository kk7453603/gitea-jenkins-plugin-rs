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
 * {@code /META-INF/native/<os>/<arch>/lib<name>.<ext>} where {@code <os>}
 * is {@code linux}, {@code darwin}, or {@code windows} and {@code <arch>}
 * is {@code amd64}, {@code aarch64}, or {@code x86}. We detect the
 * platform at load time and pick the right resource path.</p>
 *
 * <p>We cannot use {@link System#loadLibrary(String)} because that relies
 * on {@code java.library.path}, which we do not control on the Jenkins
 * controller. Instead we copy the binary to a temporary file on disk
 * (marked {@code deleteOnExit}) and load it by absolute path.</p>
 *
 * <p>If the exact {@code <os>/<arch>} pair is not bundled, we fall back to
 * any sibling directory with the same OS (so an amd64-only build still
 * loads on an arm64 controller if a universal variant is bundled). If
 * nothing matches we throw an {@link UnsatisfiedLinkError} with a clear
 * message naming the resource paths we tried.</p>
 */
public final class NativeLibraryLoader {

    private static final Set<String> LOADED =
            Collections.newSetFromMap(new ConcurrentHashMap<String, Boolean>());

    /**
     * Extract the library named {@code libName} from
     * {@code /META-INF/native/<os>/<arch>/} and load it.
     *
     * <p>Idempotent: a second call with the same {@code libName} is a no-op.
     *
     * @param libName library name without platform prefix/suffix
     *                (e.g. {@code "gitea_rust"} → {@code libgitea_rust.so}
     *                on Linux, {@code libgitea_rust.dylib} on macOS).
     * @throws UnsatisfiedLinkError if no platform-matching resource exists.
     */
    @SuppressFBWarnings(value = "RV_RETURN_VALUE_IGNORED_BAD_PRACTICE",
            justification = "deleteOnExit return value is intentionally ignored.")
    public static void load(String libName) {
        if (!LOADED.add(libName)) {
            return;
        }
        String mappedName = System.mapLibraryName(libName);
        // Try the exact platform path first, then fall back to alternate
        // arches for the same OS (lets an amd64-only build load on arm64
        // hosts when a universal binary is bundled).
        String osTag = osTag();
        String[] archCandidates = archCandidates();
        UnsatisfiedLinkError lastError = null;
        for (String arch : archCandidates) {
            String resourcePath = "/META-INF/native/" + osTag + "/" + arch + "/" + mappedName;
            try (InputStream in = NativeLibraryLoader.class.getResourceAsStream(resourcePath)) {
                if (in == null) {
                    continue;
                }
                String suffix = mappedName.endsWith(".dylib") ? ".dylib"
                        : mappedName.endsWith(".dll") ? ".dll" : ".so";
                Path tmp = Files.createTempFile("gitea-rust-", suffix);
                tmp.toFile().deleteOnExit();
                Files.copy(in, tmp, StandardCopyOption.REPLACE_EXISTING);
                System.load(tmp.toString());
                return;
            } catch (UnsatisfiedLinkError e) {
                // Wrong ELF/mach-o architecture — remember and try the next candidate.
                lastError = e;
                LOADED.remove(libName);
            } catch (IOException e) {
                LOADED.remove(libName);
                throw new ExceptionInInitializerError(e);
            }
        }
        if (lastError != null) {
            throw lastError;
        }
        // No candidate resource was found at all.
        LOADED.remove(libName);
        StringBuilder tried = new StringBuilder();
        for (String arch : archCandidates) {
            tried.append(" /META-INF/native/").append(osTag).append('/').append(arch).append('/').append(mappedName);
        }
        throw new UnsatisfiedLinkError(
                "Missing native library for os=" + osTag
                        + " arch=" + String.join("/", archCandidates) + ". Tried:" + tried);
    }

    /**
     * Map {@code os.name} system property to our resource path segment.
     */
    private static String osTag() {
        String os = System.getProperty("os.name", "").toLowerCase(java.util.Locale.ROOT);
        if (os.contains("linux")) return "linux";
        if (os.contains("mac") || os.contains("darwin")) return "darwin";
        if (os.contains("windows")) return "windows";
        return os;  // fall through — resource lookup will fail with a clear message
    }

    /**
     * Map {@code os.arch} system property to our resource path segment(s).
     * Returns a list so callers can try the exact match first and fall back
     * to alternates (e.g. an amd64 binary on an aarch64 host via Rosetta).
     */
    private static String[] archCandidates() {
        String arch = System.getProperty("os.arch", "").toLowerCase(java.util.Locale.ROOT);
        if (arch.equals("aarch64") || arch.equals("arm64")) {
            return new String[]{"aarch64", "amd64"};
        }
        if (arch.equals("amd64") || arch.equals("x86_64") || arch.equals("x86-64")) {
            return new String[]{"amd64"};
        }
        if (arch.equals("x86") || arch.equals("i386") || arch.equals("i486")
                || arch.equals("i586") || arch.equals("i686")) {
            return new String[]{"x86"};
        }
        // Unknown arch — return as-is so the error message is informative.
        return new String[]{arch};
    }

    private NativeLibraryLoader() {
    }
}
