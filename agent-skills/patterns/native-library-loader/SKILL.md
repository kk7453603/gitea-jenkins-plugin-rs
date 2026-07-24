---
name: native-library-loader
description: Cross-platform извлечение и загрузка native library (.so/.dylib/.dll) из classpath JAR/HPI без java.library.path. Применять когда есть UnsatisfiedLinkError, нужна multi-arch поддержка, или когда нативная библиотека должна загружаться автоматически из bundled ресурсов.
origin: gitea-jenkins-plugin-rs v1.1.0
tags: [java, native, jni, library-loader, multi-arch, classpath, unsatisfied-link-error]
---

# NativeLibraryLoader: загрузка .so из JAR без java.library.path

## Когда применять

- `UnsatisfiedLinkError` при `System.loadLibrary("gitea_rust")` — нужно загружать из bundled ресурсов
- Контейнер/CI где нельзя прокинуть `-Djava.library.path=...`
- Multi-arch поддержка (amd64 + arm64 в одном артефакте)
- Несколько классов вызывают `Loader.load()` — нужна idempotent-защита
- Jenkins-плагин, Java agent, containerized app

## Паттерн

`System.loadLibrary()` требует `java.library.path` указывать на `.so`, что не работает в Jenkins (контроллер запускается без нашего контроля над JVM args). Решение — извлечь `.so` из classpath ресурсов во временную папку и вызвать `System.load()` по абсолютному пути.

### Структура ресурсов

```
src/main/resources/
└── META-INF/
    └── native/
        ├── linux/
        │   ├── amd64/libgitea_rust.so
        │   └── aarch64/libgitea_rust.so
        ├── darwin/
        │   └── aarch64/libgitea_rust.dylib  (опционально, для dev на macOS)
        └── windows/
            └── amd64/gitea_rust.dll  (опционально)
```

### Каркас класса

```java
public final class NativeLibraryLoader {

    // Idempotency: второй вызов с тем же libName — no-op.
    // ConcurrentHashMap-backed Set потому что <clinit> нескольких классов
    // могут звать load() конкурентно (RustGiteaConnection + RustWebhookDispatcher).
    private static final Set<String> LOADED =
            Collections.newSetFromMap(new ConcurrentHashMap<String, Boolean>());

    @SuppressFBWarnings(value = "RV_RETURN_VALUE_IGNORED_BAD_PRACTICE",
            justification = "deleteOnExit return value is intentionally ignored.")
    public static void load(String libName) {
        // 1. Idempotency check — выходим сразу если уже загружено.
        if (!LOADED.add(libName)) {
            return;
        }

        // 2. mapLibraryName даёт платформенный формат: "gitea_rust" →
        //    "libgitea_rust.so" на Linux, "libgitea_rust.dylib" на macOS,
        //    "gitea_rust.dll" на Windows.
        String mappedName = System.mapLibraryName(libName);

        String osTag = osTag();
        String[] archCandidates = archCandidates();
        UnsatisfiedLinkError lastError = null;

        // 3. Пробуем каждую архитектуру по списку — fallback chain.
        for (String arch : archCandidates) {
            String resourcePath = "/META-INF/native/" + osTag + "/" + arch + "/" + mappedName;
            try (InputStream in = NativeLibraryLoader.class.getResourceAsStream(resourcePath)) {
                if (in == null) {
                    continue;  // ресурс для этой arch не bundled — пробуем следующую
                }
                // 4. Копируем во временный файл. Суффикс критичен —
                //    Linux требует .so, macOS — .dylib, иначе dlopen падает.
                String suffix = mappedName.endsWith(".dylib") ? ".dylib"
                        : mappedName.endsWith(".dll") ? ".dll" : ".so";
                Path tmp = Files.createTempFile("gitea-rust-", suffix);
                tmp.toFile().deleteOnExit();
                Files.copy(in, tmp, StandardCopyOption.REPLACE_EXISTING);
                // 5. Загружаем по абсолютному пути.
                System.load(tmp.toString());
                return;
            } catch (UnsatisfiedLinkError e) {
                // wrong ELF class (32 vs 64) или не та arch — пробуем следующую
                lastError = e;
                LOADED.remove(libName);  // позволяем повторную попытку
            } catch (IOException e) {
                LOADED.remove(libName);
                throw new ExceptionInInitializerError(e);
            }
        }

        if (lastError != null) {
            throw lastError;
        }
        // 6. Никакого подходящего ресурса не нашлось — кидаем понятную ошибку.
        StringBuilder tried = new StringBuilder();
        for (String arch : archCandidates) {
            tried.append(" /META-INF/native/").append(osTag).append('/').append(arch).append('/').append(mappedName);
        }
        throw new UnsatisfiedLinkError(
                "Missing native library for os=" + osTag
                        + " arch=" + String.join("/", archCandidates) + ". Tried:" + tried);
    }

    private static String osTag() {
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        if (os.contains("linux")) return "linux";
        if (os.contains("mac") || os.contains("darwin")) return "darwin";
        if (os.contains("windows")) return "windows";
        return os;  // fall through — resource lookup даст понятную ошибку
    }

    // Возвращает массив — fallback chain. На aarch64 пробуем нативную arch
    // первой, затем amd64 (работает через Rosetta на Apple Silicon).
    private static String[] archCandidates() {
        String arch = System.getProperty("os.arch", "").toLowerCase(Locale.ROOT);
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
        return new String[]{arch};  // unknown — вернём как есть для понятной ошибки
    }

    private NativeLibraryLoader() {
    }
}
```

