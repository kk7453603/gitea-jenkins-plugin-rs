# syntax=docker/dockerfile:1.7

# Force x86-64 architecture across all stages — Apple Silicon Docker defaults
# to arm64, but production Jenkins controllers are linux/amd64. The .so must
# match the JVM architecture or UnsatisfiedLinkError is thrown at class-load.
FROM rust:1.86-slim-bookworm AS rust-builder
WORKDIR /build

# Install build dependencies needed by some crates (openssl headers, pkg-config, cc)
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy entire crate — the previous stub-then-rebuild pattern left an empty
# lib.rs that confused cargo's incremental build, producing a 71 KB stub
# instead of the real 5 MB .so. Single COPY + single cargo build.
COPY rust/gitea-client/ ./
RUN cargo build --release \
   && test -s target/release/libgitea_rust.so \
   && ls -la target/release/libgitea_rust.so
# Result: /build/target/release/libgitea_rust.so (~5 MB, 40 JNI symbols)

# === Stage 2: build .hpi with Maven ===
FROM maven:3.9-eclipse-temurin-21 AS plugin-builder
WORKDIR /build

COPY pom.xml .
COPY src ./src
COPY .mvn ./.mvn

# Maven would normally invoke `cargo build` via exec-maven-plugin at the
# generate-resources phase. We skip that (-Dexec.skip=true) because the
# native library was already produced in Stage 1. After `mvn package`
# we inject the .so into the .hpi under WEB-INF/classes/ — maven-hpi-plugin
# drops binary resources from src/main/resources and target/classes, so we
# use `jar uf` to force it in.
COPY --from=rust-builder /build/target/release/libgitea_rust.so /tmp/libgitea_rust.so
RUN mkdir -p /staging/WEB-INF/classes/META-INF/native/linux/amd64 \
    && cp /tmp/libgitea_rust.so /staging/WEB-INF/classes/META-INF/native/linux/amd64/libgitea_rust.so

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
   && (cd /staging && jar uf /build/target/gitea.hpi WEB-INF/classes/META-INF/native/linux/amd64/libgitea_rust.so) \
   && jar tf /build/target/gitea.hpi | grep -E '\.so$'
# Result: /build/target/gitea.hpi

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
