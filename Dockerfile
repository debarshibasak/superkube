# syntax=docker/dockerfile:1.7
#
# Multi-stage build for superkube.
#
#   docker build -t superkube:dev .
#   docker run --rm --privileged -p 6443:6443 -p 30080:30080 \
#       -v superkube-data:/var/lib/superkube superkube:dev
#
# Notes:
#   - Inside the container `--runtime=auto` resolves to `embedded`
#     (libcontainer), so pods run *inside this container*, kind-style. That
#     needs `--privileged` (namespaces, cgroups, mounts, the pod bridge) and
#     a real volume at /var/lib/superkube: overlayfs can't put its upper
#     layer on Docker's own overlay, so without a volume every pod rootfs is
#     copied instead of overlaid. Publish the NodePorts you use (or run with
#     `--network host` on Linux to get them, and the pod bridge, on the host).
#   - The `docker` runtime backend is compiled out on Linux (bollard is
#     gated to macOS). `--runtime=mock` works for API-only tests and needs
#     neither `--privileged` nor root.
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

# libssl-dev / pkg-config: reqwest's default tls = native-tls = OpenSSL.
# (libcontainer is built without its seccomp feature, so no libseccomp-dev.)
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev \
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
# it was only pulled in to download kubectl. iptables is for the embedded
# runtime's pod-egress MASQUERADE rule.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 iptables tini curl \
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
# Root: the embedded runtime creates namespaces, cgroups, mounts, and the pod
# bridge. The `superkube` user is kept for API-only runs (`--user superkube`
# with `--runtime=mock`).
VOLUME ["/var/lib/superkube"]

# tini reaps zombies and forwards SIGTERM cleanly to the Rust process.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/superkube"]
CMD ["server"]
