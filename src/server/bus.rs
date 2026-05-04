//! Change feed for `?watch=true` endpoints.
//!
//! After a successful create / update / delete, handlers call
//! [`Bus::publish_added`] / [`publish_modified`] / [`publish_deleted`].
//! Subscribers — currently the watch path in
//! [`super::table::list_response_live`] — receive every event published
//! since they subscribed, filter by kind / namespace, and forward
//! matching ones to the client as `WatchEvent` JSON.
//!
//! ## Cross-process replication (Postgres multi-master)
//!
//! When [`Bus`] is constructed with a [`Replicator`] (`Bus::new_replicated`),
//! every `publish_*` call ALSO writes the event to the `change_log` table
//! and fires `pg_notify('kais_changes', <row id>)`. Peer servers run a
//! `LISTEN kais_changes` task (set up in [`super::run`]) which fetches the
//! row, drops the event when it originated locally (loop avoidance via
//! `instance_id`), and re-publishes it via [`Bus::republish_local`] so
//! local watchers see it. SQLite mode skips replication entirely —
//! single process, no peers.
//!
//! Buffer capacity on the local broadcast is bounded; slow subscribers
//! that fall further than [`BUS_CAPACITY`] events behind are dropped and
//! the watch stream ends, kubectl reconnects and re-lists.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use sqlx::AnyPool;
use tokio::sync::broadcast;

use crate::db::change_log;

/// How many in-flight events the broadcast channel buffers before
/// dropping slow subscribers.
const BUS_CAPACITY: usize = 1024;

/// Verb conveyed in the `WatchEvent` envelope.
#[derive(Clone, Copy, Debug)]
pub enum WatchEventType {
    Added,
    Modified,
    Deleted,
}

impl WatchEventType {
    pub fn as_wire(&self) -> &'static str {
        match self {
            WatchEventType::Added => "ADDED",
            WatchEventType::Modified => "MODIFIED",
            WatchEventType::Deleted => "DELETED",
        }
    }
}

/// One change in the cluster, as fanned out to every active watcher.
#[derive(Clone, Debug)]
pub struct ChangeEvent {
    /// Singular resource kind: `"Pod"`, `"Service"`, `"Deployment"`, ...
    /// Watchers filter on this so a pod-watcher doesn't receive service
    /// updates.
    pub kind: &'static str,
    /// `None` for cluster-scoped resources (Node, Namespace, ...).
    pub namespace: Option<String>,
    pub name: String,
    pub event_type: WatchEventType,
    /// JSON-serialized object — the full resource as it exists *after*
    /// the change (or, for `Deleted`, the last-known state).
    pub object: Value,
}

/// Filter that the watch stream applies to events from the bus.
#[derive(Clone, Debug)]
pub struct WatchFilter {
    pub kind: &'static str,
    /// `None` matches every namespace; `Some(ns)` only that namespace.
    /// Cluster-scoped resources should always pass `None`.
    pub namespace: Option<String>,
}

impl WatchFilter {
    pub fn matches(&self, event: &ChangeEvent) -> bool {
        if event.kind != self.kind {
            return false;
        }
        match (&self.namespace, &event.namespace) {
            (None, _) => true,
            (Some(want), Some(got)) => want == got,
            (Some(_), None) => false,
        }
    }
}

/// Postgres-side replication hook. Set on the [`Bus`] when the server is
/// running against a shared Postgres so events fan out across server
/// instances. SQLite mode never installs a [`Replicator`].
pub struct Replicator {
    pub pool: AnyPool,
    pub instance_id: String,
    /// Channel name for `pg_notify` / `LISTEN`. Allows tests / parallel
    /// clusters in one DB to coexist; defaults to [`DEFAULT_NOTIFY_CHANNEL`].
    pub channel: String,
}

pub const DEFAULT_NOTIFY_CHANNEL: &str = "kais_changes";

