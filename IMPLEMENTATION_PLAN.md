# План: переписать Jenkins Gitea plugin на Rust через JNI

## Контекст

Цель — перенести HTTP-клиентскую часть плагина [jenkinsci/gitea-plugin](https://github.com/jenkinsci/gitea-plugin) в Rust, сохранив 100% совместимость с Jenkins-экосистемой (артефакт `.hpi`, существующая конфигурация, UI, остальные плагины).

**Ключевое открытие при исследовании кода.** Плагин уже спроектирован с ServiceLoader SPI `org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory`. Это значит, что любую реализацию `GiteaConnection` можно подключить через `META-INF/services/org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory` **без изменения остальных ~100 Java-классов** (SCM, traits, events, webhook handlers, UI). Тесты плагина уже используют этот механизм через `MockGiteaConnectionFactory`.

**Архитектурные решения (согласованы):**

| Решение | Выбор | Обоснование |
|---|---|---|
| Граница JNI | Rust возвращает JSON-строку, Java парсит существующим Jackson | Минимум JNI-клея, не дублируем 41 POJO |
| Async модель | `reqwest` + `tokio` (lazy `Runtime` через `once_cell`) | Современный стек, понятная модель |
| Платформы | Linux x86_64 только | Покрывает основную целевую аудиторию Jenkins |
| Fallback | Нет, только Rust | Чисто; загрузка .so обязательна для работы плагина |

---

## Архитектура

```
┌────────────────────────────────────────────────────────────┐
│                       Jenkins Controller                    │
│   ┌──────────────────────────────────────────────────────┐ │
│   │ gitea.hpi  (один артефакт .hpi)                      │ │
│   │                                                      │ │
│   │  ┌────────────────────────────────────────────────┐  │ │
│   │  │ ОРИГИНАЛЬНЫЕ ~100 Java-классов                 │  │ │
│   │  │  GiteaSCMSource, GiteaSCMNavigator,            │  │ │
│   │  │  GiteaWebhookListener, traits, events,         │  │ │
│   │  │  PersonalAccessTokenImpl, GiteaServer(s),      │  │ │
│   │  │  41 POJO в client/api/, Jelly templates        │  │ │
│   │  │  → НЕ ТРОГАЕМ                                  │  │ │
│   │  └────────────────────────────────────────────────┘  │ │
│   │                          │ использует                  │ │
│   │                          ▼                            │ │
│   │  interface GiteaConnection  (НЕ ТРОГАЕМ)              │ │
│   │                          ▲                            │ │
│   │                          │ implements                  │ │
│   │  ┌───────────────────────┴────────────────────────┐  │ │
│   │  │ RustGiteaConnection  ← НОВЫЙ Java-класс         │  │ │
│   │  │  - native methods (по одному на каждый fetch*)  │  │ │
│   │  │  - static { Loader.load("gitea_rust"); }        │  │ │
│   │  │  - Jackson ObjectMapper для парсинга JSON       │  │ │
│   │  └───────────────────────┬────────────────────────┘  │ │
│   │                          │ JNI                        │ │
│   │  ┌───────────────────────▼────────────────────────┐  │ │
│   │  │ libgitea_rust.so  (Rust cdylib, bundled)       │  │ │
│   │  │                                                  │  │ │
│   │  │  • reqwest async client (Lazy static)          │  │ │
│   │  │  • tokio Runtime (Lazy static)                  │  │ │
│   │  │  • 40+ #[no_mangle] extern "system" fn         │  │ │
│   │  │  • Поддержка Basic / Token auth                │  │ │
│   │  │  • Построение URI через reqwest::Url           │  │ │
│   │  │  • Возврат сырого JSON как JString              │  │ │
│   │  └──────────────────────────────────────────────────┘ │ │
│   │                                                      │ │
│   │  META-INF/services/...GiteaConnectionFactory         │ │
│   │    → org.jenkinsci.plugin.gitea.client.impl.         │ │
│   │       RustGiteaConnectionFactory                     │ │
│   │                                                      │ │
│   │  META-INF/native/linux/amd64/libgitea_rust.so        │ │
│   └──────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────┘
```

**Удаляем `DefaultGiteaConnection.java` и `DefaultGiteaConnectionFactory.java`** (1181 + ~50 строк). Это основная масса переносимой логики. Их место занимает Rust-сторона + тонкий Java-shim.

---

## Структура workspace

```
GiteaJenkinsPluginRework/
├── pom.xml                      # корневой Maven (packaging=hpi как сейчас)
├── src/
│   ├── main/
│   │   ├── java/org/jenkinsci/plugin/gitea/        # оригинальные классы
│   │   │   ├── ... (не трогаем)
│   │   │   └── client/impl/
│   │   │       ├── RustGiteaConnection.java        # НОВЫЙ
│   │   │       ├── RustGiteaConnectionFactory.java # НОВЫЙ (вместо Default*)
│   │   │       ├── NativeLibraryLoader.java        # НОВЫЙ (распаковка .so)
│   │   │       └── DefaultGiteaConnection.java     # УДАЛИТЬ
│   │   │       └── DefaultGiteaConnectionFactory.java # УДАЛИТЬ
│   │   └── resources/
│   │       ├── META-INF/services/org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory
│   │       │                                          # указывает на RustGiteaConnectionFactory
│   │       └── META-INF/native/linux/amd64/libgitea_rust.so  # собран из Cargo
│   └── test/java/...            # расширяем существующие тесты
├── rust/
│   └── gitea-client/            # Cargo-крейт
│       ├── Cargo.toml           # crate-type = ["cdylib"]
│       ├── src/
│       │   ├── lib.rs           # JNI exports
│       │   ├── client.rs        # GiteaClient (reqwest + auth)
│       │   ├── auth.rs          # BasicAuth, TokenAuth
│       │   ├── error.rs         # GiteaError → JNI exceptions
│       │   └── runtime.rs       # Lazy<Runtime> + Lazy<reqwest::Client>
│       ├── tests/
│       │   └── integration.rs   # тесты с wiremock
│       └── build.rs             # опционально
├── CONTRIBUTING.md
├── CHANGES.md
├── Jenkinsfile                  # добавляем стадию cargo build
└── README.md
```

---

## Этапы реализации

### Этап 0. Подготовка workspace (0.5 дня)

- Скопировать содержимое `jenkinsci/gitea-plugin@master` в `GiteaJenkinsPluginRework/`
- Инициализировать git-репозиторий
- Создать структуру `rust/gitea-client/` (Cargo-проект)
- Закоммитить baseline ("initial: import upstream gitea-plugin @ <commit>")

**Артефакты:**
- `pom.xml`, `src/` (копия апстрима)
- `rust/gitea-client/Cargo.toml` со скелетом
- `.gitignore` (target/, *.so, *.dylib, *.dll)

### Этап 1. Rust GiteaClient — core (2-3 дня)

Реализовать Rust-сторону **без JNI** — как обычный Rust-крейт с тестами. Это даёт чистый, тестируемый HTTP-клиент, который можно разрабатывать и запускать отдельно от Jenkins.

**Файлы:**
- `rust/gitea-client/Cargo.toml`
- `rust/gitea-client/src/client.rs` — структура `GiteaClient { base_url, auth, http }`
- `rust/gitea-client/src/auth.rs` — `enum Auth { None, Token(String), Basic(String, String) }`
- `rust/gitea-client/src/error.rs` — `thiserror::Error`
- `rust/gitea-client/src/runtime.rs` — Lazy statics для Runtime и Client

**Покрыть методы** (полный список — из `GiteaConnection` interface, 33 уникальных):

```
GET   /api/v1/version                            fetchVersion
GET   /api/v1/user                               fetchCurrentUser
GET   /api/v1/users/{name}                       fetchUser
GET   /api/v1/orgs/{name}                        fetchOrganization
GET   /api/v1/users/{name}  + /orgs/{name}       fetchOwner (двойная попытка)
GET   /api/v1/repos/{owner}/{repo}               fetchRepository
GET   /api/v1/user/repos                         fetchCurrentUserRepositories
GET   /api/v1/users/{name}/repos                 fetchRepositories
GET   /api/v1/orgs/{org}/repos                   fetchOrganizationRepositories
GET   /api/v1/repos/{owner}/{repo}/branches/{b}  fetchBranch (с поддержкой '/')
GET   /api/v1/repos/{owner}/{repo}/branches      fetchBranches
GET   /api/v1/repos/{owner}/{repo}/git/tags/{sha} fetchAnnotatedTag
GET   /api/v1/repos/{owner}/{repo}/tags/{tag}    fetchTag
GET   /api/v1/repos/{owner}/{repo}/tags          fetchTags
GET   /api/v1/repos/{owner}/{repo}/git/commits/{sha} fetchCommit
GET   /api/v1/repos/{owner}/{repo}/collaborators fetchCollaborators
HEAD  /api/v1/repos/{owner}/{repo}/collaborators/{u} checkCollaborator
GET   /api/v1/orgs/{org}/hooks                   fetchHooks (org)
POST  /api/v1/orgs/{org}/hooks                   createHook (org)
DEL   /api/v1/orgs/{org}/hooks/{id}              deleteHook (org)
PATCH /api/v1/orgs/{org}/hooks/{id}              updateHook (org)
GET   /api/v1/repos/{owner}/{repo}/hooks         fetchHooks (repo)
POST  /api/v1/repos/{owner}/{repo}/hooks         createHook (repo)
DEL   /api/v1/repos/{owner}/{repo}/hooks/{id}    deleteHook (repo)
PATCH /api/v1/repos/{owner}/{repo}/hooks/{id}    updateHook (repo)
GET   /api/v1/repos/{owner}/{repo}/statuses/{sha} fetchCommitStatuses
POST  /api/v1/repos/{owner}/{repo}/statuses/{sha} createCommitStatus
GET   /api/v1/repos/{owner}/{repo}/pulls/{id}    fetchPullRequest
GET   /api/v1/repos/{owner}/{repo}/pulls         fetchPullRequests (?state=)
GET   /api/v1/repos/{owner}/{repo}/issues        fetchIssues (?state=)
GET   /api/v1/repos/{owner}/{repo}/raw/{ref}/{path} fetchFile
GET   /api/v1/repos/{owner}/{repo}/releases      fetchReleases (?draft=&prerelease=)
POST  /api/v1/repos/{owner}/{repo}/releases/{id}/assets createReleaseAttachment (multipart)
```

**Ключевые детали реализации:**
- Auth header: для Token → `Authorization: token <T>` (это специфика Gitea, не Bearer!), для Basic → `Authorization: Basic <base64>`
- Существующая логика плагина: для `fetchOwner` сначала пробуется `/orgs/`, при 404 откат на `/users/`. Сохраняем.
- Для 404 на `fetchPullRequests`/`fetchIssues` — возвращать пустой список (PR/issues могут быть отключены на сервере). Сохраняем.
- Для `fetchFile` 404 → исключение `FileNotFoundException`. Сохраняем.
- Pagination через `Link` header (нужно изучить `DefaultGiteaConnection_PagedRequests_Test.java`)
- Поддержка Jenkins Proxy: сейчас плагин берёт `Jenkins.get().proxy`. Это сложнее — либо прокидываем proxy в Rust, либо оставляем как ограничение MVP с TODO.

**Тесты (Rust unit):**
- Используем `wiremock` для mock-сервера
- Один тест на каждый метод (вызов + проверка URL/auth/ответа)
- Тест на 404-обработку для PR/issues
- Тест на pagination

### Этап 2. JNI exports (1-2 дня)

Поверх Rust core — слой JNI. Каждой операции соответствует один `#[no_mangle] extern "system" fn`.

**Паттерн:**

```rust
#[no_mangle]
pub extern "system" fn Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_fetchRepository(
    mut env: JNIEnv,
    _cls: JClass,
    server_url: JString,
    auth_type: jint,        // 0=none, 1=token, 2=basic
    auth_secret: JString,   // token | "user:password" | ""
    owner: JString,
    repo: JString,
) -> jstring {
    let server_url = jstr(&mut env, server_url);
    let owner = jstr(&mut env, owner);
    let repo = jstr(&mut env, repo);
    let auth = decode_auth(auth_type, &jstr(&mut env, auth_secret));

    let result = RT.block_on(async {
        let client = GiteaClient::new(&server_url, auth);
        client.fetch_repository(&owner, &repo).await
    });

    match result {
        Ok(json) => env.new_string(json).unwrap().into_raw(),
        Err(e) => {
            throw_gitea_exception(&mut env, &e);
            std::ptr::null_mut()
        }
    }
}
```

**Вспомогательные функции:**
- `jstr(env, jstring) -> String` — безопасное извлечение строки
- `decode_auth(jint, &str) -> Auth`
- `throw_gitea_exception(env, err)` — маппит `GiteaError` в:
  - `GiteaHttpStatusException` для HTTP-ошибок (с code/message/body)
  - `IOException` для сетевых
  - `InterruptedException` не нужен (sync модель)

### Этап 3. Java shim — RustGiteaConnection (1-2 дня)

**`RustGiteaConnection.java`** — реализует интерфейс `GiteaConnection` целиком. Каждый метод:

```java
@Override
public GiteaRepository fetchRepository(String username, String name)
        throws IOException, InterruptedException {
    String json = nativeFetchRepository(
        serverUrl, authEncoded(), username, name
    );
    return mapper.readerFor(GiteaRepository.class).readValue(json);
}

private static native String nativeFetchRepository(
    String serverUrl, int authType, String authSecret,
    String username, String name
);
```

**Структура класса:**
- `private final String serverUrl;`
- `private final GiteaAuth authentication;` (как в Default)
- `private final ObjectMapper mapper = new ObjectMapper();`
- Static initializer → грузит .so через `NativeLibraryLoader`
- 33+ native methods (по числу уникальных операций)
- Конструктор копирует сигнатуру `DefaultGiteaConnection`

**`NativeLibraryLoader.java`** — распаковывает `.so` из classpath во временную папку и грузит:

```java
static void load(String libName) {
    String mappedName = System.mapLibraryName(libName); // libgitea_rust.so
    String resourcePath = "/META-INF/native/linux/amd64/" + mappedName;
    try (InputStream in = NativeLibraryLoader.class.getResourceAsStream(resourcePath)) {
        if (in == null) throw new UnsatisfiedLinkError("Missing: " + resourcePath);
        Path tmp = Files.createTempFile("gitea-rust-", ".so");
        tmp.toFile().deleteOnExit();
        Files.copy(in, tmp, REPLACE_EXISTING);
        System.load(tmp.toString());
    } catch (IOException e) {
        throw new ExceptionInInitializerError(e);
    }
}
```

**`RustGiteaConnectionFactory.java`** — замена `DefaultGiteaConnectionFactory`:

```java
@org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory
public class RustGiteaConnectionFactory implements GiteaConnectionFactory {
    @Override
    public GiteaConnection open(String serverUrl, GiteaAuth auth) {
        return new RustGiteaConnection(serverUrl, auth);
    }
}
```

### Этап 4. Регистрация SPI и удаление старого (0.5 дня)

- Изменить `src/main/resources/META-INF/services/org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory`:
  ```
  org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnectionFactory
  ```
- **Удалить** `DefaultGiteaConnection.java` и `DefaultGiteaConnectionFactory.java`
- Проверить, что никакой код не ссылается на `Default*` (только через интерфейс)

### Этап 5. Maven-интеграция с Cargo (0.5 дня)

В `pom.xml` добавить `exec-maven-plugin`:

```xml
<plugin>
    <groupId>org.codehaus.mojo</groupId>
    <artifactId>exec-maven-plugin</artifactId>
    <executions>
        <execution>
            <id>cargo-build</id>
            <phase>generate-resources</phase>
            <goals><goal>exec</goal></goals>
            <configuration>
                <executable>cargo</executable>
                <workingDirectory>${project.basedir}/rust/gitea-client</workingDirectory>
                <arguments>
                    <argument>build</argument>
                    <argument>--release</argument>
                </arguments>
            </configuration>
        </execution>
    </executions>
</plugin>
<plugin>
    <groupId>org.apache.maven.plugins</groupId>
    <artifactId>maven-resources-plugin</artifactId>
    <executions>
        <execution>
            <id>copy-native-lib</id>
            <phase>process-resources</phase>
            <goals><goal>copy-resources</goal></goals>
            <configuration>
                <outputDirectory>${project.build.outputDirectory}/META-INF/native/linux/amd64</outputDirectory>
                <resources>
                    <resource>
                        <directory>${project.basedir}/rust/gitea-client/target/release</directory>
                        <includes><include>libgitea_rust.so</include></includes>
                    </resource>
                </resources>
            </configuration>
        </execution>
    </executions>
</plugin>
```

Это соберёт Rust перед Maven-фазой ресурсов и упакует `.so` в `.hpi`.

### Этап 6. Тестирование (1-2 дня)

**Rust unit-тесты** — все в `rust/gitea-client/tests/`:
- `wiremock`-based тесты каждого метода
- Тесты на 404-обработку (PR/issues → пустой список)
- Тесты на pagination

**Java unit-тесты:**
- Существующий `DefaultGiteaConnectionTest.java` → переименовать в `RustGiteaConnectionTest.java`, адаптировать под новую реализацию (входные/выходные данные через JNI, но тесты можно оставить mock-based через `MockGiteaConnection`)
- Добавить `RustGiteaConnectionSmokeTest.java` — проверяет, что .so грузится, методы вызываются (через `wiremock`-сервер на Java-стороне)
- Тестовые JSON-фикстуры (`src/test/resources/.../branchesResponse.json` и т.д.) — переиспользуем как есть

**Integration test (опционально):**
- Поднять Gitea container через testcontainers
- Запустить full cycle: fetchRepository → fetchBranches → createCommitStatus

### Этап 7. CI и упаковка (0.5 дня)

Обновить `Jenkinsfile` — добавить стадию сборки Rust:

```groovy
pipeline {
    agent any
    tools { jdk 'JDK21' }
    stages {
        stage('Build Rust') {
            steps {
                sh 'cargo --version'
                sh 'cd rust/gitea-client && cargo build --release'
                sh 'cd rust/gitea-client && cargo test'
            }
        }
        stage('Build Plugin') {
            steps {
                sh 'mvn -B -DskipTests=false clean package'
            }
        }
    }
}
```

### Этап 8. Документация (0.5 дня)

- Обновить `README.md` — объяснить архитектуру Rust+Java
- Обновить `CHANGES.md` — записать breaking change (удалён Default*)
- Раздел "Сборка" — требует `cargo` и JDK 21 на машине разработчика
- Раздел "Совместимость" — только Linux x86_64 на контроллере

---

## Ключевые файлы — что меняем

| Файл | Действие | Объём |
|---|---|---|
| `src/main/java/.../client/impl/DefaultGiteaConnection.java` | **Удалить** | -1181 строк |
| `src/main/java/.../client/impl/DefaultGiteaConnectionFactory.java` | **Удалить** | -~50 строк |
| `src/main/java/.../client/impl/RustGiteaConnection.java` | **Создать** | ~500 строк (33 метода × ~15 строк) |
| `src/main/java/.../client/impl/RustGiteaConnectionFactory.java` | **Создать** | ~30 строк |
| `src/main/java/.../client/impl/NativeLibraryLoader.java` | **Создать** | ~60 строк |
| `src/main/resources/META-INF/services/...GiteaConnectionFactory` | **Изменить** | 1 строка |
| `pom.xml` | **Изменить** | +30 строк (cargo integration) |
| `Jenkinsfile` | **Изменить** | +10 строк |
| `rust/gitea-client/` (весь крейт) | **Создать** | ~2000 строк (lib.rs ~400, client.rs ~600, tests ~1000) |
| Остальные ~95 Java-классов | **НЕ ТРОГАЕМ** | 0 |

---

## Риски и что не входит в MVP

### Риски

1. **Tokio runtime в JNI-контексте.** Tokio запускает background-threads. При выгрузке/перезагрузке плагина (Jenkins hot-reload) потоки могут остаться. Решение: плагин не должен поддерживать hot reload в MVP, только через restart Jenkins.

2. **Двойной парсинг JSON.** Rust использует `serde_json` для типизации, потом сериализует обратно в строку через `serde_json::to_string`. Java парсит обратно через Jackson. В медленных путях (fetchBranches для крупных репо) это может дать ~5-15% overhead. Решение: для hot-методов можно оптимизировать через raw JSON passthrough в Rust (без типизации).

3. **Jenkins Proxy.** Текущий `DefaultGiteaConnection` использует `Jenkins.get().proxy`. В Rust это нужно прокидывать вручную (reqwest поддерживает proxy). В MVP — пропустим, documented limitation.

4. **Pagination через Link header.** Существует `DefaultGiteaConnection_PagedRequests_Test.java` — там специфичная логика парсинга `Link: <...>; rel="next"`. Нужно аккуратно портировать.

5. **Multipart upload в `createReleaseAttachment`.** Сложный кейс (InputStream из Java). В MVP можно сделать через временный буфер (читать InputStream в byte[] в Java, передавать в Rust как jbyteArray).

6. **`GiteaAuthNone`.** Тип auth для анонимных запросов. Нужно покрыть в Rust.

### Не входит в MVP (явные TODO)

- Кросс-платформенность (macOS, Windows, Linux aarch64)
- Jenkins Proxy support в Rust
- Hot-reload плагина
- Async concurrency для batch-запросов
- Оптимизация через MessagePack/CBOR
- Fallback на DefaultGiteaConnection

---

## Верификация

После реализации каждый шаг проверяется:

1. **После этапа 1 (Rust core):**
   ```bash
   cd rust/gitea-client
   cargo test
   ```
   Все unit-тесты с wiremock проходят.

2. **После этапа 3 (Java shim):**
   ```bash
   mvn test -Dtest=RustGiteaConnectionSmokeTest
   ```
   `.so` загружается, методы вызываются, возвращают корректные данные.

3. **После этапа 5 (Maven integration):**
   ```bash
   mvn clean package
   ```
   Собирается `target/gitea.hpi`, внутри в `META-INF/native/linux/amd64/libgitea_rust.so`.

4. **End-to-end:**
   ```bash
   # Поднять Jenkins локально с установленным .hpi
   mvn hpi:run
   # В UI: Configure System → Gitea Servers → добавить сервер
   # Создать Organization Folder, проверить, что ветки и PR распознаются
   # Запустить сборку, проверить, что commit status публикуется в Gitea
   ```
   
5. **Регрессия:**
   ```bash
   mvn test  # все 14 существующих тестов проходят (после адаптации DefaultGiteaConnectionTest)
   ```

---

## Таймлайн

| Этап | Время |
|---|---|
| 0. Workspace | 0.5 дня |
| 1. Rust core | 2-3 дня |
| 2. JNI exports | 1-2 дня |
| 3. Java shim | 1-2 дня |
| 4. SPI регистрация | 0.5 дня |
| 5. Maven integration | 0.5 дня |
| 6. Тестирование | 1-2 дня |
| 7. CI | 0.5 дня |
| 8. Документация | 0.5 дня |
| **Итого** | **7-11 дней** для MVP |
