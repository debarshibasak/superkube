# Kais

A minimal Kubernetes-like container orchestration platform written in Rust.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           CONTROL PLANE (kais server)                        │
│                                                                             │
│  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐          │
│  │   API Server     │  │    Controller    │  │    Scheduler     │          │
│  │     (axum)       │  │     Manager      │  │                  │          │
│  │                  │  │                  │  │                  │          │
│  │ • REST API       │  │ • Deployment     │  │ • Pod binding    │          │
│  │ • kubectl compat │  │   controller     │  │ • Node selection │          │
│  │ • CRUD ops       │  │ • Service        │  │ • Filtering      │          │
│  │                  │  │   controller     │  │                  │          │
│  └────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘          │
│           │                     │                     │                     │
│           └─────────────────────┼─────────────────────┘                     │
│                                 │                                           │
│                          ┌──────▼──────┐                                    │
│                          │  PostgreSQL │                                    │
│                          │             │                                    │
│                          │ • Pods      │                                    │
│                          │ • Deploys   │                                    │
│                          │ • Services  │                                    │
│                          │ • Nodes     │                                    │
│                          │ • Endpoints │                                    │
│                          └─────────────┘                                    │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ HTTP/REST
                    ┌───────────────┼───────────────┐
                    │               │               │
                    ▼               ▼               ▼
┌─────────────────────────┐ ┌─────────────────────────┐ ┌─────────────────────────┐
│     NODE 1              │ │     NODE 2              │ │     NODE N              │
│   (kais node)           │ │   (kais node)           │ │   (kais node)           │
│                         │ │                         │ │                         │
│  ┌───────────────────┐  │ │  ┌───────────────────┐  │ │  ┌───────────────────┐  │
│  │   Node Agent      │  │ │  │   Node Agent      │  │ │  │   Node Agent      │  │
│  │                   │  │ │  │                   │  │ │  │                   │  │
│  │ • Registration    │  │ │  │ • Registration    │  │ │  │ • Registration    │  │
│  │ • Heartbeat       │  │ │  │ • Heartbeat       │  │ │  │ • Heartbeat       │  │
│  │ • Pod sync        │  │ │  │ • Pod sync        │  │ │  │ • Pod sync        │  │
│  │ • Status report   │  │ │  │ • Status report   │  │ │  │ • Status report   │  │
│  └─────────┬─────────┘  │ │  └─────────┬─────────┘  │ │  └─────────┬─────────┘  │
│            │            │ │            │            │ │            │            │
│  ┌─────────▼─────────┐  │ │  ┌─────────▼─────────┐  │ │  ┌─────────▼─────────┐  │
│  │   containerd      │  │ │  │   containerd      │  │ │  │   containerd      │  │
│  └───────────────────┘  │ │  └───────────────────┘  │ │  └───────────────────┘  │
│            │            │ │            │            │ │            │            │
│  ┌─────────▼─────────┐  │ │  ┌─────────▼─────────┐  │ │  ┌─────────▼─────────┐  │
│  │    Containers     │  │ │  │    Containers     │  │ │  │    Containers     │  │
│  │  ┌───┐ ┌───┐      │  │ │  │  ┌───┐ ┌───┐      │  │ │  │  ┌───┐ ┌───┐      │  │
│  │  │Pod│ │Pod│ ...  │  │ │  │  │Pod│ │Pod│ ...  │  │ │  │  │Pod│ │Pod│ ...  │  │
│  │  └───┘ └───┘      │  │ │  │  └───┘ └───┘      │  │ │  │  └───┘ └───┘      │  │
│  └───────────────────┘  │ │  └───────────────────┘  │ │  └───────────────────┘  │
└─────────────────────────┘ └─────────────────────────┘ └─────────────────────────┘
```

## Features

- **kubectl Compatible**: Full compatibility with kubectl commands
- **Namespaces**: Resource isolation and organization
- **Deployments**: Declarative pod management with replica scaling
- **Services**: ClusterIP and NodePort service types
- **Pods**: Container lifecycle management
- **Nodes**: Worker node registration and health monitoring
- **PostgreSQL Backend**: Durable state storage (instead of etcd)
- **containerd Runtime**: Native container runtime integration

## Components

### Control Plane (`kais server`)

| Component | Description |
|-----------|-------------|
| **API Server** | RESTful API compatible with Kubernetes API |
| **Controller Manager** | Reconciles deployments and services |
| **Scheduler** | Assigns pods to available nodes |

### Node Agent (`kais node`)

| Component | Description |
|-----------|-------------|
| **Node Agent** | Registers node, syncs pods, reports status |
| **Runtime** | containerd interface for container management |

## Quick Start

### Prerequisites

- Rust 1.70+
- PostgreSQL 14+
- containerd (for node agent)

### 1. Start PostgreSQL

```bash
docker-compose up -d postgres
```

### 2. Build Kais

```bash
cargo build --release
```

### 3. Start Control Plane

Migrations run automatically on server startup.

```bash
./target/release/kais server --db-url postgres://kais:kais@localhost/kais --port 6443
```

### 4. Start Node Agent

```bash
./target/release/kais node --name worker-1 --server http://localhost:6443
```

### 5. Use kubectl

```bash
# Configure kubectl
export KUBECONFIG=/dev/null
alias kubectl='kubectl --server http://localhost:6443'

