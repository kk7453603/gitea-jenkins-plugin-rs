---
name: serviceloader-native-replacement
description: Замена Default* Java-имплементаций на Rust/JNI-backed через ServiceLoader SPI без правки 95% кодовой базы. Применять когда надо подменить реализацию интерфейса, сохранить binary compatibility, или когда в проекте уже есть SPI но нужно поставить native backend.
origin: gitea-jenkins-plugin-rs v1.1.0
tags: [java, spi, serviceloader, jni, native, architecture, binary-compatibility]
---

# ServiceLoader native replacement: подмена реализации через SPI

## Когда применять

- Нужно заменить `DefaultConnectionFactory` на `RustConnectionFactory` без правки 95% кодовой базы
- Проект уже использует `ServiceLoader<Spi>` (это обязательное условие — иначе нужно вводить SPI)
- Нужна binary compatibility (все старые клиенты интерфейса продолжают работать)
- Хочется минимизировать diff с upstream для удобного rebase-а
- Native backend (Rust/JNI) подключается через тот же интерфейс

## Паттерн

### Шаг 0: проверить, что SPI уже есть

В upstream Jenkins Gitea plugin уже был SPI:

```
src/main/resources/META-INF/services/
└── org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory
    └── содержимое: org.jenkinsci.plugin.gitea.client.impl.DefaultGiteaConnectionFactory
```

**Признак готового SPI:** существует интерфейс `GiteaConnectionFactory` и upstream уже регистрирует через `META-INF/services/`. Mock-фабрика (`MockGiteaConnectionFactory`) в upstream tests — дополнительное доказательство, что SPI точка расширения.

Если SPI нет — придётся ввести его (это рефакторинг, выходящий за scope этого skill-а).

### Шаг 1: создать новую имплементацию

`RustGiteaConnectionFactory` — единственный файл, реализующий upstream interface:

```java
package org.jenkinsci.plugin.gitea.client.impl;

import org.jenkinsci.plugin.gitea.client.api.GiteaAuth;
import org.jenkinsci.plugin.gitea.client.api.GiteaConnection;
import org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory;

/**
 * {@link GiteaConnectionFactory} backed by the native Rust client.
 *
 * <p>Registered through {@code META-INF/services/...GiteaConnectionFactory}
 * and picked up by Java's {@link java.util.ServiceLoader} at runtime.</p>
 */
public class RustGiteaConnectionFactory implements GiteaConnectionFactory {

    @Override
    public GiteaConnection open(String serverUrl, GiteaAuth auth) {
        return new RustGiteaConnection(serverUrl, auth);
    }
}
```

`RustGiteaConnection` — JNI-shim, реализующий тот же интерфейс `GiteaConnection` (см. `json-over-jni-bridge/SKILL.md`).

### Шаг 2: заменить строку в `META-INF/services/`

```
# src/main/resources/META-INF/services/org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory
# Старое содержимое:
# org.jenkinsci.plugin.gitea.client.impl.DefaultGiteaConnectionFactory
# Новое содержимое:
org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnectionFactory
```

**Один файл с одной строкой.** ServiceLoader при следующем старте Jenkins подхватит новую реализацию.

### Шаг 3: удалить старую имплементацию (опционально)

Если `DefaultGiteaConnectionFactory` больше не нужна — удаляем `.java` файл. Если может понадобиться для тестов/фоллбэка — оставляем, но убираем registration из `META-INF/services/`.

В нашем случае мы её оставили — это позволяет легко переключаться между Rust и Java имплементациями для отладки.

### Что НЕ поменялось

- `GiteaConnection` интерфейс (35 методов) — нетронут
- 41 POJO в `client/api/` — нетронуты
- `GiteaServer`, `GiteaServers` (конфиг) — нетронуты
- Все SCM traits (`GiteaBrowser`, `GiteaSCMSource`, ...) — нетронуты
- Все SCM events (`GiteaPushSCMEvent`, ...) — нетронуты
- Вебхук-handlers — нетронуты (на уровне `GiteaSCMSource` они получают события через `SCMHeadEvent.fireNow()`, который запускает наш RustWebhookDispatcher)

**Итог: заменены 2 файла (Default* → Rust*), добавлены ~5 новых (RustGiteaConnection, RustGiteaConnectionFactory, NativeLibraryLoader, + Rust crate), изменена 1 строка в services/. 95 Java-классов — нетронуты.**

### SPI в Java — как это работает

