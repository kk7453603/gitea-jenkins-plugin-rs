---
name: maven-cargo-integration
description: Интеграция Maven с Cargo для hybrid JVM/native проектов — exec-maven-plugin запускает cargo build, maven-resources-plugin копирует .so в META-INF/native/. Применять когда в pom.xml нужно вызвать cargo build из Maven lifecycle, или для skip-флагов при Docker-сборке.
origin: gitea-jenkins-plugin-rs v1.1.0
tags: [maven, cargo, rust, build, pom, jenkins-plugin]
---

# Maven-Cargo integration: сборка Rust из Maven lifecycle

## Когда применять

- Hybrid проект: `pom.xml` + соседний `rust/` crate с `crate-type = ["cdylib"]`
- Нужно, чтобы `mvn package` автоматически собирал `.so`/`.dylib`/`.dll`
- В Docker .so уже собран — Maven не должен вызывать Cargo повторно
- Enforcer родительского POM валит сборку (JUnit4, deprecated APIs) — нужны skip-флаги

## Паттерн

### Два `<execution>` блока в pom.xml

Стандартная конфигурация:

```xml
<build>
  <plugins>
    <!-- 1. Запускаем `cargo build --release` на фазе generate-resources -->
    <plugin>
      <groupId>org.codehaus.mojo</groupId>
      <artifactId>exec-maven-plugin</artifactId>
      <version>3.3.0</version>
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
        <execution>
          <id>cargo-test</id>
          <phase>test</phase>
          <goals><goal>exec</goal></goals>
          <configuration>
            <executable>cargo</executable>
            <workingDirectory>${project.basedir}/rust/gitea-client</workingDirectory>
            <arguments>
              <argument>test</argument>
            </arguments>
          </configuration>
        </execution>
      </executions>
    </plugin>

    <!-- 2. Копируем .so в META-INF/native/linux/amd64/ на фазе process-resources -->
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
                <includes>
                  <include>libgitea_rust.so</include>
                </includes>
              </resource>
            </resources>
          </configuration>
        </execution>
      </executions>
    </plugin>
  </plugins>
</build>
```

### Фазы Maven (порядок имеет значение)

| Phase | Что происходит |
|---|---|
| `generate-resources` | `cargo build --release` — собирает `.so` |
| `process-resources` | `maven-resources-plugin` копирует `.so` в `target/classes/META-INF/native/...` |
| `compile` | Java компиляция |
| `test` | `cargo test` + JUnit smoke-тесты |
| `package` | `maven-hpi-plugin` упаковывает в `.hpi` |

### Skip-флаги для CI/Docker

Когда `.so` уже собран (например, в Docker multi-stage — см. `docker-rust-jenkins-multi-stage/SKILL.md`), Maven не должен вызывать Cargo:

```bash
mvn -B clean package \
    -DskipTests \
    -Dexec.skip=true \                         # ← не вызывать cargo build
    -Dcheckstyle.skip=true \
    -Dspotbugs.skip=true \
    -Dban-junit4-imports.skip=true             # ← JUnit4 в smoke-тесте (см. ниже)
```

| Skip-флаг | Что отключает | Когда использовать |
|---|---|---|
| `-Dexec.skip=true` | `exec-maven-plugin` (cargo build + cargo test) | Docker multi-stage, локально если .so уже собран |
| `-DskipTests=true` | JUnit + surefire | Быстрая пересборка |
| `-Dcheckstyle.skip=true` | Checkstyle lint | Docker (он ничего не знает про наш конфиг) |
| `-Dspotbugs.skip=true` | SpotBugs статический анализ | Docker |
| `-Dban-junit4-imports.skip=true` | Enforcer, блокирующий JUnit4 imports | Всегда при сборке, пока smoke-тест на JUnit4 (см. ниже) |

### Проблема с JUnit4 enforcer

Родительский POM `org.jenkins-ci.plugins:plugin` включает enforcer правило `ban-junit4-imports`, чтобы форсить JUnit5. Smoke-тест `RustGiteaConnectionSmokeTest` — JUnit4 (исторически). Решение: всегда передавать `-Dban-junit4-imports.skip=true`:

```bash
# Полная команда из AGENTS.md "Команды разработчика":
mvn -B clean package -DskipTests -Dban-junit4-imports.skip=true
```

Альтернатива — переписать smoke-тест на JUnit5 (`@Test` из `org.junit.jupiter` вместо `org.junit`). Это в TODO.

### Без Docker — типичный dev-loop

