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

import com.fasterxml.jackson.databind.ObjectMapper;
import org.jenkinsci.plugin.gitea.client.api.GiteaAnnotatedTag;
import org.jenkinsci.plugin.gitea.client.api.GiteaAuth;
import org.jenkinsci.plugin.gitea.client.api.GiteaAuthUser;
import org.jenkinsci.plugin.gitea.client.api.GiteaAuthToken;
import org.jenkinsci.plugin.gitea.client.api.GiteaBranch;
import org.jenkinsci.plugin.gitea.client.api.GiteaCommitDetail;
import org.jenkinsci.plugin.gitea.client.api.GiteaCommitStatus;
import org.jenkinsci.plugin.gitea.client.api.GiteaConnection;
import org.jenkinsci.plugin.gitea.client.api.GiteaHook;
import org.jenkinsci.plugin.gitea.client.api.GiteaIssue;
import org.jenkinsci.plugin.gitea.client.api.GiteaIssueState;
import org.jenkinsci.plugin.gitea.client.api.GiteaOrganization;
import org.jenkinsci.plugin.gitea.client.api.GiteaOwner;
import org.jenkinsci.plugin.gitea.client.api.GiteaPullRequest;
import org.jenkinsci.plugin.gitea.client.api.GiteaRelease;
import org.jenkinsci.plugin.gitea.client.api.GiteaRepository;
import org.jenkinsci.plugin.gitea.client.api.GiteaTag;
import org.jenkinsci.plugin.gitea.client.api.GiteaUser;
import org.jenkinsci.plugin.gitea.client.api.GiteaVersion;

import java.io.IOException;
import java.io.InputStream;
import java.util.EnumSet;
import java.util.List;
import java.util.Set;

/**
 * {@link GiteaConnection} backed by the native Rust HTTP client ({@code libgitea_rust.so}).
 *
 * <p>Each method on {@link GiteaConnection} delegates to a {@code private static native}
 * method that crosses JNI into the Rust crate at {@code rust/gitea-client/}. The Rust
 * side returns raw JSON; this class parses it with a shared Jackson
 * {@link ObjectMapper} into the existing POJO hierarchy.</p>
 *
 * <p>The {@code (serverUrl, authType, authSecret)} triple is reconstructed on every
 * call rather than cached on the Rust side, because the upstream
 * {@link GiteaConnection} contract is stateless beyond {@link #close()} (which is a
 * no-op for this implementation — the Tokio runtime is process-global).</p>
 */
public class RustGiteaConnection implements GiteaConnection {

    private static final ObjectMapper MAPPER = new ObjectMapper();

    /** Auth type encoding — must stay in sync with {@code jni::decode_auth} in Rust. */
    private static final int AUTH_NONE = 0;
    private static final int AUTH_TOKEN = 1;
    private static final int AUTH_BASIC = 2;

    static {
        NativeLibraryLoader.load("gitea_rust");
    }

    private final String serverUrl;
    private final int authType;
    private final String authSecret;