# List nodes
kubectl get nodes

# Create a deployment
kubectl apply -f - <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nginx
spec:
  replicas: 3
  selector:
    matchLabels:
      app: nginx
  template:
    metadata:
      labels:
        app: nginx
    spec:
      containers:
      - name: nginx
        image: nginx:latest
        ports:
        - containerPort: 80
EOF

# List pods
kubectl get pods

# Create a service
kubectl apply -f - <<EOF
apiVersion: v1
kind: Service
metadata:
  name: nginx
spec:
  type: NodePort
  selector:
    app: nginx
  ports:
  - port: 80
    targetPort: 80
EOF

# List services
kubectl get services

# Create a namespace
kubectl apply -f - <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: production
EOF

# List namespaces
kubectl get namespaces

# Create resources in a namespace
kubectl -n production apply -f deployment.yaml

# Delete a namespace (also deletes all resources in it)
kubectl delete namespace production
```

## API Endpoints

### Core API (v1)

| Endpoint | Methods | Description |
|----------|---------|-------------|
| `/api/v1/namespaces` | GET, POST | List/create namespaces |
| `/api/v1/namespaces/{name}` | GET, PUT, DELETE | Get/update/delete namespace |
| `/api/v1/namespaces/{ns}/pods` | GET, POST | List/create pods |
| `/api/v1/namespaces/{ns}/pods/{name}` | GET, PUT, DELETE | Get/update/delete pod |
| `/api/v1/namespaces/{ns}/services` | GET, POST | List/create services |
| `/api/v1/namespaces/{ns}/services/{name}` | GET, PUT, DELETE | Get/update/delete service |
| `/api/v1/nodes` | GET, POST | List/create nodes |
| `/api/v1/nodes/{name}` | GET, PUT, DELETE | Get/update/delete node |

### Apps API (v1)

| Endpoint | Methods | Description |
|----------|---------|-------------|
| `/apis/apps/v1/namespaces/{ns}/deployments` | GET, POST | List/create deployments |
| `/apis/apps/v1/namespaces/{ns}/deployments/{name}` | GET, PUT, DELETE | Get/update/delete deployment |

## Database Schema

```sql
-- Core tables
namespaces      -- Namespace definitions
nodes           -- Worker node registry
pods            -- Pod specifications and status
deployments     -- Deployment specifications
services        -- Service definitions
endpoints       -- Service endpoint mappings
```

## Project Structure

```
kais/
├── Cargo.toml
├── src/
│   ├── main.rs           # CLI entry point
│   ├── lib.rs
│   ├── error.rs          # Error types
│   ├── models/           # K8s resource types
│   │   ├── namespace.rs
│   │   ├── pod.rs
│   │   ├── deployment.rs
│   │   ├── service.rs
│   │   └── node.rs
│   ├── db/               # PostgreSQL layer
│   │   └── repository.rs
│   ├── server/           # Control plane
│   │   ├── api.rs
│   │   ├── controller.rs
│   │   └── scheduler.rs
│   └── node/             # Node agent
│       ├── agent.rs
│       └── runtime.rs
├── migrations/
│   └── 001_initial.sql
└── docker-compose.yml
```

## Configuration

### Server Options

| Flag | Environment | Default | Description |
|------|-------------|---------|-------------|
| `--db-url` | `DATABASE_URL` | - | PostgreSQL connection URL |
| `--port` | - | `6443` | API server port |
| `--host` | - | `0.0.0.0` | Bind address |

### Node Options

| Flag | Default | Description |
|------|---------|-------------|
| `--name` | - | Node name (required) |
| `--server` | - | API server URL (required) |
| `--containerd-socket` | `/run/containerd/containerd.sock` | containerd socket path |

## License

MIT
