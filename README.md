# Superkube

A minimal, single-binary Kubernetes-compatible control plane in Rust. Building it just because I want to build it.

PS: I am working towards making this platform production grade.

`superkube server` boots the API server **and** registers the host as a node — one process, one binary, real containers running through Docker (macOS) or libcontainer (Linux), accessible from real `kubectl`.

<img width="1502" height="776" alt="Screenshot 2026-05-01 at 16 54 50" src="https://github.com/user-attachments/assets/5ccf7058-1635-4116-aec3-9f95e377fec6" />

<img width="1512" height="982" alt="Screenshot 2026-05-01 at 16 55 27" src="https://github.com/user-attachments/assets/2240ee86-eb9c-47fc-b0e5-b11b39685fb1" />


## What works

- **kubectl-shaped API**: discovery, table responses, `cluster-info`, `get all`, `describe`, `logs -f`, `exec`, `port-forward`.
- **Workloads**: Pods, Deployments, StatefulSets, DaemonSets — each with their own controller loop.
- **Networking**: Services (ClusterIP, NodePort, LoadBalancer), Endpoints; a userspace NodePort proxy on every node forwards to local pods.
- **Configuration**: ServiceAccount, Secret, ConfigMap.
- **RBAC (storage only)**: ClusterRole, ClusterRoleBinding.
- **Scheduling**: `nodeSelector`, full node affinity (`In`/`NotIn`/`Exists`/`DoesNotExist`/`Gt`/`Lt`), pod affinity / anti-affinity with topology keys.
- **Observability**: Events emitted by the controllers, scheduler, and node agent.
- **Storage**: SQLite (default, zero-setup) or PostgreSQL — same schema, picked by `--db-url`.

Most of `kubectl get/apply/delete/describe/logs/exec/port-forward/cluster-info` works against this server with stock `kubectl`.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                  CONTROL PLANE  (superkube server)                      │
│                                                                         │
│  ┌──────────────┐  ┌──────────────────┐  ┌──────────────┐              │
│  │  API server  │  │   Controllers    │  │  Scheduler   │              │
│  │   (axum)     │  │ Deployment / SS  │  │ + node-affty │              │
│  │              │  │   DS / Pod /     │  │ + pod-affty  │              │
│  │              │  │   Service /      │  │              │              │
│  │              │  │   Endpoints      │  │              │              │
│  └──────┬───────┘  └────────┬─────────┘  └──────┬───────┘              │
│         │                   │                   │                       │
│         └───────────────────┼───────────────────┘                       │
│                             │                                           │
│                  ┌──────────▼──────────┐                                │
│                  │  SQLite or Postgres │                                │
│                  └─────────────────────┘                                │
│                                                                         │
│  ┌────────────────────  embedded node agent  ────────────────────────┐  │
│  │ registers the server's host as a control-plane node automatically │  │
│  └───────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────┘
                                   │
                                   │  HTTP / WebSocket
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                  NODE AGENT  (superkube node, optional)                 │
│                                                                         │
│  Heartbeat / pod sync / NodePort proxy / log + exec relay               │
│                                                                         │
│  Runtime selector:                                                      │
│    macOS → Docker (bollard → /var/run/docker.sock)                      │
│    Linux → embedded (oci-distribution + libcontainer)                   │
│    any   → mock (in-memory, for tests)                                  │
└─────────────────────────────────────────────────────────────────────────┘
```

### Multi-master with PostgreSQL

Point N copies of `superkube server` at the same Postgres URL and they all become active masters. Every API server is fully usable; coordination of the *write paths* (controllers + scheduler) happens through short-lived leases in a `leases` table, so any single object is reconciled by exactly one master at a time. Adding a master is just another process with the same `--db-url`.

```
   ┌─────────────────────────────────────────────────────────────────────┐
   │           kubectl / clients              workers (superkube node)   │
   └──────────────┬─────────────────────────────────────┬────────────────┘
                  │                                     │  HTTP/WebSocket
                  ▼                                     ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │                       Load balancer  (:6443)                        │
   │                       round-robin / least-conn                      │
   └────────────┬──────────────────┬───────────────────┬─────────────────┘
                ▼                  ▼                   ▼
   ┌────────────────────┐ ┌────────────────────┐ ┌────────────────────┐
   │   superkube #1     │ │   superkube #2     │ │   superkube #3     │
   │   (active master)  │ │   (active master)  │ │   (active master)  │
   │                    │ │                    │ │                    │
   │  API server  ✓     │ │  API server  ✓     │ │  API server  ✓     │
   │  Controllers (lease)│ │  Controllers (lease)│ │  Controllers (lease)│
   │  Scheduler   (lease)│ │  Scheduler   (lease)│ │  Scheduler   (lease)│
   │  embedded agent    │ │  embedded agent    │ │  embedded agent    │
   └──────────┬─────────┘ └──────────┬─────────┘ └──────────┬─────────┘
              │                      │                      │
              │            --db-url=postgres://…            │
              └──────────────────────┼──────────────────────┘
                                     ▼
              ┌──────────────────────────────────────────────┐
              │                 PostgreSQL                   │
              │      primary  ──streaming repl──►  replica   │
              │                                              │
              │  Tables:  pods / deployments / services /…   │
              │           + leases  (controller/deployment,  │
              │             controller/scheduler, …)         │
              │                                              │
              │  Single source of truth. Each named lease    │
              │  has one current `holder` — that holder is   │
              │  the only master running that controller     │
              │  for the next ~30s.                          │
              └──────────────────────────────────────────────┘
