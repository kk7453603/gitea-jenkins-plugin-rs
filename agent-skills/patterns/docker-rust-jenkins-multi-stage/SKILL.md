---
name: docker-rust-jenkins-multi-stage
description: Multi-stage Dockerfile (3+ этапов) для Jenkins-плагина, в котором Rust-ядро поставляется через JNI. Идеально для сборки контейнера с Jenkins + кастомный плагин с нативным кодом. Применять когда в prompt-е встречаются "Dockerize Jenkins plugin with Rust", "multi-stage Maven Rust build", "containerize .hpi with native dependencies", "webhook server port в Jenkins контейнере".
origin: gitea-jenkins-plugin-rs v1.1.0
tags: [docker, multi-stage, rust, jni, jenkins, multi-arch, maven]
---

# Docker: multi-stage сборка Rust+Maven+Jenkins

## Когда применять

Этот паттерн для контейнеризации Jenkins-плагина с Rust-ядром через JNI. Триггеры:
- Пользователь просит "Dockerize my Jenkins plugin", "containerize plugin with Rust"
- Есть директория `rust/` или `native-core/` рядом с `pom.xml` Jenkins-плагина
- Нужен Jenkins контроллер с предустановленным кастомным плагином
- Нужен multi-arch билд (amd64 + arm64 в одном .hpi) без QEMU на Jenkins CI

## Паттерн

3-этапный Dockerfile: 1a+1b Rust под две архитектуры, 2 Maven-сборка .hpi с инъекцией .so, 3 Jenkins-runtime.

### Этапы 1a/1b — отдельный rust-builder под каждую архитектуру

`--platform` фиксирует целевую архитектуру stage, чтобы `cargo build` шёл нативно внутри QEMU или на нативном хосте buildx-а:

```dockerfile
# --- Stage 1a: build .so for linux/amd64 ---
FROM --platform=linux/amd64 rust:1.86-slim-bookworm AS rust-builder-amd64
WORKDIR /build
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY rust/gitea-client/ ./
RUN cargo build --release \
 && test -s target/release/libgitea_rust.so \
 && ls -la target/release/libgitea_rust.so

# --- Stage 1b: build .so for linux/aarch64 ---
FROM --platform=linux/arm64 rust:1.86-slim-bookworm AS rust-builder-arm64
WORKDIR /build
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY rust/gitea-client/ ./
RUN cargo build --release \
 && test -s target/release/libgitea_rust.so \
 && ls -la target/release/libgitea_rust.so
```

Контрольная сумма через `test -s` после `cargo build` — обязательна: пустой `.so` (например, от неудачной cross-сборки) иначе молча попадёт в финальный образ и даст `UnsatisfiedLinkError` в рантайме.

### Этап 2 — Maven-сборка .hpi + инъекция .so

Ключевая хитрость: `maven-hpi-plugin` дропает любые binaries при упаковке .hpi, поэтому `.so` для обеих архитектур надо положить в `WEB-INF/classes/` вручную через `jar uf` уже **после** `mvn package`:

```dockerfile
FROM maven:3.9-eclipse-temurin-21 AS plugin-builder
WORKDIR /build

COPY pom.xml .
COPY src ./src
COPY .mvn ./.mvn

COPY --from=rust-builder-amd64  /build/target/release/libgitea_rust.so /tmp/amd64/libgitea_rust.so
COPY --from=rust-builder-arm64  /build/target/release/libgitea_rust.so /tmp/arm64/libgitea_rust.so
RUN mkdir -p /staging/WEB-INF/classes/META-INF/native/linux/amd64 \
             /staging/WEB-INF/classes/META-INF/native/linux/aarch64 \
    && cp /tmp/amd64/libgitea_rust.so /staging/WEB-INF/classes/META-INF/native/linux/amd64/libgitea_rust.so \
    && cp /tmp/arm64/libgitea_rust.so /staging/WEB-INF/classes/META-INF/native/linux/aarch64/libgitea_rust.so

RUN --mount=type=cache,target=/root/.m2,sharing=locked \
    MAVEN_OPTS="-Djava.net.preferIPv4Stack=true -Dhttp.keepAlive=false" \
    mvn -B --no-transfer-progress clean package \
        -DskipTests \
        -Dexec.skip=true \
        -Dcheckstyle.skip=true \
        -Dspotbugs.skip=true \
        -Dban-junit4-imports.skip=true \
        -Dmaven.wagon.http.retryHandler.count=10 \
        -Dmaven.wagon.http.retryHandler.requestSentEnabled=true \
        -Dmaven.wagon.http.connectionTimeout=30000 \
        -Dmaven.wagon.http.readTimeout=120000 \
   && (cd /staging && jar uf /build/target/gitea.hpi \
        WEB-INF/classes/META-INF/native/linux/amd64/libgitea_rust.so \
        WEB-INF/classes/META-INF/native/linux/aarch64/libgitea_rust.so) \
   && jar tf /build/target/gitea.hpi | grep -E '\.so$'
```

