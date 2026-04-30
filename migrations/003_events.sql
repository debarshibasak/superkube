-- Events table (portable: PostgreSQL + SQLite)
CREATE TABLE IF NOT EXISTS events (
    uid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT 'default',

    -- Involved object reference
    involved_object_api_version TEXT,
    involved_object_kind TEXT,
    involved_object_name TEXT,
    involved_object_namespace TEXT,
    involved_object_uid TEXT,
    involved_object_resource_version TEXT,
    involved_object_field_path TEXT,

    -- Event details
    reason TEXT,
    message TEXT,

    -- Source
    source_component TEXT,
    source_host TEXT,

    -- Timestamps (ISO 8601 strings)
    first_timestamp TEXT,
    last_timestamp TEXT,
    event_time TEXT,

    -- Count for duplicate events
    count INTEGER DEFAULT 1,

    -- Event type: Normal or Warning
    event_type TEXT DEFAULT 'Normal',

    -- Action and reporting
    action TEXT,
    reporting_controller TEXT,
    reporting_instance TEXT,

    -- Standard metadata timestamps (ISO 8601 strings)
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,

    UNIQUE(name, namespace)
);

CREATE INDEX IF NOT EXISTS idx_events_namespace ON events(namespace);
CREATE INDEX IF NOT EXISTS idx_events_involved_object ON events(involved_object_kind, involved_object_name, involved_object_namespace);
CREATE INDEX IF NOT EXISTS idx_events_last_timestamp ON events(last_timestamp);
