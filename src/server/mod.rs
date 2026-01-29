mod api;
mod controller;
mod routes;
mod scheduler;

use axum::Router;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

pub use controller::ControllerManager;
pub use scheduler::Scheduler;

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
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

    axum::serve(listener, app).await?;

    Ok(())
}
