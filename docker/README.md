# Jenkins + custom Gitea plugin (Rust/JNI) — Docker setup

This directory builds a Jenkins LTS image with the custom Gitea plugin
(`gitea.hpi`) baked in, alongside the Multibranch Pipeline plugin set.

The build is a 3-stage multi-stage Dockerfile so that the native Rust core
(`libgitea_rust.so`) is produced inside a Linux container — even when you
build the image on macOS.

## Build stages

1. **rust-builder** — `rust:1.82-slim-bookworm`, compiles `rust/gitea-client`
   in `--release` mode and emits `libgitea_rust.so`.
2. **plugin-builder** — `maven:3.9-eclipse-temurin-21`, runs `mvn package`
   with `-Dexec.skip=true` so `cargo` is not re-invoked; the `.so` is copied
   to the exact path `maven-resources-plugin` expects.
3. **runtime** — `jenkins/jenkins:lts-jdk21`, installs the `.hpi` as
   `/usr/share/jenkins/ref/plugins/gitea.jpi` (pinned) and pulls the rest of
   the plugin set via `jenkins-plugin-cli`.

## Build

```bash
docker compose build
```

Expect 10–15 minutes for a clean build (Rust release build + Maven
dependencies). Subsequent builds reuse the Docker layer cache.

## Run

```bash
docker compose up -d
```

Jenkins is ready when `docker compose logs jenkins` shows
`Jenkins is fully up and running` (typically 1–2 minutes).

## First login

```bash
docker compose exec jenkins cat /var/jenkins_home/secrets/initialAdminPassword
```

Open http://localhost:8080 and paste the password. The Gitea plugin is
already installed — skip the "Install suggested plugins" step and go
straight to **New Item → Multibranch Pipeline**.

## Verify the Gitea plugin is installed

```bash
docker compose exec jenkins ls /var/jenkins_home/plugins/ | grep gitea
```

You should see `gitea.jpi` (and the unpacked `gitea/` directory after the
first boot).

## Notes

- The custom Gitea plugin is **pinned** (`.jpi.pinned`) so Jenkins will not
  overwrite it from the update center.
- `plugins.txt` intentionally omits `gitea` — it ships from this build.
- The `jenkins_home` volume is named `jenkins_home` and persists across
  restarts. Drop it with `docker compose down -v` to start fresh.