/// Process-wide event publisher.
pub struct Bus {
    tx: broadcast::Sender<ChangeEvent>,
    /// Stable identity for this server process, used by [`Replicator`] to
    /// drop our own echoes when they come back via LISTEN. Always set,
    /// even in SQLite mode (in that case it just isn't compared against
    /// anything).
    instance_id: String,
    /// `None` ⇒ local-only (SQLite, or Postgres before the listener task
    /// has started). `Some` ⇒ every publish is also persisted to
    /// `change_log` and broadcast via `pg_notify`.
    replicator: Option<Arc<Replicator>>,
}

impl Bus {
    /// Local-only bus (SQLite mode, or pre-replicator wiring during boot).
    pub fn new(instance_id: String) -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self {
            tx,
            instance_id,
            replicator: None,
        }
    }

    /// Bus with cross-process replication wired up. The listener task in
    /// [`super::run`] still has to be started separately so peers can
    /// observe what we publish here.
    pub fn new_replicated(instance_id: String, replicator: Replicator) -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self {
            tx,
            instance_id,
            replicator: Some(Arc::new(replicator)),
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ChangeEvent> {
        self.tx.subscribe()
    }

    fn publish<T: Serialize>(
        &self,
        event_type: WatchEventType,
        kind: &'static str,
        namespace: Option<&str>,
        name: &str,
        object: &T,
    ) {
        let object = serde_json::to_value(object).unwrap_or(Value::Null);
        let event = ChangeEvent {
            kind,
            namespace: namespace.map(str::to_string),
            name: name.to_string(),
            event_type,
            object: object.clone(),
        };
        // 1. Local broadcast first — `send` errors only when no receivers
        //    exist, which is the common case (no active watches) and not
        //    interesting.
        let _ = self.tx.send(event);

        // 2. Best-effort cross-process replication. Spawned so handlers
        //    don't pay DB latency for every write; failures are logged
        //    but never block the local watch path.
        if let Some(rep) = &self.replicator {
            let rep = rep.clone();
            let kind_owned = kind.to_string();
            let namespace_owned = namespace.map(str::to_string);
            let name_owned = name.to_string();
            let event_wire = event_type.as_wire().to_string();
            tokio::spawn(async move {
                if let Err(e) = replicate(&rep, &kind_owned, namespace_owned.as_deref(), &name_owned, &event_wire, &object).await {
                    tracing::warn!(
                        "watch replicate {}/{:?}/{} {}: {}",
                        kind_owned, namespace_owned, name_owned, event_wire, e
                    );
                }
            });
        }
    }

    pub fn publish_added<T: Serialize>(
        &self,
        kind: &'static str,
        namespace: Option<&str>,
        name: &str,
        object: &T,
    ) {
        self.publish(WatchEventType::Added, kind, namespace, name, object);
    }

    pub fn publish_modified<T: Serialize>(
        &self,
        kind: &'static str,
        namespace: Option<&str>,
        name: &str,
        object: &T,
    ) {
        self.publish(WatchEventType::Modified, kind, namespace, name, object);
    }

    pub fn publish_deleted<T: Serialize>(
        &self,
        kind: &'static str,
        namespace: Option<&str>,
        name: &str,
        object: &T,
    ) {
        self.publish(WatchEventType::Deleted, kind, namespace, name, object);
    }

    /// Re-publish an event received from a peer server via the LISTEN
    /// task. Bypasses the replicator so we don't echo it back over
    /// Postgres (that's what produced this event in the first place).
    pub fn republish_local(&self, event: ChangeEvent) {
        let _ = self.tx.send(event);
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new(uuid::Uuid::new_v4().to_string())
    }
}

/// One async step of cross-process replication: write to `change_log`,
/// then `pg_notify` so peers wake up.
async fn replicate(
    rep: &Replicator,
    kind: &str,
    namespace: Option<&str>,
    name: &str,
    event_type: &str,
    object: &Value,
) -> anyhow::Result<()> {
    let id = change_log::insert(
        &rep.pool,
        &rep.instance_id,
        kind,
        namespace,
        name,
        event_type,
        object,
    )
    .await?;
    // `pg_notify` is a Postgres builtin; on AnyPool we run it as a plain
    // SELECT. SQLite never reaches this code (replicator is None there).
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(&rep.channel)
        .bind(&id)
        .execute(&rep.pool)
        .await?;
    Ok(())
}
