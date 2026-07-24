---
name: parallel-multi-stage-orchestration
description: Структура промптов для запуска 8-17 параллельных сабагентов через background subagents с watchdog-таймаутами, явными acceptance criteria и git-based context. Применять для больших миграций/рефакторингов, когда задача естественно декомпозируется на 9+ независимых этапов.
origin: gitea-jenkins-plugin-rs v1.1.0
tags: [orchestration, subagents, parallel, migration, prompt-engineering, git]
---

# Параллельная multi-stage оркестрация сабагентов

## Когда применять

- Большая миграция/рефакторинг (40+ файлов, 5+ дней)
- Задача естественно декомпозируется на 9+ независимых этапов
- Минимум 3+ этапа могут идти параллельно (нет зависимостей)
- Хочется сократить wall-clock время разработки через параллелизм сабагентов
- В репозитории есть `IMPLEMENTATION_PLAN.md` с поэтапным планом

## Паттерн

### Подготовка: `IMPLEMENTATION_PLAN.md`

Перед запуском сабагентов фиксируем план — это **contract** между оркестратором и каждым сабагентом:

```markdown
# Implementation Plan

## Stage 9.A: Rust webhook server
**Зависимости:** stage 8 (CI), stage 2 (JNI exports)
**Файлы:** rust/gitea-client/src/{server.rs, jni_webhook.rs, events.rs}
**Acceptance:**
- cargo test проходит
- libloading-test видит новые JNI символы (nativeStart, nativeStop, nativeRegisterDispatcherClass)
- jar uf в Docker кладёт обновлённый .so в .hpi
- Git commit с HEREDOC message

## Stage 9.B: Java dispatcher + UI
**Зависимости:** stage 9.A (native exports должны быть готовы)
...
```

### Структура промпта для каждого сабагента

Каждый промпт = 4 секции: контекст → что сделать → verification → commit.

```
Ты — сабагент для этапа 9.A Jenkins Gitea plugin (Rust webhook server).

## Контекст
1. Прочитай /Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/AGENTS.md
   целиком — там архитектура и "чего НЕ делать".
2. Прочитай /Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/IMPLEMENTATION_PLAN.md
   раздел "Stage 9.A" — там твой scope.
3. Прочитай upstream reference в /tmp/gitea-plugin/webhook/ — там
   upstream-имплементация GiteaWebhook для понимания контракта.

## Что сделать
1. Создай rust/gitea-client/src/server.rs — axum HTTP server с HMAC-verify
   и IP-allowlist. См. AGENTS.md "Согласованные архитектурные решения".
2. Создай rust/gitea-client/src/jni_webhook.rs — 3 JNI export-а:
   nativeStart, nativeStop, nativeRegisterDispatcherClass.
3. Расширь rust/gitea-client/tests/jni_symbols.rs — добавь новые символы
   в EXPECTED_SYMBOLS.
4. НЕ ТРОГАЙ:
   - 95 Java-классов вне client/impl/ и webhook/
   - pom.xml (без явного разрешения)
   - AGENTS.md (это делает оркестратор)
   - любые файлы вне твоего scope

## Verification (обязательно)
1. cd rust/gitea-client && cargo test — все тесты должны проходить
2. cargo build --release — должен собираться без warnings
3. cargo test --test jni_symbols — проверка символов должна проходить
4. НЕ запускай mvn package — это долго и scope Java-side

## Git commit
После успешной verification:
git add rust/gitea-client/src/server.rs rust/gitea-client/src/jni_webhook.rs \
        rust/gitea-client/tests/jni_symbols.rs
git commit -m "$(cat <<'EOF'
feat(stage-9a): Rust webhook server (axum + HMAC + JNI callback)

- server.rs: axum HTTP server на :8081, IP allowlist, rate limit,
  bearer/HMAC verify
- jni_webhook.rs: nativeStart/nativeStop/nativeRegisterDispatcherClass
- jni_symbols.rs: добавить 3 новых символа в EXPECTED_SYMBOLS

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"

## Acceptance criteria (явные)
- [ ] cargo test проходит (50+ тестов)
- [ ] cargo build --release без warnings
- [ ] libloading-test видит 3 новых символа
- [ ] Git commit сделан с HEREDOC
- [ ] Файлы вне scope не изменены (проверь `git diff --stat`)
```

### Запуск через background subagents

Каждый сабагент запускается с `run_in_background: true` и watchdog-таймаутом:

```bash
# Оркестратор запускает 3 параллельных сабагента
# Stage 9.A (Rust webhook server) — независим от 9.B и 9.C
# Stage 11 (TLS support) — независим
# Stage 12 (HTTP proxy) — независим

# Каждый — отдельный background subagent с watchdog 600s
```

Watchdog 600s (10 мин) — эмпирический лимит. Если сабагент не уложился, он убивается, оркестратор проверяет `git status` + `git diff` что успело сделаться, и дозавершает вручную.

### Зависимости между этапами

Не все этапы можно запускать параллельно. Зависимости:

