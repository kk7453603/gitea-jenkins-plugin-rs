# Example: Webhook Audit Sink → SIEM

**Use case:** Forward every webhook event to a corporate SIEM (Splunk, ELK, Datadog) as structured JSON for compliance and security monitoring.

**Time:** 1-2 hours.

**Approach:** Java-side `java.util.logging.Handler` attached to the `RustWebhookDispatcher` logger. No Rust changes needed.

---

## Why Java-side (not Rust)

- SIEM ingestion is a Java/Jenkins concern (credential management, retry logic, network configuration)
- Rust already forwards all events to JUL via `log_bridge.rs`
- Adding SIEM HTTP calls in Rust would duplicate Java's logging infrastructure
- Easier to test and configure from Jenkins UI

---

## Implementation

### 1. Create audit Handler class

New file `src/main/java/.../webhook/SiemAuditHandler.java`:

```java
package org.jenkinsci.plugin.gitea.webhook;

import hudson.Extension;
import hudson.util.FormValidation;
import java.io.IOException;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import jenkins.model.Jenkins;
import org.kohsuke.accmod.Restricted;
import org.kohsuke.accmod.restrictions.NoExternalUse;

/**
 * Forwards webhook events from the {@code RustWebhookDispatcher} logger to a
 * corporate SIEM HTTP endpoint as newline-delimited JSON.
 *
 * <p>Registered as a Jenkins {@link Extension} so it auto-installs on boot.
 * Reads its configuration from {@link SiemAuditConfig} (global config).</p>
 */
@Extension
public class SiemAuditHandler extends Handler {

    public SiemAuditHandler() {
        setLevel(Level.INFO);  // only INFO+ from Rust (DEBUG/TRACE filtered)
        setFilter(record -> {
            // Only forward Rust webhook dispatcher logs, not all gitea.* logs
            return record.getLoggerName().contains("RustWebhookDispatcher")
                || record.getLoggerName().contains("gitea_client.server");
        });
    }

    @Override
    public void publish(LogRecord record) {
        SiemAuditConfig config = SiemAuditConfig.get();
        if (config == null || config.getEndpointUrl() == null || config.getEndpointUrl().isEmpty()) {
            return;  // SIEM not configured
        }

        String json = formatAsJson(record);
        sendToSiem(config, json);
    }

    private String formatAsJson(LogRecord record) {
        long ts = record.getMillis();
        String level = record.getLevel().getName();
        String logger = escape(record.getLoggerName());
        String msg = escape(record.getMessage());
        return String.format(
            "{\"timestamp\":%d,\"level\":\"%s\",\"logger\":\"%s\",\"message\":\"%s\"}%n",
            ts, level, logger, msg
        );
    }

    private String escape(String s) {
        if (s == null) return "";
        return s.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n");
    }

    private void sendToSiem(SiemAuditConfig config, String json) {
        try {
            URL url = new URL(config.getEndpointUrl());
            HttpURLConnection conn = (HttpURLConnection) url.openConnection();
            conn.setRequestMethod("POST");
            conn.setRequestProperty("Content-Type", "application/json");
            if (config.getAuthToken() != null && !config.getAuthToken().isEmpty()) {
                conn.setRequestProperty("Authorization", "Bearer " + config.getAuthToken());
            }
            conn.setDoOutput(true);
            conn.setConnectTimeout(5000);
            conn.setReadTimeout(5000);
            try (OutputStream os = conn.getOutputStream()) {
                os.write(json.getBytes(StandardCharsets.UTF_8));
            }
            int code = conn.getResponseCode();
            if (code >= 400) {
                // Don't use logger here — would recurse. Use System.err.
                System.err.println("SiemAuditHandler: SIEM returned " + code);
            }
            conn.disconnect();
        } catch (IOException e) {
            System.err.println("SiemAuditHandler: failed to send to SIEM: " + e.getMessage());
        }
    }

    @Override
    public void flush() {}

    @Override
    public void close() throws SecurityException {}
}
```

### 2. Create config class

New file `src/main/java/.../webhook/SiemAuditConfig.java`:

