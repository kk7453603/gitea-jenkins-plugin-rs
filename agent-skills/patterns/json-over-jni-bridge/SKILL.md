---
name: json-over-jni-bridge
description: Архитектурный boundary — Rust возвращает JSON-строку, Java парсит Jackson в существующие POJO. Не дублируем модели. Применять когда нужно добавить новый метод в GiteaConnection, или когда хочется завести новый POJO в client/api/.
origin: gitea-jenkins-plugin-rs v1.1.0
tags: [architecture, jni, json, jackson, boundary, pojo]
---

# JSON-over-JNI: boundary между Rust и Java

## Когда применять

- Нужно добавить новый метод в `GiteaConnection` (а значит и в Rust `GiteaClient`, и в JNI export)
- Появилось искушение создать новый POJO в `client/api/` — **НЕ создайте**, переиспользуйте существующий
- Тестируете round-trip JSON → Rust → JSON → Java POJO
- Видите расхождение snake_case/camelCase в полях

## Паттерн

Главное архитектурное решение (см. AGENTS.md): **Rust отдаёт JSON-строку, Java парсит Jackson**. Не дублируем 41 POJO из upstream.

### Pipeline вызова

```
Java caller
   │
   ▼
RustGiteaConnection.fetchRepository(String, String)   ← Java
   │
   │ calls  private static native String nativeFetchRepository(...)
   ▼
Java_org_jenkinsci_plugin_gitea_client_impl_RustGiteaConnection_nativeFetchRepository
   │                                                     ← Rust JNI export
   │ RT.block_on(client.fetch_repository(owner, repo))
   ▼
GiteaClient::fetch_repository()  →  serde_json::to_string(&response_json)
   │
   ▼  returns jstring (raw JSON)
RustGiteaConnection.fetchRepository(...)
   │
   │ parseObject(json, GiteaRepository.class)
   ▼
ObjectMapper.readerFor(GiteaRepository.class).readValue(json)
   │
   ▼
GiteaRepository POJO (с @JsonProperty аннотациями от upstream)
```

### Java-side хелперы

`RustGiteaConnection` держит один статический `ObjectMapper` и 3 приватных хелпера:

```java
private static final ObjectMapper MAPPER = new ObjectMapper();

private <T> T parseObject(String json, Class<T> type) throws IOException {
    return MAPPER.readerFor(type).readValue(json);
}

private <T> List<T> parseList(String json, Class<T> elementType) throws IOException {
    return MAPPER.readerForListOf(elementType).readValue(json);
}

private String toJson(Object value) throws IOException {
    return MAPPER.writeValueAsString(value);
}
```

Пример использования в `fetchRepository`:

```java
@Override
public GiteaRepository fetchRepository(String username, String name) throws IOException, InterruptedException {
    return parseObject(
            nativeFetchRepository(serverUrl, authType, authSecret, username, name),
            GiteaRepository.class);
}
```

### Rust-side сериализация

`GiteaClient` собирает response от Gitea как `serde_json::Value` (или свой `serde::Deserialize` struct), потом `serde_json::to_string()`:

```rust
pub async fn fetch_repository(&self, owner: &str, repo: &str) -> Result<String, GiteaError> {
    let url = format!("{}/api/v1/repos/{}/{}", self.base_url, encoded(owner), encoded(repo));
    let response = self.http_get(&url).await?;
    let json: serde_json::Value = serde_json::from_str(&response)?;
    Ok(serde_json::to_string(&json)?)
}
```

**Ключевой момент:** Rust НЕ знает про Java POJO. Он просто проксирует JSON. Любая валидация/фильтрация — на стороне Jackson.

### snake_case ↔ camelCase

Gitea API шлёт snake_case (`"full_name"`, `"default_branch"`), Jackson POJO от upstream — camelCase с `@JsonProperty`:

```java
public class GiteaRepository {
    @JsonProperty("full_name")
    private String fullName;

    @JsonProperty("default_branch")
    private String defaultBranch;
    // ...
}
```

Rust-side `serde_json::Value` проходит через Jackson без изменений — Jackson сам применяет `@JsonProperty`. **Не дублируйте аннотации в Rust.**

### Pagination и merge-arrays

Rust собирает все страницы и конкатенирует JSON-массивы до возвращения:

```rust
// fetch_pull_requests — пагинация по Link: <...>; rel="next"
pub async fn fetch_pull_requests(...) -> Result<String, GiteaError> {
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut url = Some(first_page_url);
    while let Some(u) = url {
        let (page, next) = self.http_get_paginated(&u).await?;
        let arr: Vec<serde_json::Value> = serde_json::from_str(&page).unwrap_or_default();
        all.extend(arr);
        url = next;
    }
    // Удаляем null (Gitea иногда шлёт "null" в массиве)
    all.retain(|v| !v.is_null());
    Ok(serde_json::to_string(&all)?)
}
```

