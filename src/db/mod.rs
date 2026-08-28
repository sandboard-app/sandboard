//! Pluggable board database (SQLx).
//!
//! Persistence: `Board` boots from SQLite (default) or Postgres via
//! [`DurableBoardStore`] and flushes row updates. `sandboard.json` is a one-shot
//! import source when the DB is empty. Hot list/snapshot/lease paths use
//! denormalized columns and indexed SQL (`query_*` on [`BoardStore`]); agent
//! engines stay unchanged.

#![allow(dead_code)]

mod codec;
mod config;
mod durable;
mod postgres;
mod sqlite;
mod store;

pub use config::{
    apply_database_url_override, parse_database_url, BoardDatabaseConfig, DatabaseBackend,
};
pub use durable::DurableBoardStore;
#[allow(unused_imports)] // trait is the public seam; callers use concrete stores today
pub use store::{BoardStore, StoreError};
#[allow(unused_imports)] // public API for operators / later callers
pub use postgres::PostgresBoardStore;
#[allow(unused_imports)] // public API; Board goes through DurableBoardStore
pub use sqlite::SqliteBoardStore;

// Re-exported for callers / tests.
#[allow(unused_imports)]
pub use config::{DatabaseUrl, DEFAULT_DATABASE_URL, ENV_DATABASE_URL};

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::ConnectOptions;
use std::str::FromStr;

/// Embedded versioned migrations (`migrations/` at the crate root).
/// Same SQL applies to SQLite and Postgres (portable types / indexes).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Open a SQLite pool and apply migrations.
pub async fn connect_sqlite_migrated(url: &str) -> Result<SqlitePool, StoreError> {
    let parsed = parse_database_url(url)?;
    if parsed.backend() != DatabaseBackend::Sqlite {
        return Err(StoreError::WrongBackend {
            expected: DatabaseBackend::Sqlite,
            got: parsed.backend(),
        });
    }
    let options = SqliteConnectOptions::from_str(parsed.as_str())
        .map_err(|e| StoreError::Connect(e.to_string()))?
        .create_if_missing(true)
        .disable_statement_logging();
    // `sqlite::memory:` is per-connection unless shared; sqlx already enables
    // shared_cache when parsing `:memory:`, and assigns a stable name per
    // options value so one pool's connections see the same DB.
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .map_err(|e| StoreError::Connect(e.to_string()))?;
    MIGRATOR
        .run(&pool)
        .await
        .map_err(|e| StoreError::Migrate(e.to_string()))?;
    Ok(pool)
}

/// Open a Postgres pool and apply the same migrations as SQLite.
pub async fn connect_postgres_migrated(url: &str) -> Result<PgPool, StoreError> {
    let parsed = parse_database_url(url)?;
    if parsed.backend() != DatabaseBackend::Postgres {
        return Err(StoreError::WrongBackend {
            expected: DatabaseBackend::Postgres,
            got: parsed.backend(),
        });
    }
    let options = PgConnectOptions::from_str(parsed.as_str())
        .map_err(|e| StoreError::Connect(e.to_string()))?
        .disable_statement_logging();
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .map_err(|e| StoreError::Connect(e.to_string()))?;
    MIGRATOR
        .run(&pool)
        .await
        .map_err(|e| StoreError::Migrate(e.to_string()))?;
    Ok(pool)
}

