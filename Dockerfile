# Stage 1: Use official cargo-chef image as base.
# RUST_VERSION is the single source of truth for the build toolchain; keep it in
# lockstep with .github/workflows/ci.yml (env.RUST_VERSION) and mise.toml.
ARG RUST_VERSION=1.95
FROM lukemathwalker/cargo-chef:latest-rust-${RUST_VERSION} AS chef
WORKDIR /usr/src

# Install build dependencies for Debian (including perl and make for vendored OpenSSL build)
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    perl \
    make \
    && rm -rf /var/lib/apt/lists/*

# Stage 2: Generate recipe file for dependencies
FROM chef AS planner

# Copy the whole workspace, not a hand-listed set of manifests.
#
# `cargo chef prepare` reads every workspace member's Cargo.toml, so a list has
# to name all of them — and a crate added without a matching line here fails the
# build on a missing manifest, far from the change that caused it. Copying
# sources into this stage costs nothing downstream: its only output is
# recipe.json, and the expensive `cargo chef cook` layer in the builder is keyed
# on that file's content, which changes only when dependencies change.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Cargo wants a source file for the root binary; the recipe needs only manifests.
RUN mkdir -p src && echo "fn main() {}" > src/main.rs

RUN cargo chef prepare --recipe-path recipe.json

# Stage 2.5: Build frontend assets
FROM node:24-alpine AS frontend-builder
WORKDIR /usr/src/frontend

COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci

COPY frontend/ ./
RUN npm run build

# Stage 2.6: Build bundled user documentation
FROM node:24-alpine AS user-docs-builder
WORKDIR /usr/src/docs

COPY docs/package.json docs/package-lock.json ./
COPY docs/user/package.json ./user/
COPY docs/engineering/package.json ./engineering/
RUN npm ci

COPY docs/ ./
COPY skills/rise-app-builder /usr/src/skills/rise-app-builder
RUN RISE_USER_DOCS_URL=/docs node scripts/generate-llms.mjs && \
    npm run build --workspace=rise-user-docs

# Stage 3: Build dependencies (cached separately from source code)
FROM chef AS builder

COPY --from=planner /usr/src/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/src/target \
    cargo chef cook --release --all-features --recipe-path recipe.json

# Copy project files
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src
COPY migrations ./migrations
COPY static ./static
COPY --from=frontend-builder /usr/src/frontend/dist/ ./static/
COPY .sqlx ./.sqlx

# Build the application with server features
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/src/target \
    SQLX_OFFLINE=true cargo build --release --all-features --bin rise && \
    cp target/release/rise /usr/local/bin/rise

# Stage 4: Create the final, smaller image (match builder's Debian version)
FROM debian:trixie-slim AS rise

# Runtime dependencies. curl is one of them, not a convenience: container
# health checks run *inside* the container, and the ECS task definitions in
# modules/rise-ecs probe /health over HTTP from there.
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from the builder stage
COPY --from=builder /usr/local/bin/rise /usr/local/bin/rise

# Copy the configuration files
COPY config /etc/rise

# Copy static assets for filesystem-based serving (templates, SVGs, Vite build output)
COPY --from=builder /usr/src/static /var/lib/rise/static

# Copy built user documentation for serving via docs_dir
COPY --from=user-docs-builder /usr/src/docs/user/dist /var/rise/docs

# Default config location/run mode for containerized execution
ENV RISE_CONFIG_DIR=/etc/rise
ENV RISE_CONFIG_RUN_MODE=production
ENV RISE_STATIC_DIR=/var/lib/rise/static
ENV RISE_DOCS_DIR=/var/rise/docs

# Expose the application port
EXPOSE 3000

# Set the entrypoint
ENTRYPOINT ["/usr/local/bin/rise"]

# Stage 5: Create the builder image with additional build tools
# Start from debian instead of rise to improve layer caching
FROM debian:trixie-slim AS rise-builder

# Install runtime dependencies (same as rise stage)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Install dependencies for mise and build tools
RUN apt-get update && apt-get install -y \
    curl \
    git \
    && rm -rf /var/lib/apt/lists/*

# Install mise
RUN curl https://mise.run | sh

# Set up mise environment
ENV PATH="/root/.local/bin:/root/.local/share/mise/shims:${PATH}"

# Install build tools via mise.
#
# Pin every tool version explicitly — `@latest` here is what broke the image
# build: a Docker CLI plugin-loading regression in the 29.1.x line makes
# `docker buildx version` (and `docker trust`) report "unknown command: docker
# buildx" even though the plugin binary is present in the searched
# `~/.docker/cli-plugins` directory. See https://github.com/docker/cli/issues/6733.
# pack/railpack/buildkit mirror the versions pinned in `mise.toml`; docker-cli is
# held at the last 29.0.x release (29.0.4), the newest line before the regression
# — buildx itself is fine, so it's pinned to the current latest (0.34.1). Bump
# docker-cli back to the 29.1+ line once the upstream issue is resolved.
ARG PACK_VERSION=0.40.6
ARG DOCKER_CLI_VERSION=29.0.4
ARG RAILPACK_VERSION=0.15.1
ARG BUILDX_VERSION=0.34.1
ARG BUILDKIT_VERSION=0.28.0
ARG TARGETARCH

RUN /root/.local/bin/mise use -g pack@${PACK_VERSION} && \
    /root/.local/bin/mise use -g docker-cli@${DOCKER_CLI_VERSION} && \
    /root/.local/bin/mise use -g ubi:railwayapp/railpack@${RAILPACK_VERSION} && \
    /root/.local/bin/mise install

# Install Docker buildx plugin manually (pinned).
RUN mkdir -p /root/.docker/cli-plugins && \
    curl -sSL "https://github.com/docker/buildx/releases/download/v${BUILDX_VERSION}/buildx-v${BUILDX_VERSION}.linux-${TARGETARCH}" -o /root/.docker/cli-plugins/docker-buildx && \
    chmod +x /root/.docker/cli-plugins/docker-buildx

# Install buildctl from buildkit (pinned).
RUN curl -sSL "https://github.com/moby/buildkit/releases/download/v${BUILDKIT_VERSION}/buildkit-v${BUILDKIT_VERSION}.linux-${TARGETARCH}.tar.gz" | tar -xz -C /usr/local bin/buildctl && \
    chmod +x /usr/local/bin/buildctl

# Verify installations
RUN /root/.local/bin/mise exec -- pack version && \
    /root/.local/bin/mise exec -- docker --version && \
    /root/.local/bin/mise exec -- docker buildx version && \
    buildctl --version

# Copy the rise CLI binary (last to maximize layer caching)
COPY --from=builder /usr/local/bin/rise /usr/local/bin/rise

# Set the entrypoint
ENTRYPOINT []
CMD ["/usr/bin/bash"]
