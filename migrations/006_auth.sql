-- Auth-shaped resources: serviceaccounts, secrets, configmaps,
-- clusterroles, clusterrolebindings. Storage only — we don't enforce RBAC
-- yet, but `kubectl apply` against these works.

CREATE TABLE IF NOT EXISTS serviceaccounts (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(name, namespace)
);
CREATE INDEX IF NOT EXISTS idx_serviceaccounts_namespace ON serviceaccounts(namespace);

CREATE TABLE IF NOT EXISTS secrets (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    secret_type TEXT NOT NULL DEFAULT 'Opaque',
    spec TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(name, namespace)
);
CREATE INDEX IF NOT EXISTS idx_secrets_namespace ON secrets(namespace);

CREATE TABLE IF NOT EXISTS configmaps (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(name, namespace)
);
CREATE INDEX IF NOT EXISTS idx_configmaps_namespace ON configmaps(namespace);

-- Cluster-scoped: no namespace column.
CREATE TABLE IF NOT EXISTS clusterroles (
    uid TEXT PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS clusterrolebindings (
    uid TEXT PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
