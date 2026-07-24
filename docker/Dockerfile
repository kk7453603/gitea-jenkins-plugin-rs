# syntax=docker/dockerfile:1.7

# =============================================================================
# Multi-arch build: produce .so for both linux/amd64 AND linux/aarch64.
# Both libraries end up in the same .hpi (under META-INF/native/linux/{amd64,
# aarch64}/) and NativeLibraryLoader picks the right one at runtime based on
# the JVM's os.arch property. This lets the same plugin jar run on either
# Jenkins controller architecture.
# =============================================================================

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

# === Stage 2: build .hpi with Maven ===
FROM maven:3.9-eclipse-temurin-21 AS plugin-builder
WORKDIR /build

COPY pom.xml .
COPY src ./src
COPY .mvn ./.mvn

# Both architectures' .so land in their respective META-INF/native/linux/<arch>/
# directories inside the .hpi. NativeLibraryLoader.load() selects at runtime.
COPY --from=rust-builder-amd64  /build/target/release/libgitea_rust.so /tmp/amd64/libgitea_rust.so
COPY --from=rust-builder-arm64  /build/target/release/libgitea_rust.so /tmp/arm64/libgitea_rust.so
RUN mkdir -p /staging/WEB-INF/classes/META-INF/native/linux/amd64 \
             /staging/WEB-INF/classes/META-INF/native/linux/aarch64 \
    && cp /tmp/amd64/libgitea_rust.so /staging/WEB-INF/classes/META-INF/native/linux/amd64/libgitea_rust.so \
    && cp /tmp/arm64/libgitea_rust.so /staging/WEB-INF/classes/META-INF/native/linux/aarch64/libgitea_rust.so

# Build the plugin, skipping tests and the cargo execution.
# Maven retry + IPv4 + cache mount defend against flaky Jenkins-ci.org mirror.
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

# === Stage 3: Jenkins runtime ===
FROM jenkins/jenkins:lts-jdk21

# Custom plugin is placed in ref/plugins so Jenkins picks it up on first start.
# We name it .jpi (the "installed" extension) so it is not overwritten by
# anything pulled from the update center. The base image ships ref/ without a
# plugins/ subdirectory and jenkins-plugin-cli expects it to be owned by UID
# 1000, so create it and lay down our pinned plugin as root.
USER root
RUN mkdir -p /usr/share/jenkins/ref/plugins
COPY --from=plugin-builder /build/target/gitea.hpi /usr/share/jenkins/ref/plugins/gitea.jpi
RUN echo "gitea" > /usr/share/jenkins/ref/plugins/gitea.jpi.pinned \
    && chown -R 1000:1000 /usr/share/jenkins/ref/plugins

# Remaining plugins come from plugins.txt. jenkins-plugin-cli handles its
# own ownership when run as root.
COPY docker/plugins.txt /usr/share/jenkins/ref/plugins.txt
RUN jenkins-plugin-cli --plugin-file /usr/share/jenkins/ref/plugins.txt \
    && chown -R 1000:1000 /usr/share/jenkins/ref

USER jenkins
