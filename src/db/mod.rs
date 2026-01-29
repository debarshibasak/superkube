mod repository;

pub use repository::*;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Create a connection pool to PostgreSQL
pub async fn create_pool(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;

    Ok(pool)
}

/// Run database migrations
pub async fn migrate(database_url: &str) -> anyhow::Result<()> {
    let pool = create_pool(database_url).await?;

    // Run embedded migrations
    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(())
}