```bash
# 1. Меняем Rust → cargo test (быстро, ~30 сек)
cd rust/gitea-client && cargo test

# 2. Меняем Java → mvn compile (быстро с -o offline)
mvn compile -DskipTests -Dban-junit4-imports.skip=true -o
# -o = offline, не лезет в repo.jenkins-ci.org (там и так всё скачано)

# 3. Полный end-to-end → docker compose build && up
```

### Smoke-тест с `Assume.assumeTrue`

`RustGiteaConnectionSmokeTest` пропускается автоматически если `.so` не собран:

```java
public class RustGiteaConnectionSmokeTest {
    @Before
    public void setUp() {
        Assume.assumeTrue("libgitea_rust not built — run `cargo build --release`",
                isNativeLibAvailable());
    }

    private static boolean isNativeLibAvailable() {
        try {
            NativeLibraryLoader.load("gitea_rust");
            return true;
        } catch (UnsatisfiedLinkError e) {
            return false;
        }
    }
}
```

Поэтому `mvn test` без `.so` не падает, а пропускает — это удобно для быстрого `mvn compile`.

### `exec.skip` vs ручной копирование в Docker

В Docker multi-stage (см. `docker-rust-jenkins-multi-stage/SKILL.md`) Cargo уже собрал `.so` на отдельной stage. Maven-плагин должен быть отключён (`-Dexec.skip=true`), а `.so` кладётся в `.hpi` через `jar uf` после `mvn package`. **Без `-Dexec.skip=true` Maven попытается запустить `cargo build` в Docker, что приведёт либо к ошибке (нет Cargo), либо к пересборке с другими флагами.**

## Подводные камни

1. **`exec-maven-plugin` в Docker.** Без `-Dexec.skip=true` Maven вызывает `cargo build`, но Docker stage `maven:3.9-eclipse-temurin-21` не содержит Rust toolchain → сборка падает. Решение — всегда skip в Dockerfile.
2. **`workingDirectory` должен указывать на crate root.** `${project.basedir}/rust/gitea-client` — это место с `Cargo.toml`. Если указать только `rust/`, Cargo не найдёт манифест.
3. **`maven-resources-plugin` версию НЕ указываем.** Родительский POM Jenkins-а уже задаёт версию. Если указать свою — конфликт версий с другими plugin-ами.
4. **`outputDirectory` для ресурсов.** `${project.build.outputDirectory}/META-INF/native/linux/amd64` = `target/classes/META-INF/native/linux/amd64`. Maven дальше упаковывает `target/classes` в `.hpi` (или `.jar`). **Но `maven-hpi-plugin` дропает binaries** — поэтому в Docker `.so` инъектируется через `jar uf` после `package`.
5. **Версия `exec-maven-plugin` 3.3.0.** Более старые (1.x) имеют баги с наследованием `<execution>` в multi-module. 3.3.0 стабилен.
6. **Тесты Rust + тесты Java на одной фазе `test`.** `cargo-test` execution на фазе `test` запускается до surefire. Если Rust-тесты падают — Java-тесты не запустятся. Это удобно для fail-fast, но медленно для dev-loop (можно `-Dexec.skip=true` временно).
7. **Enforcer и JUnit4.** Если в `mvn package` нет `-Dban-junit4-imports.skip=true`, enforcer падает с ошибкой о JUnit4 imports в smoke-тесте. Это баг-или-фича: либо мигрируй на JUnit5, либо skip.
8. **`-o` offline mode.** После первого `mvn package` (он скачает всё из `repo.jenkins-ci.org`), последующие `mvn compile -o` работают offline — это намного быстрее и не зависит от flaky mirror.
9. **`.mvn/maven.config`.** Можно положить флаги по умолчанию в `.mvn/maven.config` (одна строка — один флаг). Например `-Dban-junit4-imports.skip=true`. Тогда `mvn package` подхватит их автоматически.

## Файлы-референсы

- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/pom.xml` — полный `exec-maven-plugin` + `maven-resources-plugin` конфиг
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/docker/Dockerfile` — пример использования `-Dexec.skip=true` и других skip-флагов
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/src/test/java/org/jenkinsci/plugin/gitea/client/impl/RustGiteaConnectionSmokeTest.java` — smoke-тест с `Assume.assumeTrue`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/rust/gitea-client/Cargo.toml` — `crate-type = ["cdylib", "rlib"]`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/AGENTS.md` — раздел "Команды разработчика"
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/patterns/docker-rust-jenkins-multi-stage/SKILL.md` — смежный паттерн Docker
