# Agent Skills Catalog

Этот каталог содержит skills для AI-агентов (Claude, qwen, и др.) которые
работают над Jenkins Gitea plugin. Каждый skill — это `SKILL.md` файл с
pattern, trigger conditions, примерами кода, подводными камнями.

## Структура

| Директория | Что внутри |
|---|---|
| `patterns/` | Архитектурные patterns из этого проекта (8 файлов, выведены из v1.1.0) |
| `core/` | Базовые dev skills (TDD, verification, prompts, agent loops) |
| `rust/` | Rust-specific patterns (idioms, testing) |
| `jenkins/` | Jenkins + Java + Docker + e2e |
| `security/` | Security review + scan |
| `watchmen/` | Watchmen curator (auto-discovery of new patterns) |

## Использование

Агент должен:

1. Прочитать `AGENTS.md` в корне репозитория — общий контекст (архитектура, "чего НЕ делать", согласованные решения)
2. Прочитать релевантные skills из этого каталога перед задачей
3. Следовать patterns из `patterns/` для подобных задач

## Triggers (для auto-discovery)

Агент может загрузить skill когда:

- Видит ключевые слова в prompt (например "webhook", "JNI", "Maven-Cargo", "Dockerize plugin")
- Касается файлов из "Файлы-референсы" секции соответствующего skill-а
- Натыкается на проблему из "Подводные камни" секции (например `UnsatisfiedLinkError`, `ClassNotFoundException`)

## Patterns (8 файлов в `patterns/`)

| Skill | Когда применять |
|---|---|
| `docker-rust-jenkins-multi-stage` | Контейнеризация Jenkins-плагина с Rust-ядром; multi-arch билд |
| `jni-bridge-generator` | Добавление новых JNI export-ов; naming convention; libloading-тесты |
| `json-over-jni-bridge` | Boundary Rust↔Java через JSON; новый метод в `GiteaConnection` |
| `maven-cargo-integration` | `pom.xml` + соседний `rust/`; skip-флаги для Docker |
| `native-library-loader` | `UnsatisfiedLinkError`; multi-arch `.so` из classpath |
| `parallel-multi-stage-orchestration` | Большие миграции через параллельные сабагенты |
| `serviceloader-native-replacement` | Подмена `Default*` на native через `META-INF/services/` |
| `webhook-jni-callback-server` | HTTP server в Rust + callback в JVM через `GlobalRef` |

## Соглашения

- Все skills на русском (как общаемся)
- Frontmatter: `name`, `description`, `origin`, `tags`
- 5 секций в каждом skill: "Когда применять", "Паттерн", "Подводные камни", "Файлы-референсы"
- Реальные примеры кода из проекта v1.1.0
- Абсолютные пути в "Файлы-референсы" (агенты могут их сразу читать)
