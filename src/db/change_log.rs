//! `change_log` table — durable backing store for cross-process watch event
//! replication. See `migrations/009_change_log.sql` for the rationale.
//!
//! Only used in Postgres mode. `super::server::bus::Replicator` writes here;
//! the LISTEN task in `server::mod` reads here on each `kais_changes` notify.

use chrono::Utc;
use serde_json::Value;
use sqlx::AnyPool;
use uuid::Uuid;

/// One row of [`change_log`]. Mirrors the broadcast `ChangeEvent` plus the
/// originating server's instance id so peers can skip their own echoes.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChangeLogRow {
    pub instance_id: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
    pub event_type: String,
    /// JSON-encoded object as a string. Stored TEXT so the schema is
    /// portable between Postgres and SQLite.
    pub object: String,
}

/// Insert a new event and return its id (which doubles as the NOTIFY
/// payload). The id is a fresh UUID so peers can use it as a primary key
/// lookup without coordinating on counters.
pub async fn insert(
    pool: &AnyPool,
    instance_id: &str,
    kind: &str,
    namespace: Option<&str>,
    name: &str,
    event_type: &str,
    object: &Value,
) -> sqlx::Result<String> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let object_str = object.to_string();
    sqlx::query(
        "INSERT INTO change_log \
         (id, instance_id, kind, namespace, name, event_type, object, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(instance_id)
    .bind(kind)
    .bind(namespace)
    .bind(name)
    .bind(event_type)
    .bind(&object_str)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Fetch a single row by id. Returns `None` if the pruner already removed
/// it (slow listener, very unlikely under normal load).
pub async fn fetch(pool: &AnyPool, id: &str) -> sqlx::Result<Option<ChangeLogRow>> {
    sqlx::query_as::<_, ChangeLogRow>(
        "SELECT instance_id, kind, namespace, name, event_type, object \
         FROM change_log WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Delete rows older than `ttl_seconds`. Run periodically by a background
/// task in `server::run` (Postgres mode only).
pub async fn prune_older_than(pool: &AnyPool, ttl_seconds: i64) -> sqlx::Result<u64> {
    let cutoff = (Utc::now() - chrono::Duration::seconds(ttl_seconds)).to_rfc3339();
    let res = sqlx::query("DELETE FROM change_log WHERE created_at < ?")
        .bind(&cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}
