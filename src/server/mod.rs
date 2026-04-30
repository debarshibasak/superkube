mod api;
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

pub use controller::ControllerManager;
pub use scheduler::Scheduler;

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub pool: AnyPool,
}

/// Run the kais control plane server
pub async fn run(db_url: &str, host: &str, port: u16) -> anyhow::Result<()> {
    // Create database pool
    tracing::info!("Connecting to database...");
    let pool = crate::db::create_pool(db_url).await?;
    tracing::info!("Database connection established");

    // Run migrations automatically on startup
    tracing::info!("Running database migrations...");
    sqlx::migrate!("./migrations").run(&pool).await?;
    tracing::info!("Database migrations completed");

    // Create app state
    let state = Arc::new(AppState { pool: pool.clone() });

    // Build router
    let app = Router::new()
        .merge(routes::api_routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    // Start background controllers
    let controller = ControllerManager::new(pool.clone());
    let scheduler = Scheduler::new(pool.clone());

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

    // Embedded node agent: register the host running `kais server` as a node.
    // This is what makes a single `kais server` invocation a complete cluster.
    // The agent connects back to ourselves via 127.0.0.1 so we don't depend on
    // resolvable DNS for the bind host.
    spawn_embedded_node(port);

    axum::serve(listener, app).await?;

    Ok(())
}

fn spawn_embedded_node(port: u16) {
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
        tracing::info!("starting embedded node agent ({})", node_name);
        if let Err(e) = crate::node::run(
            &node_name,
            &server_url,
            "/run/containerd/containerd.sock",
            labels,
        )
        .await
        {
            tracing::error!("embedded node agent exited: {}", e);
        }
    });
}
