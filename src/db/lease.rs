use std::time::Duration;

use chrono::Utc;
use sqlx::AnyPool;
use uuid::Uuid;

/// Which database backend the pool is talking to. Postgres is the only
/// backend where multiple servers can share state; SQLite is single-process
/// by design, so the lease layer skips the DB round-trip entirely there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

impl Backend {
    pub fn from_url(url: &str) -> Self {
        if url.starts_with("sqlite:") {
            Backend::Sqlite
        } else {
            Backend::Postgres
        }
    }
}

/// Acquires and renews short-lived named leases against the `leases` table.
/// Each `superkube server` process generates a fresh holder UUID at startup
/// and races peers for jobs like `controller/deployment` and `scheduler`.
///
/// In SQLite mode `try_acquire` is a no-op that returns `true` — single
/// process, no contention, no DB write needed.
#[derive(Clone)]
pub struct LeaseManager {
    pool: AnyPool,
    holder: String,
    backend: Backend,
}

impl LeaseManager {
    pub fn new(pool: AnyPool, backend: Backend) -> Self {
        Self {
            pool,
            holder: Uuid::new_v4().to_string(),
            backend,
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// Try to acquire (or renew) a named lease for `ttl`. Returns `true`
    /// when this caller now owns the lease and may proceed with the work.
    ///
    /// The semantics:
    ///   - row missing       → INSERT, we own it
    ///   - we already hold it → renew (refresh expires_at)
    ///   - someone else holds, expired → take over
    ///   - someone else holds, fresh   → fail; come back next tick
    pub async fn try_acquire(&self, name: &str, ttl: Duration) -> bool {
        if self.backend == Backend::Sqlite {
            return true;
        }

        let now = Utc::now();
        let expires = now
            + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(30));
        let now_str = now.to_rfc3339();
        let expires_str = expires.to_rfc3339();

        // Portable UPSERT — both Postgres and SQLite parse this. The DO
        // UPDATE only fires (and rows_affected > 0) if we already hold the
        // lease or the existing one has expired. Otherwise the WHERE blocks
        // the write and rows_affected returns 0.
        let result = sqlx::query(
            "INSERT INTO leases (name, holder, expires_at) VALUES (?, ?, ?) \
             ON CONFLICT (name) DO UPDATE \
                SET holder = excluded.holder, expires_at = excluded.expires_at \
                WHERE leases.holder = excluded.holder OR leases.expires_at < ?",
        )
        .bind(name)
        .bind(&self.holder)
        .bind(&expires_str)
        .bind(&now_str)
        .execute(&self.pool)
        .await;

        match result {
            Ok(r) => r.rows_affected() > 0,
            Err(e) => {
                tracing::warn!("lease acquire {} failed: {}", name, e);
                false
            }
        }
    }
}
