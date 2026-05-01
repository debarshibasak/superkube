# syntax=docker/dockerfile:1.7
#
# Multi-stage build for superkube.
#
#   docker build -t superkube:dev .
#   docker run --rm -p 6443:6443 -v superkube-data:/var/lib/superkube superkube:dev
#
# Notes:
#   - On Linux the binary's `docker` runtime backend is compiled out (bollard
#     is gated to target_os = "macos"). Inside the container, `--runtime=auto`
#     resolves to `embedded` (libcontainer) — which currently lacks pod
#     networking, so this image is best used as the API server / control plane
#     while node agents run on the host. Mount /var/run/docker.sock and use
#     `--runtime=mock` only for tests.
#   - The image targets glibc (debian-slim). For a static, distroless image,
#     build with the musl target outside Docker and `COPY` the binary in.

# Keep this in sync with rust-toolchain.toml's `channel`. The toolchain file
# is the source of truth for local dev; this ARG is here so the base image
# matches and we don't pay for a rustup download on every container build.
ARG RUST_VERSION=1.88
ARG DEBIAN_RELEASE=bookworm
# kubectl baked into the image so `docker exec <container> kubectl ...` works
# out of the box. Override with --build-arg KUBECTL_VERSION=v1.31.0 etc.
ARG KUBECTL_VERSION=v1.32.0

# ----------------------------------------------------------------------------
# Build stage
# ----------------------------------------------------------------------------
FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS builder

# libseccomp-dev: required by libcontainer (Linux-only dep).
# libssl-dev / pkg-config: reqwest's default tls = native-tls = OpenSSL.
# protobuf-compiler: containerd-client's build.rs invokes `protoc` to generate
# gRPC bindings.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libseccomp-dev pkg-config libssl-dev protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src        src
COPY migrations migrations

# BuildKit cache mounts: persist cargo registry + target dir across builds
# without bloating the final image. The binary is copied OUT of the cache
# in the same RUN so it survives.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && \
    install -m 0755 target/release/superkube /usr/local/bin/superkube

# ----------------------------------------------------------------------------
# Runtime stage
# ----------------------------------------------------------------------------
FROM debian:${DEBIAN_RELEASE}-slim AS runtime

# Re-declare ARGs after FROM so they're visible in this stage.
ARG KUBECTL_VERSION
# TARGETARCH is auto-set by BuildKit (amd64 / arm64). Falls back to dpkg if missing.
ARG TARGETARCH

# Install runtime deps + kubectl in one layer. curl is removed at the end since
# it was only pulled in to download kubectl.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libseccomp2 libssl3 tini curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 1000 superkube \
    && useradd  --system --uid 1000 --gid superkube \
                --home-dir /var/lib/superkube --shell /usr/sbin/nologin superkube \
    && install -d -m 0755 -o superkube -g superkube /var/lib/superkube \
    && arch="${TARGETARCH:-$(dpkg --print-architecture)}" \
    && curl -fsSL -o /usr/local/bin/kubectl \
         "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${arch}/kubectl" \
    && chmod 0755 /usr/local/bin/kubectl \
    && /usr/local/bin/kubectl version --client=true \
    && apt-get purge -y --auto-remove curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/superkube /usr/local/bin/superkube

# Pre-bake a kubeconfig pointing at the in-container API server so
# `docker exec <container> kubectl get nodes` works with no setup.
RUN install -d -m 0755 -o superkube -g superkube /var/lib/superkube/.kube
COPY --chown=superkube:superkube --chmod=0644 <<EOF /var/lib/superkube/.kube/config
apiVersion: v1
kind: Config
clusters:
  - name: superkube
    cluster:
      server: http://127.0.0.1:6443
contexts:
  - name: superkube
    context:
      cluster: superkube
      user: superkube
      namespace: default
users:
  - name: superkube
    user: {}
current-context: superkube
EOF

# Default to SQLite under the persistent volume. Override with -e DATABASE_URL=...
ENV DATABASE_URL=sqlite:///var/lib/superkube/superkube.db
ENV KUBECONFIG=/var/lib/superkube/.kube/config

# 6443 = API server, 10250 = node agent (logs/exec/port-forward).
EXPOSE 6443 10250

WORKDIR /var/lib/superkube
USER superkube
VOLUME ["/var/lib/superkube"]

# tini reaps zombies and forwards SIGTERM cleanly to the Rust process.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/superkube"]
CMD ["server"]
