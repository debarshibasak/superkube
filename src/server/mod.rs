mod api;
mod bus;
mod controller;
mod printers;
mod routes;
mod scheduler;
mod table;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use sqlx::AnyPool;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

use crate::db::{Backend, LeaseManager};

pub use controller::ControllerManager;
pub use scheduler::Scheduler;

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub pool: AnyPool,
    /// Service CIDR (e.g. "10.96.0.0/12"). Used for ClusterIP auto-assign.
    pub service_cidr: String,
    /// In-process change feed for `?watch=true` endpoints. Handlers
    /// publish create / update / delete events here after the database
    /// write succeeds; subscribers (the watch path in
    /// [`table::list_response_live`]) fan them out to clients as
    /// `WatchEvent` JSON.
    pub bus: std::sync::Arc<bus::Bus>,
}

/// Run the superkube control plane server
pub async fn run(
    db_url: &str,
    host: &str,
    port: u16,
    pod_cidr: &str,
    service_cidr: &str,
    containerd_socket: &str,
    runtime: &str,
) -> anyhow::Result<()> {
    // Create database pool
    tracing::info!("Connecting to database...");
    let pool = crate::db::create_pool(db_url).await?;
    tracing::info!("Database connection established");

    // Run migrations automatically on startup
    tracing::info!("Running database migrations...");
    sqlx::migrate!("./migrations").run(&pool).await?;
    tracing::info!("Database migrations completed");

    tracing::info!("pod CIDR: {}, service CIDR: {}", pod_cidr, service_cidr);

    // Per-controller leases coordinate work when multiple `superkube server`
    // processes share a Postgres. SQLite mode short-circuits to "always own
    // the lease" because only one process is touching the DB.
    let backend = Backend::from_url(db_url);
    let leases = LeaseManager::new(pool.clone(), backend);
    let instance_id = leases.holder().to_string();
    if backend == Backend::Postgres {
        tracing::info!(
            "multi-master mode: holder={} (per-controller leases active)",
            leases.holder()
        );
    }

    // Watch event bus. Postgres mode wires it up with a Replicator so
    // mutations on this server fan out to peers via `pg_notify`; SQLite
    // mode is single-process so the bus stays local.
    let bus = std::sync::Arc::new(match backend {
        Backend::Postgres => {
            tracing::info!("watch bus: replicating across servers via LISTEN/NOTIFY");
            bus::Bus::new_replicated(bus::Replicator {
                pool: pool.clone(),
                instance_id: instance_id.clone(),
                channel: bus::DEFAULT_NOTIFY_CHANNEL.to_string(),
            })
        }
        Backend::Sqlite => bus::Bus::new(),
    });

    // Create app state
    let state = Arc::new(AppState {
        pool: pool.clone(),
        service_cidr: service_cidr.to_string(),
        bus: bus.clone(),
    });

    // Postgres-only background tasks: LISTEN for peer events, prune the
    // change_log periodically.
    if backend == Backend::Postgres {
        spawn_change_log_listener(db_url.to_string(), pool.clone(), bus.clone(), instance_id.clone());
        spawn_change_log_pruner(pool.clone());
    }

    // Build router
    let app = Router::new()
        .merge(routes::api_routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    // Start background controllers
    let controller = ControllerManager::new(pool.clone(), leases.clone(), state.bus.clone());
    let scheduler = Scheduler::new(pool.clone(), leases.clone(), state.bus.clone());

    tokio::spawn(async move {
        controller.run().await;
    });

    tokio::spawn(async move {
        scheduler.run().await;
    });

    // Start server
    let addr = format!("{}:{}", host, port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("Kais API server listening on {}", addr);

    // Embedded node agent: register the host running `superkube server` as a node.
    // This is what makes a single `superkube server` invocation a complete cluster.
    // The agent connects back to ourselves via 127.0.0.1 so we don't depend on
    // resolvable DNS for the bind host.
    spawn_embedded_node(
        port,
        pod_cidr.to_string(),
        containerd_socket.to_string(),
        runtime.to_string(),
    );

    axum::serve(listener, app).await?;

    Ok(())
}

fn spawn_embedded_node(
    port: u16,
    pod_cidr: String,
    containerd_socket: String,
    runtime: String,
) {
    let node_name = crate::util::detect_hostname();
    let server_url = format!("http://127.0.0.1:{}", port);

    // Mark this node as the cluster's control-plane so kubectl shows it under
    // the "control-plane" role rather than <none>.
    let mut labels = HashMap::new();
    labels.insert("node-role.kubernetes.io/control-plane".to_string(), "".to_string());
    labels.insert(
        "kubernetes.io/hostname".to_string(),
        node_name.clone(),
    );

    tokio::spawn(async move {
        // Tiny delay to let the API listener accept connections before the
        // agent's first registration POST.
        tokio::time::sleep(Duration::from_millis(300)).await;
        tracing::info!(
            "starting embedded node agent ({}, runtime={}, socket={})",
            node_name,
            runtime,
            containerd_socket
        );
        if let Err(e) = crate::node::run_full(
            &node_name,
            &server_url,
            &containerd_socket,
            labels,
            &pod_cidr,
            &runtime,
        )
        .await
        {
            tracing::error!("embedded node agent exited: {}", e);
        }
    });
}

/// Spawn the Postgres LISTEN task. It connects with a dedicated `PgPool`
/// (separate from the shared `AnyPool`, because `PgListener` requires a
/// concrete `PgPool`), `LISTEN`s on the kais channel, and on each notify
/// fetches the originating row from `change_log`. Events whose
/// `instance_id` matches our own are skipped — those are echoes of what
/// we already broadcast locally — and everything else is republished to
/// the local bus so watchers in this process see peer mutations.
///
/// On connection drops we sleep and reconnect; transient blips don't kill
/// the server but watchers may briefly miss events (kubectl reconnects
/// every 30 minutes anyway, and the change_log row itself is durable for
/// the prune TTL).
fn spawn_change_log_listener(
    db_url: String,
    fetch_pool: sqlx::AnyPool,
    bus: std::sync::Arc<bus::Bus>,
    instance_id: String,
) {
    use sqlx::postgres::{PgListener, PgPoolOptions};
    tokio::spawn(async move {
        loop {
            let listen_pool = match PgPoolOptions::new()
                .max_connections(1)
                .connect(&db_url)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        "watch listener: failed to open Postgres connection ({}); retrying in 5s",
                        e
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            let mut listener = match PgListener::connect_with(&listen_pool).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("watch listener: PgListener::connect failed ({}); retrying", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            if let Err(e) = listener.listen(bus::DEFAULT_NOTIFY_CHANNEL).await {
                tracing::warn!("watch listener: LISTEN failed ({}); retrying", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
            tracing::info!(
                "watch listener: LISTEN {} (instance_id={})",
                bus::DEFAULT_NOTIFY_CHANNEL,
                instance_id
            );

            loop {
                let notif = match listener.recv().await {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(
                            "watch listener: recv failed ({}); reconnecting",
                            e
                        );
                        break;
                    }
                };
                let id = notif.payload();
                match crate::db::change_log::fetch(&fetch_pool, id).await {
                    Ok(Some(row)) => {
                        if row.instance_id == instance_id {
                            continue; // our own echo
                        }
                        // Re-construct the ChangeEvent and republish locally.
                        let object: serde_json::Value =
                            serde_json::from_str(&row.object).unwrap_or(serde_json::Value::Null);
                        let event_type = match row.event_type.as_str() {
                            "ADDED" => bus::WatchEventType::Added,
                            "MODIFIED" => bus::WatchEventType::Modified,
                            "DELETED" => bus::WatchEventType::Deleted,
                            other => {
                                tracing::warn!("watch listener: unknown event_type {}", other);
                                continue;
                            }
                        };
                        // Static-str kinds: we only emit the canonical names
                        // from handlers, so map back via a small whitelist.
                        // When wiring a new resource into the bus, ALSO add
                        // its kind here — peer events for unlisted kinds
                        // are dropped at debug.
                        let kind: &'static str = match row.kind.as_str() {
                            // Core
                            "Pod" => "Pod",
                            "Service" => "Service",
                            "Node" => "Node",
                            "Namespace" => "Namespace",
                            "ConfigMap" => "ConfigMap",
                            "Secret" => "Secret",
                            "ServiceAccount" => "ServiceAccount",
                            // apps/v1
                            "Deployment" => "Deployment",
                            "StatefulSet" => "StatefulSet",
                            "DaemonSet" => "DaemonSet",
                            // rbac.authorization.k8s.io/v1
                            "Role" => "Role",
                            "RoleBinding" => "RoleBinding",
                            "ClusterRole" => "ClusterRole",
                            "ClusterRoleBinding" => "ClusterRoleBinding",
                            other => {
                                tracing::debug!(
                                    "watch listener: ignoring event for unhandled kind {}",
                                    other
                                );
                                continue;
                            }
                        };
                        bus.republish_local(bus::ChangeEvent {
                            kind,
                            namespace: row.namespace,
                            name: row.name,
                            event_type,
                            object,
                        });
                    }
                    Ok(None) => {
                        // Pruned out from under us — slow listener; nothing
                        // we can do. Watchers will re-list on reconnect.
                        tracing::debug!("watch listener: notify {} no longer in change_log", id);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "watch listener: change_log fetch {} failed: {}",
                            id,
                            e
                        );
                    }
                }
            }
        }
    });
}

/// Periodically delete `change_log` rows older than the TTL. 5 minutes is
/// well past kubectl's longest watch reconnect interval, so a peer that
/// misses a notify and looks up an evicted row just falls back to a
/// re-list — it never silently loses a mutation.
fn spawn_change_log_pruner(pool: sqlx::AnyPool) {
    const PRUNE_TTL_SECS: i64 = 300;
    const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PRUNE_INTERVAL);
        ticker.tick().await; // skip immediate first tick
        loop {
            ticker.tick().await;
            match crate::db::change_log::prune_older_than(&pool, PRUNE_TTL_SECS).await {
                Ok(n) if n > 0 => {
                    tracing::debug!("watch change_log: pruned {} rows older than {}s", n, PRUNE_TTL_SECS);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("watch change_log: prune failed: {}", e);
                }
            }
        }
    });
}