Java-side видит просто JSON-массив, парсит через `parseList(json, GiteaPullRequest.class)`.

### 404 → пустой массив / FileNotFound

Разные endpoints по-разному реагируют на 404:

```rust
match status {
    404 if endpoint_returns_array => return Ok("[]".to_string()),  // для fetchPullRequests/Issues/Releases
    404 if endpoint_returns_file  => return Err(GiteaError::FileNotFound(path)),  // для fetchFile
    // ...
}
```

Это зафиксировано в AGENTS.md "Согласованные архитектурные решения".

### Кодирование запросов (input → Rust)

Для non-trivial аргументов Java-side сериализует POJO в JSON и передаёт как `String`:

```java
// fetchIssues принимает Set<GiteaIssueState>, передаём как строку-фильтр
@Override
public List<GiteaIssue> fetchIssues(GiteaRepository repository, Set<GiteaIssueState> states) throws IOException, InterruptedException {
    String stateKey = singleStateKey(states);  // "open", "closed", или null
    return parseList(
            nativeFetchIssues(serverUrl, authType, authSecret,
                    repoOwnerUsername(repository), repository.getName(),
                    stateKey == null ? "" : stateKey),
            GiteaIssue.class);
}
```

См. также `nativeSetProxy(String configJson)` и `nativeStartPolling(String configJson)` — для сложных конфигов Java-side собирает JSON-документ (через Jackson), Rust парсит через `serde_json::from_str`.

## Подводные камни

1. **НЕ создавайте новый POJO в `client/api/`.** Это зафиксировано в AGENTS.md как anti-pattern. Если Gitea прислал новое поле — добавьте `@JsonProperty` к существующему POJO, не создавайте новый. Rust-side просто проксирует JSON, изменений не требуется.
2. **`null` в массивах.** Gitea иногда шлёт `[null, {...}, null]` для disabled repos. Rust-side `.retain(|v| !v.is_null())` обязателен, иначе Jackson упадёт на `null` элементе.
3. **Pagination `Link:` header.** Не парсьте вручную — используйте `reqwest::Response::headers().get("link")`. Формат: `<url1>; rel="next", <url2>; rel="last"`. Rust-side должен следовать `next` пока не исчезнет.
4. **404 handling по endpoint-у.** Не делайте единый `if status == 404 { return Err(...) }`. Для `fetch_pull_requests/Issues/Releases` 404 = endpoint выключен на сервере → вернуть `[]`. Для `fetch_file` 404 = `FileNotFound`. Для `fetch_repository` 404 = `HttpStatusException`. См. AGENTS.md таблицу "Согласованные архитектурные решения".
5. **`jstring` для больших ответов.** JSON-ответ со списком 1000 issues — это ~1MB строка. JNI-копирование через `new_string` + `get_string` занимает ~5-10ms. Не проблема для типичного API-вызова, но не делайте этого для стриминга больших файлов (для них есть `nativeFetchFile` возвращающий `jbyteArray`).
6. **`ObjectMapper` потокобезопасный для чтения.** Один статический инстанс на `RustGiteaConnection` — OK. Не создавайте на каждый вызов.
7. **Encoding аргументов.** URL-path сегменты кодируются `percent_encode_path_segment` в Rust. Этот encoding НЕ кодирует `.` (Gitea-совместимость). См. AGENTS.md.
8. **Двойной fetch для `fetchOwner`.** Сначала `GET /api/v1/orgs/{name}`, при 404 — `GET /api/v1/users/{name}`. Это поведение upstream, Rust-side должен его повторить (см. `client.rs::fetch_owner`).

## Файлы-референсы

- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/client/impl/RustGiteaConnection.java` — Java-side: `parseObject`/`parseList`/`toJson`, статический `MAPPER`, все методы `GiteaConnection`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/client.rs` — Rust-side: `GiteaClient` с 33+ async методами, каждый возвращает `Result<String, GiteaError>`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/src/jni.rs` — JNI export-ы, мост между `client.rs` и `RustGiteaConnection.java`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/main/java/org/jenkinsci/plugin/gitea/client/api/` — 41 upstream POJO с `@JsonProperty` snake_case аннотациями (НЕ ТРОГАТЬ)
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/patterns/jni-bridge-generator/SKILL.md` — смежный паттерн про JNI naming + хелперы
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/AGENTS.md` — раздел "Согласованные архитектурные решения"