/// Apply migrations to an already-open SQLite pool (e.g. tests that build their
/// own options). Prefer [`connect_sqlite_migrated`] for the normal path.
#[cfg(test)]
pub async fn migrate_sqlite(pool: &SqlitePool) -> Result<(), StoreError> {
    MIGRATOR
        .run(pool)
        .await
        .map_err(|e| StoreError::Migrate(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    #[tokio::test]
    async fn migrations_apply_to_sqlite_memory() {
        let pool = connect_sqlite_migrated("sqlite::memory:")
            .await
            .expect("memory sqlite migrates");

        // Tables from 001_board.sql exist.
        for table in ["meta", "items", "item_blockers", "stories"] {
            let row = sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table)
                .fetch_optional(&pool)
                .await
                .expect("query");
            assert!(row.is_some(), "missing table {table}");
        }

        // Index for the dispatch hot path is present.
        let idx = sqlx::query(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_items_dispatch_queue'",
        )
        .fetch_optional(&pool)
        .await
        .expect("index query");
        assert!(idx.is_some(), "dispatch queue index missing");

        // t3 denorm indexes.
        for name in ["idx_items_backlog_ready", "idx_items_lease_sweep"] {
            let idx = sqlx::query(
                "SELECT name FROM sqlite_master WHERE type='index' AND name=?",
            )
            .bind(name)
            .fetch_optional(&pool)
            .await
            .expect("index query");
            assert!(idx.is_some(), "{name} missing");
        }

        // Denorm columns exist.
        let cols: Vec<String> = sqlx::query("PRAGMA table_info(items)")
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(cols.iter().any(|c| c == "non_retired_child_count"));
        assert!(cols.iter().any(|c| c == "open_blocker_count"));

        // Round-trip a meta key to prove the schema is writable.
        sqlx::query("INSERT INTO meta (key, value) VALUES (?, ?)")
            .bind("next_id")
            .bind("1")
            .execute(&pool)
            .await
            .expect("insert meta");
        let value: String = sqlx::query("SELECT value FROM meta WHERE key = ?")
            .bind("next_id")
            .fetch_one(&pool)
            .await
            .expect("select meta")
            .get("value");
        assert_eq!(value, "1");
    }

    #[tokio::test]
    async fn connect_sqlite_migrated_creates_file_db() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-db-migrate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sandboard.db");
        let url = format!("sqlite:{}", path.display());
        let pool = connect_sqlite_migrated(&url).await.expect("connect+migrate");
        let count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM sqlite_master WHERE type='table'")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
        assert!(count >= 4, "expected board tables, got {count}");
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn connect_postgres_migrated_rejects_sqlite_url() {
        let err = connect_postgres_migrated("sqlite::memory:")
            .await
            .expect_err("sqlite URL must not open as postgres");
        assert!(matches!(
            err,
            StoreError::WrongBackend {
                expected: DatabaseBackend::Postgres,
                got: DatabaseBackend::Sqlite,
            }
        ));
    }

    /// When `SANDBOARD_TEST_DATABASE_URL` is a reachable `postgres://` / `postgresql://`
    /// URL, apply migrations there. Offline CI leaves the env unset and skips.
    #[tokio::test]
    async fn migrations_apply_to_postgres_when_available() {
        let url = match std::env::var("SANDBOARD_TEST_DATABASE_URL") {
            Ok(u) if !u.trim().is_empty() => u,
            _ => {
                eprintln!("skipping: set SANDBOARD_TEST_DATABASE_URL to exercise Postgres migrations");
                return;
            }
        };
        let parsed = parse_database_url(&url).expect("SANDBOARD_TEST_DATABASE_URL must parse");
        assert_eq!(
            parsed.backend(),
            DatabaseBackend::Postgres,
            "SANDBOARD_TEST_DATABASE_URL must be postgres:// or postgresql://"
        );

        let pool = connect_postgres_migrated(parsed.as_str())
            .await
            .expect("postgres connect+migrate");

        for table in ["meta", "items", "item_blockers", "stories"] {
            let (exists,): (bool,) = sqlx::query_as(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM information_schema.tables
                    WHERE table_schema = 'public' AND table_name = $1
                )
                "#,
            )
            .bind(table)
            .fetch_one(&pool)
            .await
            .expect("table exists query");
            assert!(exists, "missing table {table}");
        }

        sqlx::query("INSERT INTO meta (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
            .bind("next_id")
            .bind("1")
            .execute(&pool)
            .await
            .expect("insert meta");
        let value: String = sqlx::query("SELECT value FROM meta WHERE key = $1")
            .bind("next_id")
            .fetch_one(&pool)
            .await
            .expect("select meta")
            .get("value");
        assert_eq!(value, "1");

        pool.close().await;
    }
}