```
9.A (Rust server)  ──┬──→ 9.B (Java dispatcher)  ──→ 9.C (tests + docs)
                     │
                     └──→ 9.D (webhook integration tests)

11 (TLS)            ────────────────────────────→ 12 (proxy) → 13 (combined)
10 (polling)        ────────────────────────────→ (независимо от 9)
16 (IP allowlist)   ──→ уже внутри 9.A
```

Оркестратор выстраивает **волны**:
- Волна 1: 9.A, 10, 11 (параллельно — нет общих файлов)
- Волна 2: 9.B, 12 (после 9.A и 11)
- Волна 3: 9.C, 9.D, 13 (после 9.B и 12)

### Защита от конфликтов

Главное правило: **никакие два параллельных сабагента не должны трогать один файл**.

Плохо:
- Сабагент A меняет `rust/gitea-client/src/lib.rs` (добавляет `pub mod server;`)
- Сабагент B тоже меняет `lib.rs` (добавляет `pub mod polling;`)
- Оба делают `git add lib.rs` — конфликт

Хорошо:
- Сабагент A отвечает за `server.rs` + `jni_webhook.rs` + `lib.rs` (он один трогает lib.rs)
- Сабагент B отвечает за `polling.rs` + `jni_polling.rs` — но добавление `pub mod polling;` в `lib.rs` делегировано A (или делается отдельным merge-step после обеих волн)

Альтернатива — выделить "shared-files" этап: оркестратор сам обновляет `lib.rs`, `AGENTS.md`, `pom.xml` после каждой волны.

### Git commit с HEREDOC

ВСЕГДА HEREDOC для многострочных commit messages:

```bash
git commit -m "$(cat <<'EOF'
feat(stage-9a): Rust webhook server (axum + HMAC + JNI callback)

- server.rs: axum HTTP server на :8081, IP allowlist, rate limit,
  bearer/HMAC verify
- jni_webhook.rs: nativeStart/nativeStop/nativeRegisterDispatcherClass

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

НЕ используйте `--amend`, `--no-verify`, single-line `-m "..."` для сложных коммитов.

### Watchdog и восстановление

Если сабагент упал по watchdog:

```bash
# 1. Что успело сделаться?
git status
git diff
git log --oneline -5

# 2. Если файлы есть но нет commit-а — дозапусти verification
cargo test
# если проходит — git commit вручную с правильным message

# 3. Если файлы неполные — дозаверши вручную, потом commit
```

Не перезапускай сабагент с нуля — это потеря контекста. Лучше дозавершить с того места, где он остановился.

## Подводные камни

1. **Конкурентный `git add`.** Два сабагента могут одновременно закоммитить в один файл. Решение — строгие scope-границы, shared-files этап после волны.
2. **Watchdog слишком короткий.** 600s — это ~10 минут. Если сабагент делает сложный refactor, может не уложиться. Решение — повышать до 1200s для известных долгих этапов (миграция тестов, refactor крупных модулей).
3. **Сабагент не читает `AGENTS.md`.** Это самая частая ошибка. Без контекста он нарушает "Согласованные архитектурные решения" (например, шлёт `Bearer` вместо `token`). Решение — первый пункт промпта всегда "Прочитай AGENTS.md целиком".
4. **Сабагент создаёт новый POJO в `client/api/`.** Запрещено AGENTS.md. Решение — явное "НЕ ТРОГАЙ" в промпте.
5. **Commit message без HEREDOC.** `git commit -m "line1\nline2"` даёт multiline через shell-escape, но `git log` показывает криво. HEREDOC — единственный надёжный способ.
6. **Сабагент делает `mvn package` без `-Dban-junit4-imports.skip=true`.** Enforcer валит сборку. Решение — фиксируй команду в промпте.
7. **Фоновые сабагенты без commit-а.** Если сабагент упал без commit-а, его файлы в `git status` — но как их различить от твоих собственных изменений? Решение — каждый сабагент должен коммитить ВСЕ свои изменения перед завершением.
8. **`--amend` и `--no-verify`** — запрещены (см. `git workflow` в CLAUDE.md).
9. **Race condition при чтении файлов.** Сабагент A читает `lib.rs`, видит старую версию. Сабагент B уже обновил `lib.rs`. A не знает про новые модули. Решение — сабагенты не должны зависеть от состояния файлов вне их scope.
10. **Промпт без verification section.** Сабагент "сделал" — но как проверить? Явные команды verification (`cargo test`, `mvn compile -o`) + критерии "успеха" обязательны.

## Файлы-референсы

- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/IMPLEMENTATION_PLAN.md` — поэтапный план с зависимостями
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/AGENTS.md` — общий контекст (читают все сабагенты)
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/CHANGES.md` — что уже сделано по версиям
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/CONTRIBUTING.md` — как добавлять фичи (для согласования коммитов)
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/core/continuous-agent-loop/SKILL.md` — базовый скилл для управления длинными цепочками сабагентов
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/core/verification-loop/SKILL.md` — скилл verification (build → test → security check)
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/core/tdd-workflow/SKILL.md` — TDD для новых модулей
