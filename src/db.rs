//! SQLite（schema v2，设计文档 §3）。WAL + 单连接池（单写者，操作串行）。
//! 迁移用 PRAGMA user_version 顺序执行。

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};
use std::path::Path;
use std::time::Duration;

const MIGRATION_V1: &str = r#"
CREATE TABLE parts (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  forked_from_revision_id TEXT,
  deleted_at  TEXT,
  created_at  TEXT NOT NULL
);

CREATE TABLE revisions (
  id          TEXT PRIMARY KEY,
  part_id     TEXT NOT NULL REFERENCES parts(id),
  seq         INTEGER NOT NULL,
  parent_id   TEXT,
  author      TEXT NOT NULL,
  device_id   TEXT NOT NULL,
  message     TEXT NOT NULL DEFAULT '',
  status      TEXT NOT NULL DEFAULT 'current',
  pinned      INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL
);
CREATE INDEX idx_rev_part ON revisions(part_id, seq);

CREATE TABLE representations (
  revision_id TEXT NOT NULL REFERENCES revisions(id),
  kind        TEXT NOT NULL,
  rel_path    TEXT NOT NULL,
  object_hash TEXT NOT NULL,
  size        INTEGER NOT NULL,
  PRIMARY KEY (revision_id, kind, rel_path)
);

CREATE TABLE files (
  rel_path            TEXT PRIMARY KEY,
  part_id             TEXT NOT NULL REFERENCES parts(id),
  kind                TEXT NOT NULL,
  current_revision_id TEXT,
  base_revision_id    TEXT,
  local_state         TEXT NOT NULL DEFAULT 'synced',
  content_hash        TEXT,
  updated_at          TEXT
);

CREATE TABLE objects (
  hash         TEXT PRIMARY KEY,
  size         INTEGER NOT NULL,
  upload_state TEXT NOT NULL DEFAULT 'pending',
  deleted_at   TEXT,
  uploaded_at  TEXT
);

CREATE TABLE reservations (
  part_id    TEXT NOT NULL,
  device_id  TEXT NOT NULL,
  author     TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  created_at TEXT NOT NULL
);
"#;

const MIGRATION_V2: &str = r#"
ALTER TABLE revisions ADD COLUMN origin TEXT NOT NULL DEFAULT 'local';
ALTER TABLE files ADD COLUMN deleted_at TEXT;
CREATE TABLE conflicts (
  id TEXT PRIMARY KEY,
  part_id TEXT NOT NULL REFERENCES parts(id),
  local_revision_id TEXT NOT NULL,
  remote_revision_id TEXT NOT NULL,
  base_revision_id TEXT,
  status TEXT NOT NULL DEFAULT 'open',
  choice TEXT,
  created_at TEXT NOT NULL,
  resolved_at TEXT
);
CREATE TABLE pending_updates (
  rel_path TEXT PRIMARY KEY,
  revision_id TEXT NOT NULL,
  object_hash TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE applied_events (key TEXT PRIMARY KEY, applied_at TEXT NOT NULL);
"#;

pub async fn open_db(ws_root: &Path) -> Result<Pool<Sqlite>> {
    let db_path = ws_root.join(".pdm").join("pdm.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("打开数据库 {}", db_path.display()))?;

    let ver: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
    // sqlx 0.9 对动态 SQL 有注入审计 lint，PRAGMA 不能拼字符串，按版本静态枚举
    if ver < 1 {
        sqlx::query(MIGRATION_V1).execute(&pool).await?;
        sqlx::query("PRAGMA user_version = 1").execute(&pool).await?;
        tracing::info!("数据库迁移到 v1");
    }
    if ver < 2 {
        sqlx::query(MIGRATION_V2).execute(&pool).await?;
        sqlx::query("PRAGMA user_version = 2").execute(&pool).await?;
        tracing::info!("数据库迁移到 v2");
    }
    if ver < 3 {
        sqlx::query(
            "CREATE TABLE dependencies ( \
               part_id TEXT NOT NULL, \
               depends_on TEXT NOT NULL, \
               source_rel_path TEXT NOT NULL, \
               PRIMARY KEY (part_id, depends_on, source_rel_path) \
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query("PRAGMA user_version = 3").execute(&pool).await?;
        tracing::info!("数据库迁移到 v3");
    }
    Ok(pool)
}

pub fn utc_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ---------- 常用查询 ----------

pub async fn part_id_by_name(db: &Pool<Sqlite>, name: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT id FROM parts WHERE name = ? AND deleted_at IS NULL")
        .bind(name)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("id")))
}

/// 零件当前 head（status='current' 的修订）
pub async fn part_head(db: &Pool<Sqlite>, part_id: &str) -> Result<Option<(String, i64)>> {
    let row = sqlx::query(
        "SELECT id, seq FROM revisions WHERE part_id = ? AND status = 'current' ORDER BY seq DESC LIMIT 1",
    )
    .bind(part_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| (r.get::<String, _>("id"), r.get::<i64, _>("seq"))))
}
