-- Namespaces table
CREATE TABLE IF NOT EXISTS namespaces (
    uid UUID PRIMARY KEY,
    name VARCHAR(255) UNIQUE NOT NULL,
    labels JSONB DEFAULT '{}',
    annotations JSONB DEFAULT '{}',
    spec JSONB DEFAULT '{}',
    status VARCHAR(50) NOT NULL DEFAULT 'Active',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for namespace lookups
CREATE INDEX IF NOT EXISTS idx_namespaces_status ON namespaces(status);
CREATE INDEX IF NOT EXISTS idx_namespaces_labels ON namespaces USING GIN(labels);

-- Insert default namespaces
INSERT INTO namespaces (uid, name, status) VALUES
    ('00000000-0000-0000-0000-000000000001', 'default', 'Active'),
    ('00000000-0000-0000-0000-000000000002', 'kube-system', 'Active'),
    ('00000000-0000-0000-0000-000000000003', 'kube-public', 'Active'),
    ('00000000-0000-0000-0000-000000000004', 'kube-node-lease', 'Active')
ON CONFLICT (name) DO NOTHING;