```

How dispatch works:

- **API serves are symmetric.** Any master answers reads and writes; the LB just round-robins.
- **Per-controller leases (Postgres only).** Each tick, every master tries to grab the lease for `controller/deployment`, `controller/statefulset`, `controller/daemonset`, `controller/pod`, `controller/service`, and `scheduler`. Whoever wins runs that loop; the others skip until the lease frees up. Different leases land on different masters, so the work spreads.
- **Acquisition is one UPSERT.** `INSERT … ON CONFLICT (name) DO UPDATE … WHERE leases.holder = me OR leases.expires_at < now()` — a row is taken over only if it's stale. No advisory locks, no long-held connections.
- **Failure recovery is the TTL.** If the lease holder crashes, the lease expires after 30s and another master picks it up on its next tick.
- **SQLite mode skips this entirely.** SQLite is single-process by design, so the lease layer short-circuits to "always own it" — the `leases` table is created but never written.
- **Work execution still flows through `pod.spec.nodeName`.** The scheduler (whichever master holds its lease) writes `nodeName`; the embedded agent on that host's master picks the pod up and runs it. Masters are also nodes, so this is the same path as a single-master cluster.
- **HA is the database's job.** Use a managed Postgres or a primary/replica with automatic failover (Patroni, RDS Multi-AZ, Cloud SQL HA). Superkube just needs one connection string.
- **Workers don't pin to a master.** The node agent talks HTTP/WebSocket to the LB; any master answers pod sync, log relay, and exec.

## Quick start

### Prerequisites

- Rust 1.88+ (transitive deps: `home` ≥0.5.12 needs 1.88, `base64ct` ≥1.8 needs `edition2024`/1.85)
- One of:
    - **macOS**: Docker Desktop running (the embedded node agent talks to its socket)
    - **Linux**: a kernel with cgroups v2 + namespaces (any modern distro); `libseccomp` headers if you build with seccomp enabled
- `kubectl` ≥1.27 if you want to drive it

### Build and run

```bash
cargo build --release

