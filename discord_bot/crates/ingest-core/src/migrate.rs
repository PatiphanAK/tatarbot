//! Applies the `ingest` schema migrations. Like the runtime's runner: embedded files,
//! version order, one transaction each, SHA-256 checksums in `ingest._migrations` (separate
//! from the bot's `_sqlx_migrations`, so the two never see each other's versions), and an
//! advisory lock against concurrent runs. Editing an applied file is an error.

use sha2::{Digest, Sha256};
use sqlx::{Connection, PgPool, Row};

/// Arbitrary, fixed: the advisory lock every runner of these migrations takes.
const LOCK_KEY: i64 = 0x1A6E_5700_0001;

struct Migration {
    version: i64,
    description: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 20261104000000,
    description: "ingest",
    sql: include_str!("../migrations/20261104000000_ingest.sql"),
}];

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("migration {0} was applied but is missing here")]
    Missing(i64),
    #[error("migration {0} was modified after it was applied")]
    Modified(i64),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Applies pending migrations; returns the versions applied.
pub async fn migrate(pool: &PgPool) -> Result<Vec<i64>, MigrateError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *conn)
        .await?;
    let result = apply(&mut conn).await;
    let unlock = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *conn)
        .await;
    let applied = result?;
    unlock?;
    Ok(applied)
}

async fn apply(conn: &mut sqlx::PgConnection) -> Result<Vec<i64>, MigrateError> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS ingest;
         CREATE TABLE IF NOT EXISTS ingest._migrations (
             version     BIGINT      PRIMARY KEY,
             description TEXT        NOT NULL,
             checksum    BYTEA       NOT NULL,
             applied_at  TIMESTAMPTZ NOT NULL DEFAULT now()
         );",
    )
    .execute(&mut *conn)
    .await?;
    let rows = sqlx::query("SELECT version, checksum FROM ingest._migrations")
        .fetch_all(&mut *conn)
        .await?;
    let mut applied = std::collections::HashMap::new();
    for row in rows {
        applied.insert(row.get::<i64, _>(0), row.get::<Vec<u8>, _>(1));
    }
    for (version, checksum) in &applied {
        let known = MIGRATIONS
            .iter()
            .find(|m| m.version == *version)
            .ok_or(MigrateError::Missing(*version))?;
        if &checksum_of(known.sql) != checksum {
            return Err(MigrateError::Modified(*version));
        }
    }
    let mut done = Vec::new();
    for m in MIGRATIONS {
        if applied.contains_key(&m.version) {
            continue;
        }
        tracing::info!(
            version = m.version,
            description = m.description,
            "applying ingest migration"
        );
        let mut tx = conn.begin().await?;
        sqlx::raw_sql(m.sql).execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO ingest._migrations (version, description, checksum) VALUES ($1, $2, $3)",
        )
        .bind(m.version)
        .bind(m.description)
        .bind(checksum_of(m.sql))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        done.push(m.version);
    }
    Ok(done)
}

fn checksum_of(sql: &str) -> Vec<u8> {
    Sha256::digest(sql.as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_ordered_and_unique() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        let mut sorted = versions.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(versions, sorted);
    }

    #[test]
    fn migrations_never_mention_discord() {
        for m in MIGRATIONS {
            assert!(!m.sql.to_lowercase().contains("discord"), "{}", m.version);
        }
    }
}
