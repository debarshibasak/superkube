-- Initial schema for Kais - Kubernetes-like platform
-- Portable SQL: works on both PostgreSQL and SQLite.

CREATE TABLE IF NOT EXISTS nodes (
    uid TEXT PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    labels TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'NotReady',
    addresses TEXT NOT NULL DEFAULT '[]',
    capacity TEXT NOT NULL DEFAULT '{}',
    allocatable TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pods (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Pending',
    node_name TEXT,
    pod_ip TEXT,
    host_ip TEXT,
    container_statuses TEXT NOT NULL DEFAULT '[]',
    owner_reference TEXT NOT NULL DEFAULT 'null',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(name, namespace)
);

CREATE TABLE IF NOT EXISTS deployments (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL,
    replicas INTEGER NOT NULL DEFAULT 1,
    ready_replicas INTEGER NOT NULL DEFAULT 0,
    available_replicas INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(name, namespace)
);

CREATE TABLE IF NOT EXISTS services (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',
    labels TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL,
    service_type TEXT NOT NULL DEFAULT 'ClusterIP',
    cluster_ip TEXT,
    node_port INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(name, namespace)
);

CREATE TABLE IF NOT EXISTS endpoints (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',
    subsets TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(name, namespace)
);

CREATE INDEX IF NOT EXISTS idx_pods_namespace ON pods(namespace);
CREATE INDEX IF NOT EXISTS idx_pods_node_name ON pods(node_name);
CREATE INDEX IF NOT EXISTS idx_pods_status ON pods(status);

CREATE INDEX IF NOT EXISTS idx_deployments_namespace ON deployments(namespace);

CREATE INDEX IF NOT EXISTS idx_services_namespace ON services(namespace);

CREATE INDEX IF NOT EXISTS idx_nodes_status ON nodes(status);

CREATE INDEX IF NOT EXISTS idx_endpoints_namespace ON endpoints(namespace);
