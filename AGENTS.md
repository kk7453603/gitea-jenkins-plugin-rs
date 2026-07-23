# AGENTS.md

Контекстный файл для AI-агентов (Claude Code и др.), начинающих работу над этим репозиторием. Прочитай целиком перед любым действием.

## Что это

Fork [jenkinsci/gitea-plugin](https://github.com/jenkinsci/gitea-plugin) @ `ae31972` с HTTP-клиентской частью, переписанной на **Rust через JNI**. Плагин собирается в стандартный `.hpi`, совместим с Jenkins 2.479.3+, ставится в Production без изменений UI.

**Главное архитектурное решение:** используется встроенный upstream'ом ServiceLoader SPI `GiteaConnectionFactory`. Благодаря этому **не тронуты ~95 Java-классов** (SCM, traits, events, webhook, UI) — заменены только `Default*` → `Rust*` + добавлен Rust-крейт `rust/gitea-client/`.

## Текущий статус

| Этап | Статус | Что |
|---|---|---|
| 0 | ✅ done | Workspace setup |
| 1 | ✅ done | Rust GiteaClient core (50 wiremock-тестов) |
| 2 | ✅ done | JNI exports (35 `#[no_mangle]` functions) |
| 3 | ✅ done | Java shim (`RustGiteaConnection`, `Factory`, `NativeLibraryLoader`) |
| 4 | ✅ done | SPI registration, удалены `Default*` |
| 5 | ✅ done | Maven-Cargo интеграция |
| 6 | ✅ done | Smoke-тесты Rust+JNI |
| 7 | ✅ done | CI Jenkinsfile |
| 8 | ✅ done | README/CHANGES/CONTRIBUTING |
| Docker | ✅ done | Multi-stage Dockerfile + docker-compose |
| **9.A** | ⏳ **in progress** (background agent) | Rust webhook server (axum + HMAC + JNI callback) |
| 9.B | ⏸ pending | Java dispatcher + UI (after 9.A) |
| 9.C | ⏸ pending | Webhook tests + docs |
| 10 | planned | Polling scheduler |
| 12 | planned | TLS / Jenkins trust store |
| 13 | planned | Jenkins HTTP proxy support |
| 16 | planned | Auth extensions (IP allowlist, rate limit) |

**Полная карта этапов 10-17** — в чате с пользователем, резюме в `IMPLEMENTATION_PLAN.md` (раздел "Не входит в MVP").

## Архитектура (TL;DR)

```
Jenkins (JVM)
  └── ~95 Java-классов (НЕ ТРОГАЕМ)
        │ используют
        ▼
  interface GiteaConnection (35 методов)
        ▲
        │ implements
  ┌─────┴──────────────────────────────┐
  │ RustGiteaConnection.java           │  ← JNI-shim
  │   static { Loader.load("gitea_rust"); }
  │   35 private static native методов │
  └─────┬──────────────────────────────┘
        │ JNI
  ┌─────▼──────────────────────────────┐
  │ libgitea_rust.so                   │  ← Rust cdylib
  │   jni.rs: 35 extern "system" fn    │
  │   client.rs: GiteaClient (reqwest) │
  │   auth.rs: Auth {None/Token/Basic} │
  │   runtime.rs: Lazy<tokio::Runtime> │
  └────────────────────────────────────┘
        │ HTTPS
        ▼
     Gitea API
```

Этап 9 добавит `server.rs` (axum HTTP server на :8081) + `events.rs` + `jni_webhook.rs` — Rust будет принимать webhook'и напрямую от Gitea.

## Структура каталогов

```
GiteaJenkinsPluginRework/
├── AGENTS.md                    ← ты здесь
├── IMPLEMENTATION_PLAN.md       ← полный план, читай для деталей
├── README.md                    ← пользовательская документация
├── CHANGES.md                   ← breaking changes по версиям
├── CONTRIBUTING.md              ← как добавлять фичи
├── pom.xml                      ← Maven (packaging=hpi)
├── Jenkinsfile                  ← CI pipeline
├── docker-compose.yml           ← локальный Jenkins + Gitea
├── docker/
│   ├── Dockerfile               ← 3-stage: rust → maven → jenkins:lts-jdk21
│   ├── plugins.txt              ← multibranch, branch-api, git, ...
│   └── README.md                ← как поднять
├── src/                         ← Java-часть
│   ├── main/java/org/jenkinsci/plugin/gitea/
│   │   ├── client/impl/         ← НАШИ файлы здесь
│   │   │   ├── RustGiteaConnection.java
│   │   │   ├── RustGiteaConnectionFactory.java
│   │   │   └── NativeLibraryLoader.java
│   │   └── ... (остальное не трогать)
│   ├── main/resources/META-INF/
│   │   ├── services/...GiteaConnectionFactory  ← 1 строка: RustGiteaConnectionFactory
│   │   └── native/linux/amd64/   ← libgitea_rust.so (после сборки)
│   └── test/java/.../client/impl/
│       └── RustGiteaConnectionSmokeTest.java
└── rust/
    └── gitea-client/
        ├── Cargo.toml           ← crate-type=["cdylib","rlib"]
        ├── src/
        │   ├── lib.rs           ← pub модули
        │   ├── auth.rs          ← Auth enum
        │   ├── client.rs        ← GiteaClient (33 метода)
        │   ├── error.rs         ← GiteaError
        │   ├── runtime.rs       ← Lazy<Runtime>
        │   ├── jni.rs           ← 35 #[no_mangle] extern "system" fn
        │   ├── server.rs        ← (этап 9.A — axum webhook server)
        │   ├── events.rs        ← (этап 9.A — 7 Gitea event types)
        │   └── jni_webhook.rs   ← (этап 9.A — nativeStart/nativeStop)
        └── tests/
            ├── integration.rs   ← 49 wiremock-тестов всех методов
            └── jni_symbols.rs   ← проверка наличия JNI-символов в .so
```

## Команды разработчика

### Сборка

```bash
# Rust (быстро, на macOS)
cd rust/gitea-client && cargo build --release

# Java (долго, тянет все Jenkins deps)
mvn -B clean package -DskipTests -Dban-junit4-imports.skip=true

# Docker (multi-stage, ~5-15 мин с кэшем)
docker compose build
docker compose up -d
# UI: http://localhost:8080
# Admin password: docker compose exec jenkins cat /var/jenkins_home/secrets/initialAdminPassword
```

### Тесты

```bash
# Rust unit + integration (50 тестов, ~30 сек)
cd rust/gitea-client && cargo test

# Java smoke (пропускается без .so через Assume.assumeTrue)
mvn test -Dtest=RustGiteaConnectionSmokeTest
```

### Локальная разработка цикла

```bash
# 1. Меняешь Rust →
cd rust/gitea-client && cargo test

# 2. Меняешь Java-shim →
mvn compile -DskipTests -Dban-junit4-imports.skip=true -o

# 3. Хочешь end-to-end в Jenkins →
docker compose build && docker compose up -d
```

## Согласованные архитектурные решения (НЕ менять)

| Решение | Значение | Обоснование |
|---|---|---|
| Граница JNI | Rust отдаёт JSON-строку, Java парсит Jackson | Минимум клея, не дублируем 41 POJO |
| Async стек | `reqwest` + `tokio` | Современный, хорошо поддерживается |
| Runtime | Lazy `tokio::Runtime` через `once_cell`, 1 на process | Hot-reload не поддерживается (см. ниже) |
| Auth header | `Authorization: token <T>` (НЕ `Bearer`) | Gitea-specific |
| 404 → `[]` | для `fetchPullRequests`/`fetchIssues`/`fetchReleases` | эти endpoints могут быть отключены на сервере |
| 404 → `FileNotFound` | для `fetchFile` | совпадает с Java `FileNotFoundException` |
| Pagination | парсинг `Link: <...>; rel="next"` + конкатенация JSON-массивов + удаление `null` | порт из upstream |
| `fetchOwner` | double-fetch: orgs/ → users/ при 404 | поведение upstream |
| URL-encoding | `percent_encode_path_segment` — не кодирует `.` | совместимость с Gitea |
| Платформа prod | Linux x86_64 | Jenkins controller; macOS только для dev |

## Известные ограничения (MVP)

- ❌ **Hot-reload плагина** — Tokio потоки не убираются при unmount, нужен restart Jenkins
- ❌ **Jenkins HTTP Proxy** — в Rust TODO (этап 13)
- ❌ **Cross-platform** — только Linux x86_64 в prod (macOS собирает `.dylib` для dev)
- ❌ **Self-signed Gitea certs** — Rust использует webpki-roots (этап 12 добавит Jenkins trust store)
- ❌ **Polling fallback** — webhook-only (этап 10)

## Чего НЕ делать

1. **Не правь ~95 Java-классов** вне `client/impl/`. Любое изменение в `api/`, `traits/`, `events/`, `servers/`, `scm/` требует обсуждения.
2. **Не используй `Bearer` для token auth** — только `token <T>`.
3. **Не удаляй `NativeLibraryLoader`** — без него `UnsatisfiedLinkError` при запуске.
4. **Не добавляй новые типы POJO в `client/api/`** — существующие Jackson-аннотации от upstream. Если нужен новый тип — пусть Rust возвращает JSON, Java парсит в существующий POJO.
5. **Не запускай `mvn package` без `-Dban-junit4-imports.skip=true`** — enforcer родительского POM валит сборку из-за JUnit 4 в smoke-тесте.
6. **Не делай `--no-verify`/`--amend`** — обычные коммиты с HEREDOC.

## Соглашения по коммитам

```
research(init): ...      — workspace setup
research(protocol): ...  — фиксируем план ДО запуска
research(results): H<N> — ...  — результат этапа
research(reflect): ...   — смена направления
research(infra): ...     — docker/ci
```

## Сабагенты

Каждый этап запускается как **background subagent** с детальным prompt. Паттерн:

```
1. Прочитай IMPLEMENTATION_PLAN.md (раздел <этап>)
2. Прочитай соответствующие upstream файлы в /tmp/gitea-plugin/
3. Реализуй X файлов
4. cargo test / mvn compile (проверка)
5. Git commit с HEREDOC
6. Не трогай файлы вне scope
```

**Если сабагент падает (watchdog 600s):** проверь что он успел сделать через `git status` + `git diff`, дозаверши вручную.

## Полезные навыки (установлены в `~/.claude/skills/`)

| Skill | Когда применять |
|---|---|
| `verification-loop` | После каждого этапа: build → test → security check |
| `tdd-workflow` | Для новых Rust модулей: сначала тесты, потом impl |
| `continuous-agent-loop` | Управление длинными цепочками сабагентов |
| `security-review` | Ревью webhook layer, auth, HMAC (этап 9, 16) |
| `e2e-testing` | End-to-end: curl POST webhook → Jenkins log → assert build triggered |
| `token-budget-advisor` | Перед длинными ответами — спроси уровень детализации |
| `docker-patterns` | Оптимизация Dockerfile, multi-service compose |

## 🧠 Дополнительные находки (Jenkins SDK)

**Контекст:** Анализ крейта `jenkins-sdk`, который предоставляет высокоуровневый API для Jenkins.
**Файл с деталями:** [`jenkins_sdk_findings/jenkins_sdk_integration_proposals.md`](jenkins_sdk_findings/jenkins_sdk_integration_proposals.md)

**Краткое резюме предложений:**
1.  **Полная замена (Async):** Полный переход на асинхронный клиент SDK.
2.  **Гибридный подход:** Использование `Client` и `BlockingClient` по мере необходимости.
3.  **Абстракция через GiteaClient (Рекомендуется):** Оборачивание вызовов SDK внутри существующих методов `GiteaClient`, что минимизирует изменения в Java-shim.

**Рекомендация:** Начать с **Способа 3**.
