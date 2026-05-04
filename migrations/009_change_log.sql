-- Cross-process change feed used by Postgres multi-master deployments.
--
-- When a server publishes a watch event (Pod added/updated/deleted, etc.)
-- it inserts a row here and immediately fires `pg_notify('kais_changes', id)`.
-- Peer servers `LISTEN kais_changes`, fetch the row by id, skip events
-- whose `instance_id` matches their own (local broadcast already happened),
-- and re-publish the rest into their local watch streams.
--
-- A background pruner deletes rows older than ~5 minutes; slow watchers
-- that lag past that just reconnect and re-list.
--
-- SQLite mode never inserts into this table (single-process, no peers),
-- but the table is created on both backends so migrations stay portable.

CREATE TABLE IF NOT EXISTS change_log (
    id TEXT PRIMARY KEY,
    instance_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    namespace TEXT,
    name TEXT NOT NULL,
    event_type TEXT NOT NULL,
    object TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS change_log_created_at_idx ON change_log(created_at);
