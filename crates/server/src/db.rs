use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{FromRow, PgPool};

pub async fn migrate(pool: &PgPool) -> Result<()> {
    // Portable YSQL/PostgreSQL DDL; no extensions, advisory locks, or LISTEN/NOTIFY.
    for statement in [
        "CREATE TABLE IF NOT EXISTS definitions (id TEXT PRIMARY KEY, revision BIGINT NOT NULL, spec JSONB NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        "CREATE TABLE IF NOT EXISTS runs (id TEXT PRIMARY KEY, definition_id TEXT NOT NULL, definition_name TEXT NOT NULL, revision BIGINT NOT NULL, spec JSONB NOT NULL, input JSONB NOT NULL, files JSONB NOT NULL DEFAULT '[]', status TEXT NOT NULL, worker_id TEXT, session_id TEXT, attempt_id TEXT, result JSONB, error TEXT, exit_code INTEGER, cancel_requested BOOLEAN NOT NULL DEFAULT FALSE, idempotency_key TEXT UNIQUE, request_hash TEXT, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), started_at TIMESTAMPTZ, finished_at TIMESTAMPTZ)",
        "CREATE INDEX IF NOT EXISTS runs_queue_idx ON runs (status, created_at)",
        "ALTER TABLE runs ADD COLUMN IF NOT EXISTS cancel_requested_at TIMESTAMPTZ",
        "CREATE TABLE IF NOT EXISTS workers (id TEXT PRIMARY KEY, name TEXT NOT NULL, version TEXT NOT NULL, tags JSONB NOT NULL, drivers JSONB NOT NULL, status TEXT NOT NULL, enabled BOOLEAN NOT NULL DEFAULT TRUE, session_id TEXT, last_seen TIMESTAMPTZ NOT NULL DEFAULT now())",
        "CREATE TABLE IF NOT EXISTS run_logs (run_id TEXT PRIMARY KEY, text TEXT NOT NULL DEFAULT '', truncated BOOLEAN NOT NULL DEFAULT FALSE)",
        "CREATE TABLE IF NOT EXISTS tokens (id TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL, token_hash TEXT NOT NULL UNIQUE, revoked BOOLEAN NOT NULL DEFAULT FALSE, created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        "CREATE TABLE IF NOT EXISTS artifacts (id TEXT PRIMARY KEY, run_id TEXT, attempt_id TEXT, name TEXT NOT NULL, size BIGINT NOT NULL, object_key TEXT NOT NULL UNIQUE, status TEXT NOT NULL DEFAULT 'pending', created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        "CREATE INDEX IF NOT EXISTS artifacts_run_idx ON artifacts (run_id)",
        "CREATE TABLE IF NOT EXISTS platform_settings (key TEXT PRIMARY KEY, value JSONB NOT NULL)",
    ] {
        // YSQL catalog changes can conflict with asynchronous index/catalog work.
        // Every statement is idempotent; only retry explicit transaction conflicts.
        let mut retries = 0;
        loop {
            match sqlx::query(statement).execute(pool).await {
                Ok(_) => break,
                Err(error)
                    if retries < 8
                        && error.as_database_error().is_some_and(|db| {
                            matches!(db.code().as_deref(), Some("40001" | "40P01"))
                        }) =>
                {
                    retries += 1;
                    tracing::warn!(retries, "schema catalog conflict; retrying statement");
                    tokio::time::sleep(std::time::Duration::from_millis(250 * retries)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, FromRow)]
pub struct Definition {
    pub id: String,
    pub revision: i64,
    pub spec: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Run {
    pub id: String,
    pub definition_id: String,
    pub definition_name: String,
    pub revision: i64,
    pub spec: Value,
    pub input: Value,
    pub files: Value,
    pub status: String,
    pub worker_id: Option<String>,
    #[serde(skip)]
    pub session_id: Option<String>,
    pub attempt_id: Option<String>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub cancel_requested: bool,
    #[serde(skip)]
    pub request_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}
#[derive(Debug, Serialize, FromRow)]
pub struct Worker {
    pub id: String,
    pub name: String,
    pub version: String,
    pub tags: Value,
    pub drivers: Value,
    pub status: String,
    pub enabled: bool,
    pub last_seen: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Artifact {
    pub id: String,
    pub run_id: Option<String>,
    pub attempt_id: Option<String>,
    pub name: String,
    pub size: i64,
    #[serde(skip)]
    pub object_key: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

pub async fn fail_active(pool: &PgPool, session_id: Option<&str>, reason: &str) -> Result<()> {
    sqlx::query("UPDATE runs SET status='failed', error=$1, finished_at=now() WHERE status IN ('assigned','running') AND ($2::TEXT IS NULL OR session_id=$2)")
        .bind(reason).bind(session_id).execute(pool).await?;
    Ok(())
}
