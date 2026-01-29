-- Initial schema for Kais - Kubernetes-like platform

-- Nodes table
CREATE TABLE IF NOT EXISTS nodes (
    uid UUID PRIMARY KEY,
    name VARCHAR(255) UNIQUE NOT NULL,
    labels JSONB DEFAULT '{}',
    status VARCHAR(50) NOT NULL DEFAULT 'NotReady',
    addresses JSONB DEFAULT '[]',
    capacity JSONB DEFAULT '{}',
    allocatable JSONB DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Pods table
CREATE TABLE IF NOT EXISTS pods (
    uid UUID PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    namespace VARCHAR(255) NOT NULL DEFAULT 'default',
    labels JSONB DEFAULT '{}',
    annotations JSONB DEFAULT '{}',
    spec JSONB NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'Pending',
    node_name VARCHAR(255),
    pod_ip VARCHAR(45),
    container_statuses JSONB DEFAULT '[]',
    owner_reference JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(name, namespace)
);

-- Deployments table
CREATE TABLE IF NOT EXISTS deployments (
    uid UUID PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    namespace VARCHAR(255) NOT NULL DEFAULT 'default',
    labels JSONB DEFAULT '{}',
    annotations JSONB DEFAULT '{}',
    spec JSONB NOT NULL,
    replicas INT NOT NULL DEFAULT 1,
    ready_replicas INT NOT NULL DEFAULT 0,
    available_replicas INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(name, namespace)
);

-- Services table
CREATE TABLE IF NOT EXISTS services (
    uid UUID PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    namespace VARCHAR(255) NOT NULL DEFAULT 'default',
    labels JSONB DEFAULT '{}',
    spec JSONB NOT NULL,
    service_type VARCHAR(50) NOT NULL DEFAULT 'ClusterIP',
    cluster_ip VARCHAR(45),
    node_port INT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(name, namespace)
);

-- Endpoints table (for service discovery)
CREATE TABLE IF NOT EXISTS endpoints (
    uid UUID PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    namespace VARCHAR(255) NOT NULL DEFAULT 'default',
    subsets JSONB DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(name, namespace)
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_pods_namespace ON pods(namespace);
CREATE INDEX IF NOT EXISTS idx_pods_node_name ON pods(node_name);
CREATE INDEX IF NOT EXISTS idx_pods_status ON pods(status);
CREATE INDEX IF NOT EXISTS idx_pods_labels ON pods USING GIN(labels);

CREATE INDEX IF NOT EXISTS idx_deployments_namespace ON deployments(namespace);
CREATE INDEX IF NOT EXISTS idx_deployments_labels ON deployments USING GIN(labels);

CREATE INDEX IF NOT EXISTS idx_services_namespace ON services(namespace);
CREATE INDEX IF NOT EXISTS idx_services_labels ON services USING GIN(labels);

CREATE INDEX IF NOT EXISTS idx_nodes_status ON nodes(status);
CREATE INDEX IF NOT EXISTS idx_nodes_labels ON nodes USING GIN(labels);

CREATE INDEX IF NOT EXISTS idx_endpoints_namespace ON endpoints(namespace);
