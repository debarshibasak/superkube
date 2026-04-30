-- Namespaces table (portable: PostgreSQL + SQLite)
CREATE TABLE IF NOT EXISTS namespaces (
    uid TEXT PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    labels TEXT NOT NULL DEFAULT '{}',
    annotations TEXT NOT NULL DEFAULT '{}',
    spec TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'Active',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_namespaces_status ON namespaces(status);

-- Seed default namespaces. created_at/updated_at use a fixed sentinel value
-- so re-running the migration does not change them. The application will
-- always read these as ISO 8601 strings.
INSERT INTO namespaces (uid, name, status, created_at, updated_at) VALUES
    ('00000000-0000-0000-0000-000000000001', 'default',         'Active', '1970-01-01T00:00:00Z', '1970-01-01T00:00:00Z'),
    ('00000000-0000-0000-0000-000000000002', 'kube-system',     'Active', '1970-01-01T00:00:00Z', '1970-01-01T00:00:00Z'),
    ('00000000-0000-0000-0000-000000000003', 'kube-public',     'Active', '1970-01-01T00:00:00Z', '1970-01-01T00:00:00Z'),
    ('00000000-0000-0000-0000-000000000004', 'kube-node-lease', 'Active', '1970-01-01T00:00:00Z', '1970-01-01T00:00:00Z')
ON CONFLICT (name) DO NOTHING;
