pub mod change_log;
mod lease;
mod repository;

pub use lease::{Backend, LeaseManager};
pub use repository::*;

use sqlx::any::{install_default_drivers, AnyPoolOptions};
use sqlx::AnyPool;

/// Create a connection pool to the database.
///
/// Supported URLs:
///   - PostgreSQL: `postgres://...` or `postgresql://...`
///   - SQLite: `sqlite://path/to/file.db` or `sqlite::memory:`
///
/// SQLite databases are created automatically if the file does not exist.
pub async fn create_pool(database_url: &str) -> anyhow::Result<AnyPool> {
    install_default_drivers();

    if is_sqlite_url(database_url) {
        ensure_sqlite_file(database_url)?;
    }

    let pool = AnyPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;

    Ok(pool)
}

fn is_sqlite_url(url: &str) -> bool {
    url.starts_with("sqlite:")
}

/// SQLite refuses to open a file that does not exist unless we create it first
/// or pass `?mode=rwc`. We touch the file here so the URL can stay simple.
fn ensure_sqlite_file(url: &str) -> anyhow::Result<()> {
    // Strip "sqlite:" prefix and any leading slashes after the scheme.
    let after_scheme = url.trim_start_matches("sqlite:");
    let path = after_scheme.trim_start_matches("//");

    // Drop query string if present.
    let path = path.split('?').next().unwrap_or(path);

    if path.is_empty() || path == ":memory:" {
        return Ok(());
    }

    let path = std::path::Path::new(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }

    if !path.exists() {
        std::fs::File::create(path)?;
    }

    Ok(())
}