    public RustGiteaConnection(String serverUrl, GiteaAuth auth) {
        this.serverUrl = serverUrl;
        if (auth instanceof GiteaAuthToken) {
            this.authType = AUTH_TOKEN;
            this.authSecret = ((GiteaAuthToken) auth).getToken();
        } else if (auth instanceof GiteaAuthUser) {
            GiteaAuthUser user = (GiteaAuthUser) auth;
            this.authType = AUTH_BASIC;
            this.authSecret = user.getUsername() + ":" + user.getPassword();
        } else {
            this.authType = AUTH_NONE;
            this.authSecret = "";
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    private <T> T parseObject(String json, Class<T> type) throws IOException {
        return MAPPER.readerFor(type).readValue(json);
    }

    private <T> List<T> parseList(String json, Class<T> elementType) throws IOException {
        return MAPPER.readerForListOf(elementType).readValue(json);
    }

    private String toJson(Object value) throws IOException {
        return MAPPER.writeValueAsString(value);
    }

    private String singleStateKey(Set<GiteaIssueState> states) {
        if (states == null || states.size() != 1) {
            return null;
        }
        for (GiteaIssueState s : GiteaIssueState.values()) {
            if (states.contains(s)) {
                return s.getKey();
            }
        }
        return null;
    }

    private String ownerUsername(GiteaOwner owner) {
        return owner.getUsername();
    }

    private String repoOwnerUsername(GiteaRepository repository) {
        return repository.getOwner().getUsername();
    }

    // ------------------------------------------------------------------
    // Version + users + owners
    // ------------------------------------------------------------------

    @Override
    public GiteaVersion fetchVersion() throws IOException, InterruptedException {
        return parseObject(nativeFetchVersion(serverUrl, authType, authSecret), GiteaVersion.class);
    }

    @Override
    public GiteaUser fetchCurrentUser() throws IOException, InterruptedException {
        return parseObject(nativeFetchCurrentUser(serverUrl, authType, authSecret), GiteaUser.class);
    }

    @Override
    public GiteaOwner fetchOwner(String name) throws IOException, InterruptedException {
        return parseObject(nativeFetchOwner(serverUrl, authType, authSecret, name), GiteaOwner.class);
    }

    @Override
    public GiteaUser fetchUser(String name) throws IOException, InterruptedException {
        return parseObject(nativeFetchUser(serverUrl, authType, authSecret, name), GiteaUser.class);
    }

    @Override
    public GiteaOrganization fetchOrganization(String name) throws IOException, InterruptedException {
        return parseObject(nativeFetchOrganization(serverUrl, authType, authSecret, name), GiteaOrganization.class);
    }

    @Override
    public GiteaRepository fetchRepository(String username, String name) throws IOException, InterruptedException {
        return parseObject(
                nativeFetchRepository(serverUrl, authType, authSecret, username, name),
                GiteaRepository.class);
    }

    @Override
    public GiteaRepository fetchRepository(GiteaOwner owner, String name) throws IOException, InterruptedException {
        return fetchRepository(ownerUsername(owner), name);
    }

    @Override
    public List<GiteaRepository> fetchCurrentUserRepositories() throws IOException, InterruptedException {
        return parseList(
                nativeFetchCurrentUserRepositories(serverUrl, authType, authSecret),
                GiteaRepository.class);
    }

    @Override
    public List<GiteaRepository> fetchRepositories(String username) throws IOException, InterruptedException {
        return parseList(
                nativeFetchRepositories(serverUrl, authType, authSecret, username),
                GiteaRepository.class);
    }

    @Override
    public List<GiteaRepository> fetchRepositories(GiteaOwner owner) throws IOException, InterruptedException {
        return fetchRepositories(ownerUsername(owner));
    }

    @Override
    public List<GiteaRepository> fetchOrganizationRepositories(GiteaOwner owner) throws IOException, InterruptedException {
        return parseList(
                nativeFetchOrganizationRepositories(serverUrl, authType, authSecret, ownerUsername(owner)),
                GiteaRepository.class);
    }

    // ------------------------------------------------------------------
    // Branches
    // ------------------------------------------------------------------

    @Override
    public GiteaBranch fetchBranch(String username, String repository, String name)
            throws IOException, InterruptedException {
        return parseObject(
                nativeFetchBranch(serverUrl, authType, authSecret, username, repository, name),
                GiteaBranch.class);
    }

    @Override
    public GiteaBranch fetchBranch(GiteaRepository repository, String name) throws IOException, InterruptedException {
        return fetchBranch(repoOwnerUsername(repository), repository.getName(), name);
    }

    @Override
    public List<GiteaBranch> fetchBranches(String username, String name) throws IOException, InterruptedException {
        return parseList(
                nativeFetchBranches(serverUrl, authType, authSecret, username, name),
                GiteaBranch.class);
    }

    @Override
    public List<GiteaBranch> fetchBranches(GiteaRepository repository) throws IOException, InterruptedException {
        return fetchBranches(repoOwnerUsername(repository), repository.getName());
    }

    // ------------------------------------------------------------------
    // Tags
    // ------------------------------------------------------------------

    @Override
    public GiteaAnnotatedTag fetchAnnotatedTag(String username, String repository, String sha1)
            throws IOException, InterruptedException {
        return parseObject(
                nativeFetchAnnotatedTag(serverUrl, authType, authSecret, username, repository, sha1),
                GiteaAnnotatedTag.class);
    }

    @Override
    public GiteaAnnotatedTag fetchAnnotatedTag(GiteaRepository repository, GiteaTag tag)
            throws IOException, InterruptedException {
        return fetchAnnotatedTag(repoOwnerUsername(repository), repository.getName(), tag.getId());
    }

    @Override
    public GiteaTag fetchTag(String username, String repository, String tag) throws IOException, InterruptedException {
        return parseObject(
                nativeFetchTag(serverUrl, authType, authSecret, username, repository, tag),
                GiteaTag.class);
    }

    @Override
    public GiteaTag fetchTag(GiteaRepository repository, String tag) throws IOException, InterruptedException {
        return fetchTag(repoOwnerUsername(repository), repository.getName(), tag);
    }

    @Override
    public List<GiteaTag> fetchTags(String username, String name) throws IOException, InterruptedException {
        return parseList(
                nativeFetchTags(serverUrl, authType, authSecret, username, name),
                GiteaTag.class);
    }

    @Override
    public List<GiteaTag> fetchTags(GiteaRepository repository) throws IOException, InterruptedException {
        return fetchTags(repoOwnerUsername(repository), repository.getName());
    }

    // ------------------------------------------------------------------
    // Commits + collaborators
    // ------------------------------------------------------------------

    @Override
    public GiteaCommitDetail fetchCommit(String username, String repository, String sha1)
            throws IOException, InterruptedException {
        return parseObject(
                nativeFetchCommit(serverUrl, authType, authSecret, username, repository, sha1),
                GiteaCommitDetail.class);
    }

    @Override
    public GiteaCommitDetail fetchCommit(GiteaRepository repository, String sha1)
            throws IOException, InterruptedException {
        return fetchCommit(repoOwnerUsername(repository), repository.getName(), sha1);
    }

    @Override
    public List<GiteaUser> fetchCollaborators(String username, String name) throws IOException, InterruptedException {
        return parseList(
                nativeFetchCollaborators(serverUrl, authType, authSecret, username, name),
                GiteaUser.class);
    }

    @Override
    public List<GiteaUser> fetchCollaborators(GiteaRepository repository) throws IOException, InterruptedException {
        return fetchCollaborators(repoOwnerUsername(repository), repository.getName());
    }

    @Override
    public boolean checkCollaborator(String username, String name, String collaboratorName)
            throws IOException, InterruptedException {
        return nativeCheckCollaborator(serverUrl, authType, authSecret, username, name, collaboratorName);
    }

    @Override
    public boolean checkCollaborator(GiteaRepository repository, String collaboratorName)
            throws IOException, InterruptedException {
        return checkCollaborator(repoOwnerUsername(repository), repository.getName(), collaboratorName);
    }

    // ------------------------------------------------------------------
    // Organization hooks
    // ------------------------------------------------------------------

    @Override
    public List<GiteaHook> fetchHooks(String organizationName) throws IOException, InterruptedException {
        return parseList(
                nativeFetchHooksOrg(serverUrl, authType, authSecret, organizationName),
                GiteaHook.class);
    }

    @Override
    public List<GiteaHook> fetchHooks(GiteaOrganization organization) throws IOException, InterruptedException {
        return fetchHooks(ownerUsername(organization));
    }

    @Override
    public GiteaHook createHook(GiteaOrganization organization, GiteaHook hook) throws IOException, InterruptedException {
        return parseObject(
                nativeCreateHookOrg(serverUrl, authType, authSecret, ownerUsername(organization), toJson(hook)),
                GiteaHook.class);
    }

    @Override
    public void deleteHook(GiteaOrganization organization, GiteaHook hook) throws IOException, InterruptedException {
        deleteHook(organization, hook.getId());
    }

    @Override
    public void deleteHook(GiteaOrganization organization, long id) throws IOException, InterruptedException {
        nativeDeleteHookOrg(serverUrl, authType, authSecret, ownerUsername(organization), id);
    }

    @Override
    public void updateHook(GiteaOrganization organization, GiteaHook hook) throws IOException, InterruptedException {
        nativeUpdateHookOrg(
                serverUrl, authType, authSecret, ownerUsername(organization), hook.getId(), toJson(hook));
    }

    // ------------------------------------------------------------------
    // Repository hooks
    // ------------------------------------------------------------------

    @Override
    public List<GiteaHook> fetchHooks(String username, String name) throws IOException, InterruptedException {
        return parseList(
                nativeFetchHooksRepo(serverUrl, authType, authSecret, username, name),
                GiteaHook.class);
    }

    @Override
    public List<GiteaHook> fetchHooks(GiteaRepository repository) throws IOException, InterruptedException {
        return fetchHooks(repoOwnerUsername(repository), repository.getName());
    }

    @Override
    public GiteaHook createHook(GiteaRepository repository, GiteaHook hook) throws IOException, InterruptedException {
        return parseObject(
                nativeCreateHookRepo(
                        serverUrl, authType, authSecret, repoOwnerUsername(repository), repository.getName(), toJson(hook)),
                GiteaHook.class);
    }

    @Override
    public void deleteHook(GiteaRepository repository, GiteaHook hook) throws IOException, InterruptedException {
        deleteHook(repository, hook.getId());
    }

    @Override
    public void deleteHook(GiteaRepository repository, long id) throws IOException, InterruptedException {
        nativeDeleteHookRepo(serverUrl, authType, authSecret, repoOwnerUsername(repository), repository.getName(), id);
    }

    @Override
    public void updateHook(GiteaRepository repository, GiteaHook hook) throws IOException, InterruptedException {
        nativeUpdateHookRepo(
                serverUrl,
                authType,
                authSecret,
                repoOwnerUsername(repository),
                repository.getName(),
                hook.getId(),
                toJson(hook));
    }

    // ------------------------------------------------------------------
    // Commit statuses
    // ------------------------------------------------------------------

    @Override
    public List<GiteaCommitStatus> fetchCommitStatuses(GiteaRepository repository, String sha)
            throws IOException, InterruptedException {
        return parseList(
                nativeFetchCommitStatuses(
                        serverUrl, authType, authSecret, repoOwnerUsername(repository), repository.getName(), sha),
                GiteaCommitStatus.class);
    }

    @Override
    public GiteaCommitStatus createCommitStatus(String username, String repository, String sha, GiteaCommitStatus status)
            throws IOException, InterruptedException {
        return parseObject(
                nativeCreateCommitStatus(
                        serverUrl, authType, authSecret, username, repository, sha, toJson(status)),
                GiteaCommitStatus.class);
    }

    @Override
    public GiteaCommitStatus createCommitStatus(GiteaRepository repository, String sha, GiteaCommitStatus status)
            throws IOException, InterruptedException {
        return createCommitStatus(repoOwnerUsername(repository), repository.getName(), sha, status);
    }

    // ------------------------------------------------------------------
    // Pull requests
    // ------------------------------------------------------------------

    @Override
    public GiteaPullRequest fetchPullRequest(String username, String name, long id)
            throws IOException, InterruptedException {
        return parseObject(
                nativeFetchPullRequest(serverUrl, authType, authSecret, username, name, id),
                GiteaPullRequest.class);
    }

    @Override
    public GiteaPullRequest fetchPullRequest(GiteaRepository repository, long id)
            throws IOException, InterruptedException {
        return fetchPullRequest(repoOwnerUsername(repository), repository.getName(), id);
    }

    @Override
    public List<GiteaPullRequest> fetchPullRequests(String username, String name)
            throws IOException, InterruptedException {
        return fetchPullRequests(username, name, EnumSet.of(GiteaIssueState.OPEN));
    }

    @Override
    public List<GiteaPullRequest> fetchPullRequests(GiteaRepository repository)
            throws IOException, InterruptedException {
        return fetchPullRequests(repository, EnumSet.of(GiteaIssueState.OPEN));
    }

    @Override
    public List<GiteaPullRequest> fetchPullRequests(String username, String name, Set<GiteaIssueState> states)
            throws IOException, InterruptedException {
        return parseList(
                nativeFetchPullRequests(serverUrl, authType, authSecret, username, name, singleStateKey(states)),
                GiteaPullRequest.class);
    }

    @Override
    public List<GiteaPullRequest> fetchPullRequests(GiteaRepository repository, Set<GiteaIssueState> states)
            throws IOException, InterruptedException {
        return fetchPullRequests(repoOwnerUsername(repository), repository.getName(), states);
    }

    // ------------------------------------------------------------------
    // Issues
    // ------------------------------------------------------------------

    @Override
    public List<GiteaIssue> fetchIssues(String username, String name) throws IOException, InterruptedException {
        return fetchIssues(username, name, EnumSet.of(GiteaIssueState.OPEN));
    }

    @Override
    public List<GiteaIssue> fetchIssues(GiteaRepository repository) throws IOException, InterruptedException {
        return fetchIssues(repository, EnumSet.of(GiteaIssueState.OPEN));
    }

    @Override
    public List<GiteaIssue> fetchIssues(String username, String name, Set<GiteaIssueState> states)
            throws IOException, InterruptedException {
        return parseList(
                nativeFetchIssues(serverUrl, authType, authSecret, username, name, singleStateKey(states)),
                GiteaIssue.class);
    }

    @Override
    public List<GiteaIssue> fetchIssues(GiteaRepository repository, Set<GiteaIssueState> states)
            throws IOException, InterruptedException {
        return fetchIssues(repoOwnerUsername(repository), repository.getName(), states);
    }

    // ------------------------------------------------------------------
    // Files
    // ------------------------------------------------------------------

    @Override
    public byte[] fetchFile(GiteaRepository repository, String ref, String path)
            throws IOException, InterruptedException {
        return nativeFetchFile(
                serverUrl, authType, authSecret, repoOwnerUsername(repository), repository.getName(), ref, path);
    }

    @Override
    public boolean checkFile(GiteaRepository repository, String ref, String path)
            throws IOException, InterruptedException {
        return nativeCheckFile(
                serverUrl, authType, authSecret, repoOwnerUsername(repository), repository.getName(), ref, path);
    }

    // ------------------------------------------------------------------
    // Releases
    // ------------------------------------------------------------------

    @Override
    public List<GiteaRelease> fetchReleases(String username, String name, boolean draft, boolean prerelease)
            throws IOException, InterruptedException {
        return parseList(
                nativeFetchReleases(
                        serverUrl, authType, authSecret, username, name, draft, prerelease),
                GiteaRelease.class);
    }

    @Override
    public List<GiteaRelease> fetchReleases(GiteaRepository repository, boolean draft, boolean prerelease)
            throws IOException, InterruptedException {
        return fetchReleases(repoOwnerUsername(repository), repository.getName(), draft, prerelease);
    }

    @Override
    public GiteaRelease.Attachment createReleaseAttachment(
            String username, String repository, long id, String name, InputStream file)
            throws IOException, InterruptedException {
        byte[] data = readAll(file);
        return parseObject(
                nativeCreateReleaseAttachment(
                        serverUrl, authType, authSecret, username, repository, id, name, data),
                GiteaRelease.Attachment.class);
    }

    @Override
    public GiteaRelease.Attachment createReleaseAttachment(
            GiteaRepository repository, long id, String name, InputStream file)
            throws IOException, InterruptedException {
        return createReleaseAttachment(repoOwnerUsername(repository), repository.getName(), id, name, file);
    }

    private static byte[] readAll(InputStream in) throws IOException {
        java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
        byte[] buf = new byte[8192];
        int n;
        while ((n = in.read(buf)) > 0) {
            out.write(buf, 0, n);
        }
        return out.toByteArray();
    }

    // ------------------------------------------------------------------
    // Closeable
    // ------------------------------------------------------------------

    @Override
    public void close() throws IOException {
        // No per-connection resources: the Tokio runtime is process-global
        // and the underlying reqwest Client pool is owned by the Lazy static.
    }

    // ------------------------------------------------------------------
    // Native method declarations — one per JNI export in rust/gitea-client/src/jni.rs
    // ------------------------------------------------------------------

    private static native String nativeFetchVersion(String serverUrl, int authType, String authSecret);

    private static native String nativeFetchCurrentUser(String serverUrl, int authType, String authSecret);

    private static native String nativeFetchUser(String serverUrl, int authType, String authSecret, String name);

    private static native String nativeFetchOrganization(String serverUrl, int authType, String authSecret, String name);

    private static native String nativeFetchOwner(String serverUrl, int authType, String authSecret, String name);

    private static native String nativeFetchRepository(
            String serverUrl, int authType, String authSecret, String owner, String repo);

    private static native String nativeFetchCurrentUserRepositories(
            String serverUrl, int authType, String authSecret);

    private static native String nativeFetchRepositories(
            String serverUrl, int authType, String authSecret, String user);

    private static native String nativeFetchOrganizationRepositories(
            String serverUrl, int authType, String authSecret, String org);

    private static native String nativeFetchBranch(
            String serverUrl, int authType, String authSecret, String owner, String repo, String name);

    private static native String nativeFetchBranches(
            String serverUrl, int authType, String authSecret, String owner, String repo);

    private static native String nativeFetchAnnotatedTag(
            String serverUrl, int authType, String authSecret, String owner, String repo, String sha);

    private static native String nativeFetchTag(
            String serverUrl, int authType, String authSecret, String owner, String repo, String tag);

    private static native String nativeFetchTags(
            String serverUrl, int authType, String authSecret, String owner, String repo);

    private static native String nativeFetchCommit(
            String serverUrl, int authType, String authSecret, String owner, String repo, String sha);

    private static native String nativeFetchCollaborators(
            String serverUrl, int authType, String authSecret, String owner, String repo);

    private static native boolean nativeCheckCollaborator(
            String serverUrl, int authType, String authSecret, String owner, String repo, String collaborator);

    private static native String nativeFetchHooksOrg(
            String serverUrl, int authType, String authSecret, String org);

    private static native String nativeCreateHookOrg(
            String serverUrl, int authType, String authSecret, String org, String body);

    private static native void nativeDeleteHookOrg(
            String serverUrl, int authType, String authSecret, String org, long id);

    private static native String nativeUpdateHookOrg(
            String serverUrl, int authType, String authSecret, String org, long id, String body);

    private static native String nativeFetchHooksRepo(
            String serverUrl, int authType, String authSecret, String owner, String repo);

    private static native String nativeCreateHookRepo(
            String serverUrl, int authType, String authSecret, String owner, String repo, String body);

    private static native void nativeDeleteHookRepo(
            String serverUrl, int authType, String authSecret, String owner, String repo, long id);

    private static native String nativeUpdateHookRepo(
            String serverUrl, int authType, String authSecret, String owner, String repo, long id, String body);

    private static native String nativeFetchCommitStatuses(
            String serverUrl, int authType, String authSecret, String owner, String repo, String sha);

    private static native String nativeCreateCommitStatus(
            String serverUrl, int authType, String authSecret, String owner, String repo, String sha, String body);

    private static native String nativeFetchPullRequest(
            String serverUrl, int authType, String authSecret, String owner, String repo, long id);

    private static native String nativeFetchPullRequests(
            String serverUrl, int authType, String authSecret, String owner, String repo, String state);

    private static native String nativeFetchIssues(
            String serverUrl, int authType, String authSecret, String owner, String repo, String state);

    private static native byte[] nativeFetchFile(
            String serverUrl, int authType, String authSecret, String owner, String repo, String ref, String path);

    private static native boolean nativeCheckFile(
            String serverUrl, int authType, String authSecret, String owner, String repo, String ref, String path);

    private static native String nativeFetchReleases(
            String serverUrl, int authType, String authSecret, String owner, String repo,
            boolean draft, boolean prerelease);

    private static native String nativeCreateReleaseAttachment(
            String serverUrl, int authType, String authSecret, String owner, String repo,
            long releaseId, String name, byte[] data);

    /**
     * Install additional CA certificates (PEM-encoded) into the native Rust
     * HTTP client's trust store. Called once during plugin initialisation
     * when the operator has populated the "Trusted certificates (PEM)"
     * field in {@link org.jenkinsci.plugin.gitea.servers.GiteaServers}.
     *
     * <p>After this call, every {@code RustGiteaConnection} constructed in
     * this JVM trusts both the Mozilla CA bundle (always) and the supplied
     * PEM (on top). Passing {@code null} or an empty array clears any
     * previously-installed extra trust material.</p>
     *
     * <p><strong>Hot-reload caveat:</strong> the native side stores the PEM
     * in a write-once {@code OnceCell}, so subsequent calls are silently
     * ignored — changing the PEM and saving Jenkins' global config takes
     * effect only after a controller restart. This matches the existing
     * Tokio-runtime limitation documented in {@code AGENTS.md}.</p>
     *
     * @param pem raw PEM bytes, e.g. {@code "-----BEGIN CERTIFICATE-----\n..."}
     *            potentially containing multiple certificates; or {@code null}
     *            to clear the slot.
     */
    public static native void nativeSetTrustedCertificates(byte[] pem);

    /**
     * Configure the outbound HTTP proxy for all subsequent Gitea API
     * requests made by the native Rust client.
     *
     * <p>The argument is a JSON document with the shape</p>
     * <pre>{@code
     * {
     *   "url": "http://proxy.corp:3128",   // or https://... or socks5://...
     *   "username": "",                    // optional Basic-auth user
     *   "password": "",                    // optional Basic-auth pass
     *   "noProxyHosts": "localhost,127.0.0.1,.internal.corp.com"
     * }
     * }</pre>
     *
     * <p>Passing {@code null}, an empty string, an invalid JSON document,
     * or a config with an empty {@code url} clears the explicit-proxy slot
     * and the native client falls back to the {@code HTTP_PROXY} /
     * {@code HTTPS_PROXY} / {@code NO_PROXY} environment variables (the
     * default {@code reqwest} behaviour).</p>
     *
     * <p><strong>Hot-reload caveat:</strong> like
     * {@link #nativeSetTrustedCertificates(byte[])}, the native side stores
     * the config in a write-once slot, so subsequent calls are silently
     * ignored — changing the proxy and saving Jenkins' global config takes
     * effect only after a controller restart. This matches the existing
     * Tokio-runtime limitation documented in {@code AGENTS.md}.</p>
     *
     * @param configJson the JSON document described above, or {@code null}
     *                   to disable the explicit proxy.
     */
    public static native void nativeSetProxy(String configJson);

    /**
     * Start (or restart) the adaptive polling scheduler — stage 10.
     *
     * <p>The argument is a JSON document of the shape</p>
     * <pre>{@code
     * {
     *   "intervalSeconds": 300,
     *   "targets": [
     *     {
     *       "serverUrl":  "https://gitea.corp",
     *       "authType":   1,                 // 0=None, 1=Token, 2=Basic
     *       "authSecret": "d4f5...",         // token | "user:pass" | ""
     *       "owner":      "acme",
     *       "repo":       "widget"
     *     }
     *   ]
     * }
     * }</pre>
     *
     * <p>If a polling loop is already running, it is aborted first so the
     * new configuration takes effect immediately. Passing an empty
     * {@code intervalSeconds} or an empty {@code targets} list (or any
     * malformed JSON) is equivalent to calling
     * {@link #nativeStopPolling()}.</p>
     *
     * <p>Like the other native configuration setters, this method is
     * infallible at the JNI boundary — failures are logged on the Rust
     * side and silently ignored so that saving Jenkins' global config
     * never fails. Subject to the same hot-reload caveat as the rest of
     * the native layer (see {@code AGENTS.md}).</p>
     *
     * @param configJson the JSON document described above.
     */
    public static native void nativeStartPolling(String configJson);

    /**
     * Stop the adaptive polling scheduler if one is running. Idempotent.
     */
    public static native void nativeStopPolling();
}
