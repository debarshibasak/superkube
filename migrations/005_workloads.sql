-- StatefulSets: stable-identity, ordered pod sets.
CREATE TABLE IF NOT EXISTS statefulsets (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL,
    replicas INTEGER NOT NULL DEFAULT 1,
    ready_replicas INTEGER NOT NULL DEFAULT 0,
    current_replicas INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(name, namespace)
);

CREATE INDEX IF NOT EXISTS idx_statefulsets_namespace ON statefulsets(namespace);

-- DaemonSets: one pod per matching node.
CREATE TABLE IF NOT EXISTS daemonsets (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL,
    desired_number_scheduled INTEGER NOT NULL DEFAULT 0,
    current_number_scheduled INTEGER NOT NULL DEFAULT 0,
    number_ready INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(name, namespace)
);

CREATE INDEX IF NOT EXISTS idx_daemonsets_namespace ON daemonsets(namespace);
