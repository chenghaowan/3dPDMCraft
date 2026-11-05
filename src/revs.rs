//! 修订创建核心：先传对象、后写行、再写 meta 事件（设计文档 §3 不变量）。
//! init 基线 / watcher 变更 / 手动重扫共用这一条路径。

use crate::db::{part_head, utc_now};
use crate::engine::EngineCtx;
use anyhow::{Context, Result};
use sqlx::{Pool, Row, Sqlite};

#[derive(Debug, Clone, serde::Serialize)]
pub struct RepInput {
    pub kind: String,
    pub rel_path: String,
    pub object_hash: String,
    pub size: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct RevInfo {
    pub id: String,
    pub part_id: String,
    pub part_name: String,
    pub seq: i64,
    pub status: String,
    pub created_at: String,
    pub reps: Vec<RepInput>,
}

pub async fn ensure_part(db: &Pool<Sqlite>, name: &str, now: &str) -> Result<String> {
    if let Some(id) = crate::db::part_id_by_name(db, name).await? {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO parts (id, name, created_at) VALUES (?,?,?)")
        .bind(&id)
        .bind(name)
        .bind(now)
        .execute(db)
        .await?;
    Ok(id)
}

/// 建一次修订。seq = head+1；旧 head 降级 archived；冲突子修订（同 parent 多子）在 M1 对账时标 conflict。
pub async fn create_revision(
    ctx: &EngineCtx,
    part_id: &str,
    part_name: &str,
    message: &str,
    reps: Vec<RepInput>,
) -> Result<RevInfo> {
    if reps.is_empty() {
        anyhow::bail!("create_revision: 无表示");
    }
    let now = utc_now();
    let rev_id = uuid::Uuid::new_v4().to_string();

    // 1) 上传对象（去重：objects 表已 done 则跳过；两设备并发传同 hash = 同 key 同内容，幂等）
    for rep in &reps {
        let done: Option<String> = sqlx::query("SELECT upload_state FROM objects WHERE hash = ?")
            .bind(&rep.object_hash)
            .fetch_optional(&ctx.db)
            .await?
            .map(|r| r.get::<String, _>("upload_state"));
        if done.as_deref() == Some("done") {
            continue;
        }
        let src = ctx.root.join(&rep.rel_path);
        let key = crate::storage::Storage::object_key(&rep.object_hash);
        if !ctx.store.exists(&key).await {
            ctx.store
                .put_file(&key, &src)
                .await
                .with_context(|| format!("上传 {}", rep.rel_path))?;
        }
        sqlx::query(
            "INSERT INTO objects (hash,size,upload_state,uploaded_at) VALUES (?,?,?,?) \
             ON CONFLICT(hash) DO UPDATE SET upload_state=excluded.upload_state, uploaded_at=excluded.uploaded_at, size=excluded.size",
        )
        .bind(&rep.object_hash)
        .bind(rep.size as i64)
        .bind("done")
        .bind(&now)
        .execute(&ctx.db)
        .await?;
    }

    // 2) 修订行 + 表示 + 文件指针（单事务）
    let (seq, parent_id) = match part_head(&ctx.db, part_id).await? {
        Some((hid, s)) => (s + 1, Some(hid)),
        None => (1, None),
    };
    let mut tx = ctx.db.begin().await?;
    if parent_id.is_some() {
        sqlx::query("UPDATE revisions SET status='archived' WHERE part_id=? AND status='current'")
            .bind(part_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO revisions (id,part_id,seq,parent_id,author,device_id,message,status,pinned,created_at) \
         VALUES (?,?,?,?,?,?,?,?,0,?)",
    )
    .bind(&rev_id)
    .bind(part_id)
    .bind(seq)
    .bind(&parent_id)
    .bind(&ctx.author)
    .bind(&ctx.device_id)
    .bind(message)
    .bind("current")
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    for rep in &reps {
        sqlx::query(
            "INSERT OR IGNORE INTO representations (revision_id,kind,rel_path,object_hash,size) VALUES (?,?,?,?,?)",
        )
        .bind(&rev_id)
        .bind(&rep.kind)
        .bind(&rep.rel_path)
        .bind(&rep.object_hash)
        .bind(rep.size as i64)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO files (rel_path,part_id,kind,current_revision_id,base_revision_id,local_state,content_hash,updated_at) \
             VALUES (?,?,?,?,?,'synced',?,?) \
             ON CONFLICT(rel_path) DO UPDATE SET current_revision_id=excluded.current_revision_id, \
             base_revision_id=excluded.base_revision_id, content_hash=excluded.content_hash, \
             local_state='synced', updated_at=excluded.updated_at",
        )
        .bind(&rep.rel_path)
        .bind(part_id)
        .bind(&rep.kind)
        .bind(&rev_id)
        .bind(&rev_id)
        .bind(&rep.object_hash)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    // 3) meta 事件（失败仅告警：本地修订已成立，事件对账在 M1 补）
    if let Err(e) = crate::sync::emit_revision_event(
        ctx, &rev_id, part_name, seq, parent_id.clone(), message, &now, &reps,
    )
    .await
    {
        tracing::warn!("写 meta 事件失败: {e}");
    }

    tracing::info!("part {part_name} 修订 v{seq} ({message}), {} 个表示", reps.len());
    Ok(RevInfo {
        id: rev_id,
        part_id: part_id.into(),
        part_name: part_name.into(),
        seq,
        status: "current".into(),
        created_at: now,
        reps,
    })
}