```java
package org.jenkinsci.plugin.gitea.webhook;

import hudson.Extension;
import java.util.logging.Logger;
import jenkins.model.GlobalConfiguration;
import net.sf.json.JSONObject;
import org.kohsuke.accmod.Restricted;
import org.kohsuke.accmod.restrictions.NoExternalUse;
import org.kohsuke.stapler.StaplerRequest2;

/**
 * Global config for SIEM audit sink. Fields are configurable in
 * {@code Manage Jenkins → System → Gitea Servers → SIEM Audit}.
 */
@Extension
public class SiemAuditConfig extends GlobalConfiguration {

    private static final Logger LOGGER = Logger.getLogger(SiemAuditConfig.class.getName());

    /** HTTP endpoint of the SIEM ingestion API. */
    private String endpointUrl = "";

    /** Bearer token for SIEM auth. Plaintext in config.xml. */
    private String authToken = "";

    public SiemAuditConfig() {
        load();
    }

    public static SiemAuditConfig get() {
        return GlobalConfiguration.all().get(SiemAuditConfig.class);
    }

    @Restricted(NoExternalUse.class)
    public String getEndpointUrl() {
        return endpointUrl == null ? "" : endpointUrl;
    }

    @Restricted(NoExternalUse.class)
    public void setEndpointUrl(String url) {
        this.endpointUrl = url == null ? "" : url;
    }

    @Restricted(NoExternalUse.class)
    public String getAuthToken() {
        return authToken == null ? "" : authToken;
    }

    @Restricted(NoExternalUse.class)
    public void setAuthToken(String token) {
        this.authToken = token == null ? "" : token;
    }

    @Override
    public boolean configure(StaplerRequest2 req, JSONObject json) throws FormException {
        req.bindJSON(this, json);
        save();
        return true;
    }
}
```

### 3. UI

New file `src/main/resources/.../webhook/SiemAuditConfig/config.jelly`:

```xml
<?jelly escape-by-default='true'?>
<j:jelly xmlns:j="jelly:core" xmlns:f="/lib/form">
    <f:section title="${%SIEM Audit}">
        <f:entry title="${%SIEM endpoint URL}" field="endpointUrl">
            <f:textbox placeholder="https://siem.corp/api/v1/ingest"/>
        </f:entry>
        <f:entry title="${%Auth token (Bearer)}" field="authToken">
            <f:password/>
        </f:entry>
    </f:section>
</j:jelly>
```

### 4. Attach Handler to logger

Modify `RustWebhookDispatcher.<clinit>` to attach the handler:

```java
static {
    try {
        NativeLibraryLoader.load("gitea_rust");
        nativeRegisterDispatcherClass(RustWebhookDispatcher.class);
        nativeInstallLogBridge();

        // Attach SIEM handler to capture Rust webhook events
        Logger logger = Logger.getLogger("org.jenkinsci.plugin.gitea");
        try {
            SiemAuditHandler handler = new SiemAuditHandler();
            logger.addHandler(handler);
        } catch (Throwable t) {
            LOGGER.log(Level.WARNING, "Failed to attach SiemAuditHandler", t);
        }
    } catch (UnsatisfiedLinkError e) {
        LOGGER.log(Level.SEVERE, "Failed to load libgitea_rust", e);
        throw e;
    }
}
```

### 5. Test

Add unit test for `SiemAuditHandler`:

```java
@Test
public void formatAsJson_escapesQuotes() throws Exception {
    SiemAuditHandler handler = new SiemAuditHandler();
    // Use reflection or extract formatAsJson to test
    // (production code keeps it private — refactor to package-private for tests)
}
```

Manual integration test:
1. Configure SIEM endpoint (use `https://httpbin.org/post` for testing)
2. Send a webhook via `tools/smoke-test.sh`
3. Check SIEM endpoint receives the JSON

---

## Production hardening

For real corporate use:

1. **Async sending:** the example blocks the calling thread. Use `ExecutorService` with a bounded queue:
   ```java
   private static final ExecutorService SIEM_EXECUTOR =
       Executors.newSingleThreadExecutor(r -> {
           Thread t = new Thread(r, "siem-audit-sender");
           t.setDaemon(true);
           return t;
       });
   ```

2. **Batching:** accumulate records, flush every N seconds or M records.

3. **Retry with exponential backoff:** on network failure, retry up to 3 times.

4. **Drop on overflow:** if queue is full, drop oldest records (don't block the JVM).

5. **Mutual TLS:** if SIEM requires client cert, configure `HttpsURLConnection.setDefaultSSLSocketFactory`.

6. **Local spool:** if SIEM is unreachable, write to local file `$JENKINS_HOME/logs/siem-spool.jsonl`, replay later.

---

## Alternative: use Jenkins Logstash plugin

If you don't want custom code, install the [Logstash plugin](https://plugins.jenkins.io/logstash/) and configure it to forward `org.jenkinsci.plugin.gitea.*` logs to your SIEM. No code changes needed.

**Trade-off:** less control over format, depends on Logstash plugin availability in corp Jenkins.

---

## What this audit captures

| Event | Captured? |
|---|---|
| Webhook received (push/pull_request/...) | ✅ |
| Webhook rejected (401/403/429) | ✅ (via `gitea_client.server` logger) |
| Webhook dispatched to SCMEvent | ✅ |
| Outbound Gitea API call | ✅ (if Rust logs at INFO) |
| Plugin load/unload | ✅ |
| Build trigger | ❌ (different logger — `org.jenkinsci.plugins.workflow`) |

For build trigger audit, add a separate Jenkins `RunListener` — outside the scope of this plugin.