### Использование — статический инициализатор

Два класса независимо вызывают `load()` в своих `<clinit>`:

```java
// RustGiteaConnection.java
public class RustGiteaConnection implements GiteaConnection {
    static {
        NativeLibraryLoader.load("gitea_rust");
    }
    // ...
}

// RustWebhookDispatcher.java
@Extension
public class RustWebhookDispatcher {
    static {
        try {
            NativeLibraryLoader.load("gitea_rust");  // ← второй вызов — no-op
            nativeRegisterDispatcherClass(RustWebhookDispatcher.class);
            nativeInstallLogBridge();
        } catch (UnsatisfiedLinkError e) {
            LOGGER.log(Level.SEVERE, "Failed to load libgitea_rust", e);
            throw e;
        }
    }
    // ...
}
```

`LOADED.add(libName)` возвращает `false` при повторном вызове — второй `<clinit>` выходит немедленно.

## Подводные камни

1. **`System.loadLibrary()` НЕ работает.** Она ищет по `java.library.path`, который мы не контролируем в Jenkins. Только `System.load(absolutePath)`.
2. **`mapLibraryName` обязательно.** Не хардкодьте `.so`/`.dylib` — на Windows это `.dll`, на macOS — `.dylib`, и `mapLibraryName` знает правило.
3. **Суффикс временного файла.** `Files.createTempFile("gitea-rust-", ".so")` — Linux `dlopen` требует расширение `.so`. На macOS — `.dylib`. На Windows — `.dll`. Если суффикс не тот, `System.load` падает с невнятной ошибкой.
4. **`deleteOnExit()` не работает в Jenkins.** Jenkins не shutdown-ится нормально, файлы копятся в `/tmp`. Это известная утечка, но альтернатива (load из bundled location в `JENKINS_HOME`) ломает hot-reload. Принимаем как known limitation.
5. **`LOADED.remove(libName)` при UnsatisfiedLinkError.** Если попытка загрузить wrong-arch упала, мы должны разрешить повторную попытку с другим arch. Иначе fallback chain не сработает.
6. **Не используйте `File.createTempFile`.** Используйте `Files.createTempFile` (NIO) — лучше обработка ошибок и явные permissions.
7. **`ConcurrentHashMap`-backed Set.** `Collections.newSetFromMap(new ConcurrentHashMap<>())` — потокобезопасный Set. `<clinit>` разных классов может вызываться конкурентно из разных потоков ClassLoader-а.
8. **Rosetta на aarch64.** На Apple Silicon JVM репортит `os.arch=aarch64`, но amd64-binary работает через Rosetta 2. Поэтому fallback chain для aarch64 — `["aarch64", "amd64"]`. На Linux ARM (без Rosetta) amd64-binary не загрузится — `UnsatisfiedLinkError` с понятным сообщением.
9. **Multi-arch .hpi.** Для multi-arch поддержки (amd64 + aarch64 в одном артефакте) нужно вручную инъектировать `.so` для обеих arch через `jar uf` (см. `docker-rust-jenkins-multi-stage/SKILL.md`). `maven-resources-plugin` по умолчанию копирует только host-arch.
10. **`@SuppressFBWarnings("RV_RETURN_VALUE_IGNORED_BAD_PRACTICE")`.** SpotBugs ругается, что мы игнорируем return `deleteOnExit()`. Justification: return — это `boolean`, означающий "file was registered", и мы сознательно его игнорируем.
11. **FindClass из native threads.** После того как `.so` загружен, JNI-символы доступны. Но если Rust-код хочет колбэкнуть в Java из tokio worker thread, `find_class` упадёт (system ClassLoader не видит plugin classes). См. `webhook-jni-callback-server/SKILL.md` — там через `GlobalRef`.
12. **`tmpfs` с `noexec`.** Если `/tmp` смонтирован с `noexec` (некоторые hardened-дистрибутивы), `System.load()` падает. Решение — указать другой temp-каталог через `-Djava.io.tmpdir=...`.

## Файлы-референсы

- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/client/impl/NativeLibraryLoader.java` — полный класс с os/arch detection и fallback chain
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/client/impl/RustGiteaConnection.java` — `static { NativeLibraryLoader.load("gitea_rust"); }`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/webhook/RustWebhookDispatcher.java` — второй `<clinit>` с `load()` + register-dispatcher
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/docker/Dockerfile` — multi-arch инъекция `.so` в `META-INF/native/linux/{amd64,aarch64}/`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/patterns/docker-rust-jenkins-multi-stage/SKILL.md` — смежный паттерн Docker multi-arch
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/patterns/jni-bridge-generator/SKILL.md` — смежный паттерн JNI naming
