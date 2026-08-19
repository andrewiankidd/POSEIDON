# POSEIDON web instance - multi-stage build for a slim final image.
#
# Stage 1 compiles the server binary (the whole workspace is available, but we
# only build `poseidon-server`). Stage 2 is a minimal Debian slim runtime with
# just the binary, the static frontend bundle, and CA certificates (needed for
# TLS to Azure DevOps). SQLite is statically bundled into the binary via
# libsqlite3-sys, so no database package is installed at runtime.

# Pinned dependency versions. Overridable at build time (`--build-arg`), but the
# defaults are the versions we build + test against, so a plain `docker build` is
# reproducible. Bump these deliberately, not implicitly.
#   RUST_VERSION     - builder/dev base image tag. MUST match rust-toolchain.toml's
#                      pinned channel, so the image ships that toolchain and no
#                      rustup download happens at build time. (This is the build
#                      toolchain, not the crates' MSRV floor - that's Cargo.toml's
#                      `rust-version`.)
#   DEBIAN_RELEASE   - Debian codename for both base images + the Azure CLI apt repo
#   AZURE_CLI_VERSION- pinned `az` package (see the runtime stage)
ARG RUST_VERSION=1.97.1
ARG DEBIAN_RELEASE=bookworm

# ---- Stage 1: build (cargo-chef caches the dependency compile) --------------
# cargo-chef splits the build so third-party dependencies compile in a layer keyed
# only by Cargo.toml/Cargo.lock. An app-code change then recompiles only our own
# crates (~seconds) instead of the whole dependency tree (~minutes) - which is what
# makes `poseidon.sh up`/`verify` and `skaffold dev` fast on every edit.
FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS chef
RUN cargo install cargo-chef --locked
WORKDIR /build

FROM chef AS planner
# Only the Cargo manifests + crate sources drive the dependency recipe, so a
# frontend/docs edit never reruns this (or busts the cook layer below).
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# Cook ONLY the dependencies first - this layer is cached until Cargo.toml/.lock
# change, so a source-only edit skips straight past it. Scoped to poseidon-server:
# the workspace also contains poseidon-app (Tauri), whose deps need GTK/WebKit
# system libraries this slim builder doesn't have.
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release -p poseidon-server --recipe-path recipe.json

# The RUST build inputs ONLY (no frontend/, no docs/) - so editing the frontend or
# the docs does NOT invalidate this layer and `cargo build` stays cached; only a
# Rust change recompiles our crates. This is what keeps UI-only redeploys fast.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p poseidon-server --bin poseidon-server

# Frontend bundle assembled AFTER the compile: a JS/CSS or docs edit rebuilds only
# these fast file-copy layers, never the Rust workspace. Docs are bundled into the
# web assets so the served UI's in-app docs viewer can fetch assets/docs/*.md (the
# desktop build does this via build.rs); source of truth stays /docs.
COPY frontend ./frontend
COPY docs ./docs
RUN mkdir -p frontend/web/assets/docs && cp docs/*.md frontend/web/assets/docs/

# ---- Dev stage: Docker-only live-reload loop --------------------------------
# `docker compose up` (see compose.yaml) builds this (`--target dev`) and runs the
# web instance with recompile-on-save, so a contributor needs ONLY Docker - no
# Rust toolchain on the host. No source is COPYed in: compose bind-mounts the
# working tree and supplies named volumes for the Cargo registry + build cache.
# Placed BEFORE the runtime stage on purpose, so `docker build` with no --target
# builds the production `runtime` image (the last stage), not this. See docs/RUNNING.md.
FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS dev
RUN cargo install cargo-watch --locked
WORKDIR /build
ENV POSEIDON_STATIC_DIR=/build/frontend/web \
    POSEIDON_DATA_DIR=/data \
    POSEIDON_BIND_ADDR=0.0.0.0 \
    RUST_LOG=info
EXPOSE 8737
# Watch the Rust sources only (the frontend is static and served live from the
# mount - editing JS needs just a browser refresh, no restart). `--poll` because
# inotify events don't cross the Docker Desktop / WSL2 mount boundary reliably.
CMD ["cargo", "watch", "--poll", "-w", "crates", "-x", "run -p poseidon-server --bin poseidon-server"]

# ---- Runtime stage (the default build target) -------------------------------
FROM debian:${DEBIAN_RELEASE}-slim AS runtime

# Re-declare the ARGs consumed in this stage (values inherited from the defaults
# above, or from `--build-arg`).
ARG DEBIAN_RELEASE
# CA certs for outbound HTTPS to Azure DevOps and the OAuth token endpoints.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# No Azure CLI: device-code sign-in is now a native (pure-HTTP) OAuth flow in the
# app, so the image needs no `az` (and no Python) - a much slimmer runtime.
# Non-root user. The data dir is a mount point owned by this user.
RUN useradd --system --uid 10001 --home /nonexistent --shell /usr/sbin/nologin poseidon \
    && mkdir -p /data \
    && chown poseidon:poseidon /data

COPY --from=builder /build/target/release/poseidon-server /usr/local/bin/poseidon-server
COPY --from=builder /build/frontend/web /app/frontend/web

# Runtime configuration:
#   - static bundle location (served by the axum fallback)
#   - data root → the mounted volume (DB + logs land here; see the Helm PVC)
#   - bind on all interfaces inside the container
ENV POSEIDON_STATIC_DIR=/app/frontend/web \
    POSEIDON_DATA_DIR=/data \
    POSEIDON_BIND_ADDR=0.0.0.0 \
    RUST_LOG=info

# Version identity, read at runtime by /env.js + the Doctor update check. Declared
# HERE (the runtime stage) - ARG/ENV don't cross build stages, so setting them in
# the builder never reached the final image. Quoted so a build-timestamp value with
# a space (poseidon.sh passes "YYYYMMDD HH:MM") survives ENV parsing intact.
# CI's docker.yml passes the git sha; local builds pass the timestamp.
ARG POSEIDON_VERSION=latest-main
ARG POSEIDON_COMMIT=unknown
ENV POSEIDON_VERSION="$POSEIDON_VERSION"
ENV POSEIDON_COMMIT="$POSEIDON_COMMIT"

USER poseidon
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8737

ENTRYPOINT ["/usr/local/bin/poseidon-server"]