# Single command — server + embedded node agent + control-plane registration.
./target/release/superkube server
```

That's it. The first run creates `./superkube.db` (SQLite), starts the API on `:6443`, and the embedded agent registers the host:

```bash
$ kubectl --server=http://localhost:6443 get nodes
NAME                    STATUS   ROLES           AGE   VERSION
Debarshis-MacBook-Pro   Ready    control-plane   3s    superkube/0.1.0
```

### Running on macOS

macOS is the primary development target. The embedded node agent talks to a Docker-API-compatible Unix socket at `/var/run/docker.sock` — Docker Desktop, Colima, and Podman all expose one (Colima and Podman either symlink it for you or do so via a small helper). Pick whichever you prefer; the rest of the project doesn't care.

> **Why the socket path matters:** `bollard` (the Rust Docker client) is hardcoded to `/var/run/docker.sock` here and does **not** read `DOCKER_HOST`. Whatever runtime you choose has to be reachable at that path.

#### Shared prerequisites

```bash
# Rust toolchain (skip if `rustc --version` already prints 1.88+)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# kubectl
brew install kubectl
```

Then pick **one** of the three container runtimes below.

#### Option A — Docker Desktop

1. Install and start it:

   ```bash
   brew install --cask docker
   open -a Docker
   # wait for the whale icon → "Docker Desktop is running"
   docker ps   # sanity check, should succeed without sudo
   ```

2. Build and start superkube:

   ```bash
   cargo build --release
   ./target/release/superkube server
   ```

3. From a second terminal:

   ```bash
   kubectl --server=http://localhost:6443 get nodes
   ```

Apple Silicon note: Docker Desktop emulates `linux/amd64` images via QEMU. If a pod stays in `ContainerCreating`, prefer multi-arch images or pin `linux/arm64` variants (e.g. `nginx:latest` is multi-arch and works out of the box).

#### Option B — Colima (open-source, no Docker Desktop)

[Colima](https://github.com/abiosoft/colima) runs `dockerd` inside a small Lima VM and exposes the socket back to the host — fully open source, no Docker Desktop license, fewer background services.

1. Install Colima and the Docker CLI:

   ```bash
   brew install colima docker
   ```

2. Start a VM (defaults are fine; tune later if needed):

   ```bash
   colima start
   # or, to size it explicitly:
   #   colima start --cpu 4 --memory 6 --disk 30
   ```

   On Apple Silicon, add `--arch aarch64` (default) for native arm64 or `--arch x86_64` if you specifically need amd64 images without emulation.

3. Confirm the socket is wired up:

   ```bash
   ls -l /var/run/docker.sock   # should symlink into ~/.colima/default/
   docker ps                    # sanity check
   ```

   If `/var/run/docker.sock` is missing (older Colima, or a leftover from Docker Desktop), symlink it once:

   ```bash
   sudo ln -sf "$HOME/.colima/default/docker.sock" /var/run/docker.sock
   ```

4. Build and run superkube as in Option A. Stop the VM with `colima stop` when you're done.

#### Option C — Podman

[Podman](https://podman.io) on macOS runs its container engine inside a `podman machine` VM and ships a small helper (`podman-mac-helper`) that points `/var/run/docker.sock` at it.

1. Install Podman:

   ```bash
   brew install podman
   ```

2. Create and start the VM:

   ```bash
   podman machine init
   podman machine start
   ```

   Default sizing is conservative — if pods get OOM-killed, recreate with more headroom:

   ```bash
   podman machine stop
   podman machine rm
   podman machine init --cpus 4 --memory 6144 --disk-size 30
   podman machine start
   ```

3. Wire the Docker-compat socket to `/var/run/docker.sock`:

   ```bash
   sudo /opt/homebrew/bin/podman-mac-helper install   # Apple Silicon
   # or  sudo /usr/local/bin/podman-mac-helper install   on Intel
   podman machine stop && podman machine start        # required after helper install
   docker ps                                          # should work via Podman now
   ```

   Without the helper, Podman only exposes its socket inside the VM. You can still bridge it manually by reading `podman machine inspect | jq -r '.[0].ConnectionInfo.PodmanSocket.Path'` and symlinking that to `/var/run/docker.sock`, but the helper is the supported path.

4. Build and run superkube as in Option A.

Podman caveats worth knowing:

- The Docker API surface `bollard` uses (pull, create, start, inspect, logs, exec, port publishing) all work, but Podman's **exec stream** has historically been the flakiest part of the compat layer — if `kubectl exec` hangs or drops, that's the first thing to suspect.
- Rootless Podman publishes ports through `slirp4netns` / `pasta`, so NodePort works for `curl` from the host but throughput is lower than Docker Desktop's vmnet path.

#### Troubleshooting (any runtime)

- `error trying to connect: ... /var/run/docker.sock` — the runtime isn't running, or its socket isn't at that path. `ls -l /var/run/docker.sock` and re-check the steps for whichever option you picked.
- Port `6443` already in use — another tool (often a previous run, or a real Kubernetes context) is bound. `lsof -iTCP:6443 -sTCP:LISTEN` to find it, or pass `--port 8443` to `superkube server`.
- NodePort `curl` hangs — all three runtimes publish container ports onto the host, so reach them via `localhost`. If you're calling from another machine on the LAN, hit the Mac's LAN IP, not `localhost`.

### kubectl

```bash
# (optional) wire up a kubeconfig once
cat > ~/.kube/superkube.yaml <<'EOF'
apiVersion: v1
kind: Config
clusters: [{name: superkube, cluster: {server: http://localhost:6443}}]
contexts: [{name: superkube, context: {cluster: superkube, user: superkube}}]
users:    [{name: superkube, user: {}}]
current-context: superkube
EOF
export KUBECONFIG=~/.kube/superkube.yaml

kubectl apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nginx
spec:
  replicas: 3
  selector: {matchLabels: {app: nginx}}
  template:
    metadata: {labels: {app: nginx}}
    spec:
      containers:
        - name: nginx
          image: nginx:latest
          ports: [{containerPort: 80}]
          resources:
            requests: {cpu: 100m, memory: 128Mi}
            limits:   {cpu: 500m, memory: 256Mi}
EOF

kubectl apply -f - <<'EOF'
apiVersion: v1
kind: Service
metadata: {name: nginx}
spec:
  type: NodePort
  selector: {app: nginx}
  ports: [{port: 80, targetPort: 80, nodePort: 31080}]
EOF

kubectl get all                # pods, deployments, services, RS/RC stubs
kubectl logs -f <pod>          # streams from Docker
kubectl exec -it <pod> -- sh   # interactive shell inside the container
curl http://localhost:31080/   # NodePort proxy → real nginx
```

### Cross-compiling for Linux from macOS

If you develop on a Mac but want a binary that runs on a Linux node, cross-compile with `cross` (recommended — it runs the build inside a Linux container, so glibc, OpenSSL, and `libseccomp` headers are sorted out for you):

```bash
# one-time setup
cargo install cross
rustup target add x86_64-unknown-linux-gnu        # Intel/AMD Linux
rustup target add aarch64-unknown-linux-gnu       # ARM Linux (Graviton, RPi 64-bit, etc.)

# build (Docker Desktop / Colima / Podman must be running — cross uses it)
cross build --release --target x86_64-unknown-linux-gnu
# binary lands at: ./target/x86_64-unknown-linux-gnu/release/superkube
```

For a fully static binary that runs on any Linux without glibc concerns, target musl. You'll need a musl cross-toolchain because the host macOS linker can't produce ELF binaries:

```bash
brew tap messense/macos-cross-toolchains
brew install x86_64-unknown-linux-musl
rustup target add x86_64-unknown-linux-musl

# .cargo/config.toml
# [target.x86_64-unknown-linux-musl]
# linker = "x86_64-linux-musl-gcc"

CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc \
  cargo build --release --target x86_64-unknown-linux-musl
```

Notes:

- The `embedded` runtime (libcontainer) only compiles on Linux, so cross-compilation is the only way to produce that variant from a Mac. The `docker` runtime cross-compiles cleanly either way.
- `seccomp` requires `libseccomp` headers in the build environment. `cross`'s default image already has them; for raw `cargo build` against musl, either disable the seccomp feature or install `libseccomp-dev` into the toolchain sysroot.
- Apple Silicon → x86_64 Linux works via `cross` (it runs an amd64 container under QEMU emulation in Docker Desktop). It's slow but reliable; prefer `aarch64-unknown-linux-gnu` if your target Linux box is ARM.

### Adding more nodes (multi-host)

```bash
# On any other host with Docker:
./target/release/superkube node --server http://<server-host>:6443
# (--name defaults to the host's hostname)
```

Or, having cross-compiled above, ship the produced binary to the Linux host:

```bash
scp target/x86_64-unknown-linux-gnu/release/superkube node1:/usr/local/bin/
ssh node1 'superkube node --server http://<server-host>:6443'
```

## Deployment

The repo ships ready-to-use service files and one-shot installers under [`deploy/`](deploy/), plus a [`Makefile`](Makefile) wrapping the common targets.

### As a service

| Host | Files | Install |
|------|-------|---------|
| Linux (systemd) | [`deploy/systemd/superkube-server.service`](deploy/systemd/superkube-server.service), [`superkube-node.service`](deploy/systemd/superkube-node.service), [`superkube.env.example`](deploy/systemd/superkube.env.example) | `sudo make install-linux` (server) or `sudo make install-linux-node SERVER=http://master:6443` |
| macOS (launchd) | [`deploy/launchd/dev.superkube.server.plist`](deploy/launchd/dev.superkube.server.plist) | `sudo make install-macos` |

The installers ([`install-linux.sh`](deploy/install/install-linux.sh), [`install-macos.sh`](deploy/install/install-macos.sh)) auto-detect a binary in `target/release/` (or any `target/<linux-triple>/release/` for cross-builds), create a dedicated `superkube` user, set up `/var/lib/superkube`, write `/etc/superkube/superkube.env`, drop the unit/plist into place, and start it. They're idempotent — re-run to upgrade. `make install` picks the right one for the host OS.

Inspect / control:

```bash
# Linux
sudo systemctl status  superkube-server
sudo journalctl -fu    superkube-server

# macOS
sudo launchctl print system/dev.superkube.server
tail -f /var/log/superkube/server.{log,err}
```

Uninstall with `sudo make uninstall` (binary, data, and env file are left in place — remove manually if you want a clean slate).

### As a container

A multi-stage [`Dockerfile`](Dockerfile) is included. Build with BuildKit (cache mounts speed up incremental builds):

```bash
make docker                                  # → superkube:dev
make docker-run                              # → :6443 on the host, SQLite on a named volume
```

Or directly:

```bash
DOCKER_BUILDKIT=1 docker build -t superkube .
docker run --rm -p 6443:6443 \
    -v superkube-data:/var/lib/superkube \
    superkube
```

The image runs as a non-root user, exposes `6443` (API) and `10250` (node agent), and defaults to SQLite at `/var/lib/superkube/superkube.db`. For Postgres, pass `-e DATABASE_URL=postgres://…`. Note: inside a Linux container `--runtime=docker` is not compiled in (bollard is gated to macOS in [`Cargo.toml`](Cargo.toml)) — the image is intended for the API/control-plane role, with node agents running on the host where they can reach a real container runtime.

## CLI

### `superkube server`

| Flag | Env | Default | Notes |
|------|-----|---------|-------|
| `--db-url` | `DATABASE_URL` | `sqlite://./superkube.db` | Postgres also supported: `postgres://user:pass@host/db` |
| `--host` | — | `0.0.0.0` | Bind address |
| `--port` | — | `6443` | API server port |
| `--pod-cidr` | — | `10.244.0.0/16` | First /24 used by the embedded agent for pod IPs |
| `--service-cidr` | — | `10.96.0.0/12` | First /24 used to auto-assign ClusterIPs |

Server boots the API + an embedded node agent that registers the host as the control-plane node. No separate `superkube node` invocation is needed for a single-host cluster.

### `superkube node`

| Flag | Default | Notes |
|------|---------|-------|
| `--server` | — required | URL of the superkube control plane |
| `--name` | host's short hostname | Node name |
| `--runtime` | `auto` | `auto` / `docker` / `embedded` / `mock` |
| `--containerd-socket` | `/run/containerd/containerd.sock` | only used by the mock runtime placeholder |

`--runtime=auto` picks Docker on macOS, the embedded libcontainer runtime on Linux, otherwise the mock.

## Container runtimes

| Backend | Where | What it talks to | Status |
|---------|-------|------------------|--------|
| `docker` | macOS, Linux | `/var/run/docker.sock` via `bollard` | Production-ready: pull / create / start / inspect / logs (live stream) / exec / port publishing for NodePort. |
| `embedded` | Linux only | youki's `libcontainer` crate, in-process | Skeleton: image pull (`oci-distribution`) → bundle build (our `oci/bundle.rs`) → `libcontainer::ContainerBuilder.start()`. **TODO:** networking (no veth/bridge yet), log capture, exec. |
| `mock` | any | nothing | In-memory stub for tests / dev without a runtime. |

The embedded path is the answer to "single static binary, no host daemon" on Linux: `superkube node --runtime=embedded` pulls images itself and hands the OCI bundle to libcontainer for namespaces / cgroups v2 / pivot_root.

## Ports

| Port | Who | What |
|------|-----|------|
| **6443** | superkube server | Kubernetes API (kubectl talks here) |
| **10250** | superkube node agent | Logs / exec / port-forward HTTP+WS endpoint |
| **30000–32767** | superkube node proxy | NodePort listeners — one TCP socket per `type: NodePort` Service, opened on `0.0.0.0:<nodePort>` |
| pod's `containerPort` (e.g. **80**) | the container itself | Inside the pod's netns, on the pod IP (`10.244.0.X` by default) |

The CNI / bridge layer doesn't open any port — it's just netlink syscalls into the kernel to wire up `superkube0`, veth pairs, IPs, and routes. Nothing listens for connections there.

## Resources

| Group/Version | Kinds | Notes |
|---------------|-------|-------|
| `v1` | Pod, Service, Endpoints, Node, Namespace, Event, ServiceAccount, Secret, ConfigMap | Pods/Services run real workloads; SA/Secret/CM are storage-only. |
| `v1` | ReplicationController | Stub (empty list). Exists so `kubectl get all` doesn't 404. |
| `apps/v1` | Deployment, StatefulSet, DaemonSet | Each has its own reconciliation loop; pods are owned directly. |
| `apps/v1` | ReplicaSet | Stub (empty list). Deployments don't materialize ReplicaSets here. |
| `rbac.authorization.k8s.io/v1` | ClusterRole, ClusterRoleBinding | Stored only — no enforcement. |

## Storage

One portable schema across both backends.

```
namespaces / nodes / pods / deployments / services / endpoints
events / serviceaccounts / secrets / configmaps
clusterroles / clusterrolebindings
statefulsets / daemonsets
```

JSON spec/labels/annotations stored as `TEXT`; UUIDs and timestamps as ISO strings. Migrations run on every server start.

## Known caveats

- **`kubectl apply` on existing objects** uses HTTP `PATCH`, which we don't implement yet. First-time apply (PUT/POST) works; re-applying a changed resource currently fails with `MethodNotAllowed`. Workarounds: `kubectl replace -f file.yaml --force`, or `delete` + `apply`.
- **Embedded runtime**: skeleton only — image pull and libcontainer wiring are in place, but pod networking, log capture, and exec aren't yet hooked up. On Linux today, `--runtime=docker` is the productive choice.
- **No CNI**: pod IPs are assigned from `--pod-cidr` (default `10.244.0.0/16`) but pod-to-pod connectivity isn't wired. Service traffic works because the NodePort proxy connects to the host port that Docker publishes for each container.
- **No RBAC enforcement**: ClusterRole/Binding objects round-trip through the API but are not consulted at request time. The API has no auth.
- **OpenAPI schemas** aren't generated. We serve a benign empty `/openapi/v2` (zero bytes, parses as an empty protobuf `Document`) and an empty `/openapi/v3` JSON, so kubectl validation passes without `--validate=false`.

## Status

Hobby project — built incrementally. The pieces above all work end-to-end on macOS through Docker Desktop; the Linux embedded path compiles but needs a Linux box to actually exercise.

## License

MIT
