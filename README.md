# Superkube

A minimal, single-binary Kubernetes-compatible control plane in Rust. Building it just because I want to build it.

PS: I am working towards making this platform production grade.

`superkube server` boots the API server **and** registers the host as a node — one process, one binary, real containers running through Docker (macOS) or libcontainer (Linux), accessible from real `kubectl`.

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

- Rust 1.75+
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

### Adding more nodes (multi-host)

```bash
# On any other host with Docker:
./target/release/superkube node --server http://<server-host>:6443
# (--name defaults to the host's hostname)
```

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
