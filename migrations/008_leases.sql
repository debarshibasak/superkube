-- Per-controller leases for multi-master coordination over PostgreSQL.
-- Multiple `superkube server` processes pointed at the same Postgres race
-- for these named leases; the row's `holder` is the winner for `expires_at`.
--
-- The schema is portable, but the lease dance only runs in Postgres mode
-- (see src/db/lease.rs). In SQLite mode the server is single-process by
-- design, so this table sits unused.

CREATE TABLE IF NOT EXISTS leases (
    name TEXT PRIMARY KEY,
    holder TEXT,
    expires_at TEXT
);
