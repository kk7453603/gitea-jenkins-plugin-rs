# syntax=docker/dockerfile:1.7

# === Stage 1: build Rust core (libgitea_rust.so) ===
FROM rust:1.86-slim-bookworm AS rust-builder
WORKDIR /build

# Install build dependencies needed by some crates (openssl headers, pkg-config, cc)
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Cache dependencies separately by compiling a lib stub first. Cargo.toml
# declares a cdylib+rlib target, so we need src/lib.rs (not main.rs).
COPY rust/gitea-client/Cargo.toml rust/gitea-client/Cargo.lock* ./
RUN mkdir -p src && echo "" > src/lib.rs && cargo build --release || true

# Now the real source
COPY rust/gitea-client/src ./src
COPY rust/gitea-client/tests ./tests
RUN cargo build --release
# Result: /build/target/release/libgitea_rust.so

# === Stage 2: build .hpi with Maven ===
FROM maven:3.9-eclipse-temurin-21 AS plugin-builder
WORKDIR /build

COPY pom.xml .
COPY src ./src
COPY .mvn ./.mvn

# Maven would normally invoke `cargo build` via exec-maven-plugin at the
# generate-resources phase. We skip that (-Dexec.skip=true) because the
# native library was already produced in Stage 1. We pre-place it where
# maven-resources-plugin expects it.
RUN mkdir -p rust/gitea-client/target/release
COPY --from=rust-builder /build/target/release/libgitea_rust.so rust/gitea-client/target/release/

# Build the plugin, skipping tests and the cargo execution.
RUN mvn -B --no-transfer-progress clean package \
        -DskipTests \
        -Dexec.skip=true \
        -Dcheckstyle.skip=true \
        -Dspotbugs.skip=true \
        -Dban-junit4-imports.skip=true
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