Расшифровка флагов Maven:
- `-Dexec.skip=true` — **обязательно**, потому что `pom.xml` регистрирует `exec-maven-plugin` на фазе `generate-resources`, который вызывает `cargo build`. В Docker .so уже собран на этапе 1, повторный запуск Cargo сломает билд (другой target, другие флаги).
- `-Dban-junit4-imports.skip=true` — родительский POM Jenkins валит сборку из-за JUnit 4 в smoke-тестах плагина.
- `-Dmaven.wagon.http.retryHandler.count=10` + `preferIPv4Stack=true` — `repo.jenkins-ci.org` регулярно отдаёт таймауты/плохие keepalive.
- `--mount=type=cache,target=/root/.m2,sharing=locked` — кэш между билдами, общий для всех параллельных stage-ов.

Финальный `grep -E '\.so$'` — last-mile проверка, что инъекция прошла (его отсутствие = либо build упал, либо `.so` забыт).

### Этап 3 — Jenkins runtime

`jenkins/jenkins:lts-jdk21` — официальный LTS образ. Плагин кладётся в `ref/plugins/` как `.jpi` (это "installed" расширение, его не перепишет update center), и пинится через `.pinned`:

```dockerfile
FROM jenkins/jenkins:lts-jdk21

USER root
RUN mkdir -p /usr/share/jenkins/ref/plugins
COPY --from=plugin-builder /build/target/gitea.hpi /usr/share/jenkins/ref/plugins/gitea.jpi
RUN echo "gitea" > /usr/share/jenkins/ref/plugins/gitea.jpi.pinned \
    && chown -R 1000:1000 /usr/share/jenkins/ref/plugins

COPY docker/plugins.txt /usr/share/jenkins/ref/plugins.txt
RUN jenkins-plugin-cli --plugin-file /usr/share/jenkins/ref/plugins.txt \
    && chown -R 1000:1000 /usr/share/jenkins/ref

USER jenkins
```

`jenkins-plugin-cli` требует UID 1000, поэтому `chown` обязателен. Сам плагин копируется ДО `jenkins-plugin-cli`, чтобы CLI видел его при resolvement зависимостей.

### docker-compose: webhook-порт и объём jenkins_home

```yaml
services:
  jenkins:
    build:
      context: .
      dockerfile: docker/Dockerfile
    image: jenkins-gitea-rust:local
    ports:
      - "8080:8080"    # Jenkins UI
      - "50000:50000"  # JNLP agents
      - "8081:8081"    # Rust webhook server (axum, отдельный listener)
    environment:
      - JAVA_OPTS=-Djenkins.install.runSetupWizard=false
    volumes:
      - jenkins_home:/var/jenkins_home
    restart: unless-stopped

volumes:
  jenkins_home:
```

## Подводные камни

1. **`jar uf` обязательно.** `maven-hpi-plugin` упаковывает только ресурсы, которые он считает ресурсами плагина. Любой `.so` в `target/classes/META-INF/native/...` будет потерян. Это 4-я попытка заставить multi-arch работать.
2. **Не запускайте `cargo build` в Maven stage.** Даже если Cargo рядом — pass `-Dexec.skip=true`, иначе получите смесь native libs из двух сборок.
3. **Flaky mirror `repo.jenkins-ci.org`.** Без `retryHandler.count=10` + `preferIPv4Stack` билд падает в ~30% случаев с `Connection reset`. IPv6 закомментирован потому что внутренний docker-bridge роняет пакеты.
4. **UID 1000 для `ref/plugins`.** Базовый образ не создаёт `ref/plugins/`, `jenkins-plugin-cli` падает с permission denied, если папка не принадлежит jenkins-юзеру. `chown -R 1000:1000` — после каждого `COPY`.
5. **Multi-arch без QEMU.** Трюк: 2 rust-builder stage, каждый с `--platform=linux/<arch>`, позволят `docker buildx` нативно собирать каждую архитектуру на buildx-node с той же архитектурой (например arm64 builder + amd64 builder). Это **в разы быстрее** чем QEMU-эмуляция.
6. **`.jpi` vs `.hpi` расширения.** `.hpi` — это артефакт сборки (что отдаёт Maven). `.jpi` — это installed-форма (что лежит в `JENKINS_HOME/plugins/`). Переименование делается в Dockerfile, не в Maven.
7. **`test -s` после `cargo build`.** Пустой `.so` (сломанный cross-compile, dead-code elimination) проходит все проверки, пока не даст `UnsatisfiedLinkError` в рантайме Jenkins. Тест на ненулевой размер в Dockerfile = ранняя диагностика.
8. **`deleteOnExit` и `Files.createTempFile`.** `NativeLibraryLoader` копирует `.so` во временную папку; в Docker это `/tmp` контейнера, который живёт столько же сколько контейнер. Если смонтирован `tmpfs` с `noexec`, плагин не загрузится — ставьте `tmpfs` с `exec` или убирайте.

## Файлы-референсы

- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/docker/Dockerfile` — весь 3-stage паттерн целиком
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/docker-compose.yml` — expose :8081, jenkins_home volume
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/docker/plugins.txt` — список upstream-плагинов для `jenkins-plugin-cli`
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/pom.xml` — `exec-maven-plugin` на `generate-resources` (его и skip-аем через `-Dexec.skip=true`)
- `/Users/kirillkom/BIT-projects/GiteaJenkinsPluginRework/agent-skills/patterns/native-library-loader/SKILL.md` — смежный паттерн про `NativeLibraryLoader.java`, который извлекает `.so` из `META-INF/native/linux/{amd64,aarch64}/` в рантайме