```java
// ServiceLoader сканирует classpath, ищет все файлы
// META-INF/services/<Interface>, читает FQCN из каждой строки,
// инстанцирует через no-arg конструктор.
ServiceLoader<GiteaConnectionFactory> loader =
        ServiceLoader.load(GiteaConnectionFactory.class);
for (GiteaConnectionFactory factory : loader) {
    // В нашем случае — только одна: RustGiteaConnectionFactory
    return factory.open(serverUrl, auth);
}
```

Jenkins при старте плагина:
1. Загружает плагин ClassLoader
2. Сканирует `META-INF/services/` внутри `.hpi`
3. Находит `RustGiteaConnectionFactory`
4. Вызывает `Class.forName(...).getDeclaredConstructor().newInstance()`
5. Внутри `<clinit>` `RustGiteaConnection` грузит `.so` через `NativeLibraryLoader`
6. Все 35 native methods доступны через `GiteaConnection` interface

### Multi-implementation (если нужно)

ServiceLoader может вернуть несколько implementations. Например если оставить и Default, и Rust — обе появятся в iteration. Решение — `Provider<>.stream().filter(...)` или приоритизация через отдельный marker interface.

В нашем случае — одна реализация, простой `iterator().next()`.

## Подводные камни

1. **SPI файл должен быть UTF-8.** Не ASCII, не Latin-1. Особенно критично если в FQCN есть не-ASCII символы (редкость, но всё же).
2. **Одна строка — одна имплементация.** Несколько строк = несколько implementations. Не путайте с `META-INF/MANIFEST.MF` (там key=value).
3. **Classpath visibility.** ServiceLoader использует текущий Thread's context ClassLoader. В Jenkins это plugin ClassLoader — OK. В тестах (surefire) — system ClassLoader, может не увидеть plugin classes. Решение — тестировать через `mvn hpi:run`.
4. **`<clinit>` lazy loading.** `RustGiteaConnectionFactory.class.newInstance()` триггерит `<clinit>` `RustGiteaConnection`, который грузит `.so`. Если `.so` нет — `UnsatisfiedLinkError` падает при первом обращении к `factory.open(...)`, а не при старте Jenkins. Это удобно для dev (плагин не падает при отсутствии `.so`), но плохо для prod (тихое падение при первом webhook-е).
5. **`MockGiteaConnectionFactory` в upstream.** Её existence в upstream tests — доказательство, что SPI готов к extension. Если в upstream нет mock — нужно ввести SPI осторожно (через deprecate Default + introduce abstract Factory).
6. **NativeLibraryLoader должен вызываться ДО первого native method.** Это делает `<clinit>` `RustGiteaConnection`. Но если кто-то вызовет `nativeFetchVersion` через рефлексию (минуя `Class.forName(...)`) — `.so` может быть не загружен. Решение — `<clinit>` гарантированно срабатывает при первом обращении к классу из любого пути.
7. **Не делайте SPI файл generated.** Maven `annotations`-processor может генерировать `META-INF/services/` автоматически. Это удобно, но затрудняет отладку (файл не виден в IDE). В нашем случае — ручной файл, проще для понимания.
8. **Binary compatibility.** Любое изменение `GiteaConnection` интерфейса ломает binary compat — старые плагины, скомпилированные против предыдущей версии, упадут с `AbstractMethodError`. Решение — только `default` methods в интерфейсах, никаких новых abstract methods.
9. **`@Indexed` vs `META-INF/services/`.** Spring 5+ использует `META-INF/spring.factories` или `@Indexed` аннотации. Это **не** ServiceLoader. Для Jenkins-плагина — только стандартный ServiceLoader.
10. **Two plugins, same SPI.** Если два Jenkins-плагина регистрируют одну SPI implementation, ServiceLoader вернёт обе — порядок не гарантирован. Решение — уникальный интерфейс для каждого плагина, или priority-через `@ServiceProvider(priority=N)` (не стандарт, требует доп. библиотеки).

## Файлы-референсы

- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/resources/META-INF/services/org.jenkinsci.plugin.gitea.client.spi.GiteaConnectionFactory` — ОДНА строка: `org.jenkinsci.plugin.gitea.client.impl.RustGiteaConnectionFactory`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/client/impl/RustGiteaConnectionFactory.java` — новая имплементация SPI
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/client/impl/RustGiteaConnection.java` — JNI-shim, реализующий upstream `GiteaConnection`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/client/spi/GiteaConnectionFactory.java` — upstream SPI interface (НЕ ТРОГАТЬ)
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/AGENTS.md` — раздел "Архитектура (TL;DR)" — обоснование "95 классов нетронуты"
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/patterns/native-library-loader/SKILL.md` — смежный паттерн про загрузку `.so`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/patterns/json-over-jni-bridge/SKILL.md` — смежный паттерн про JSON boundary
