-- Namespaced RBAC: Role + RoleBinding. Stored only — we don't enforce
-- the rules at request time (no auth on the API), but kubectl apply / get
-- round-trip cleanly.

CREATE TABLE IF NOT EXISTS roles (
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
CREATE INDEX IF NOT EXISTS idx_roles_namespace ON roles(namespace);

CREATE TABLE IF NOT EXISTS rolebindings (
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
CREATE INDEX IF NOT EXISTS idx_rolebindings_namespace ON rolebindings(namespace);
