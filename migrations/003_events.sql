-- Events table for Kubernetes-compatible events
CREATE TABLE events (
    uid UUID PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    namespace VARCHAR(255) NOT NULL DEFAULT 'default',

    -- Involved object reference
    involved_object_api_version VARCHAR(255),
    involved_object_kind VARCHAR(255),
    involved_object_name VARCHAR(255),
    involved_object_namespace VARCHAR(255),
    involved_object_uid VARCHAR(255),
    involved_object_resource_version VARCHAR(255),
    involved_object_field_path TEXT,

    -- Event details
    reason VARCHAR(255),
    message TEXT,

    -- Source
    source_component VARCHAR(255),
    source_host VARCHAR(255),

    -- Timestamps
    first_timestamp TIMESTAMPTZ,
    last_timestamp TIMESTAMPTZ,
    event_time TIMESTAMPTZ,

    -- Count for duplicate events
    count INT DEFAULT 1,

    -- Event type: Normal or Warning
    event_type VARCHAR(50) DEFAULT 'Normal',

    -- Action and reporting
    action VARCHAR(255),
    reporting_controller VARCHAR(255),
    reporting_instance VARCHAR(255),

    -- Standard metadata timestamps
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

-- Index for efficient queries
CREATE INDEX idx_events_namespace ON events(namespace);
CREATE INDEX idx_events_involved_object ON events(involved_object_kind, involved_object_name, involved_object_namespace);
CREATE INDEX idx_events_last_timestamp ON events(last_timestamp DESC);
CREATE INDEX idx_events_name_namespace ON events(name, namespace);
