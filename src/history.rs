//! 历史 + 回滚（设计文档 §6.5）：回滚 = current 指针移动（零上传），工作区从目标修订落盘；
//! 此后再编辑的修订 parent 指向回滚点（head 查询按 status='current'）。

use crate::db::{self, utc_now};
use crate::engine::EngineCtx;
use crate::scan;
use crate::storage::Storage;
use anyhow::{Context, Result};
use sqlx::Row;

#[derive(serde::Serialize)]
pub struct RepRow {
    pub kind: String,
    pub rel_path: String,
    pub object_hash: String,
    pub size: i64,
}

#[derive(serde::Serialize)]
pub struct RevRow {
    pub id: String,
    pub seq: i64,
    pub status: String,
    pub author: String,
    pub message: String,
    pub created_at: String,
    pub reps: Vec<RepRow>,
}

pub async fn list_history(ctx: &EngineCtx, part_name: &str) -> Result<Vec<RevRow>> {
    let revs_ = sqlx::query(
        "SELECT v.id, v.seq, v.status, v.author, v.message, v.created_at \
         FROM revisions v JOIN parts p ON p.id = v.part_id \
         WHERE p.name = ? AND p.deleted_at IS NULL ORDER BY v.seq",
    )
    .bind(part_name)
    .fetch_all(&ctx.db)
    .await?;
    let mut out = Vec::new();
    for r in revs_ {
        let id: String = r.get("id");
        let reps = sqlx::query(
            "SELECT kind, rel_path, object_hash, size FROM representations WHERE revision_id = ?",
        )
        .bind(&id)
        .fetch_all(&ctx.db)
        .await?;
        out.push(RevRow {
            id,
            seq: r.get("seq"),
            status: r.get("status"),
            author: r.get("author"),
            message: r.get("message"),
            created_at: r.get("created_at"),
            reps: reps
                .into_iter()
                .map(|x| RepRow {
                    kind: x.get("kind"),
                    rel_path: x.get("rel_path"),
                    object_hash: x.get("object_hash"),
                    size: x.get("size"),
                })
                .collect(),
        });
    }
    Ok(out)
}

/// 回滚到 part 的 target_seq。返回替换落盘的文件数。
pub async fn rollback(ctx: &EngineCtx, part_name: &str, target_seq: i64) -> Result<usize> {
    let part_id = db::part_id_by_name(&ctx.db, part_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("零件不存在: {part_name}"))?;
    let target = sqlx::query("SELECT id FROM revisions WHERE part_id=? AND seq=?")
        .bind(&part_id)
        .bind(target_seq)
        .fetch_optional(&ctx.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("修订不存在: {part_name} v{target_seq}"))?;
    let target_id: String = target.get("id");

    // 目标点的有效表示：每个 rel_path 取 seq<=target 的最新 representation
    let rows = sqlx::query(
        "SELECT r.rel_path, r.kind, r.object_hash, r.size \
         FROM representations r JOIN revisions v ON v.id = r.revision_id \
         WHERE v.part_id=? AND v.seq<=? ORDER BY v.seq DESC",
    )
    .bind(&part_id)
    .bind(target_seq)
    .fetch_all(&ctx.db)
    .await?;
    let mut effective: Vec<RepRow> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in rows {
        let rel: String = r.get("rel_path");
        if seen.insert(rel.clone()) {
            effective.push(RepRow {
                kind: r.get("kind"),
                rel_path: rel,
                object_hash: r.get("object_hash"),
                size: r.get("size"),
            });
        }
    }

    // 落盘：内容不同 → 下载对象 → 验 BLAKE3 → 原子替换（§5）；相同 → 仅对齐指针
    let tmp_dir = ctx.root.join(".pdm").join("tmp");
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let now = utc_now();
    let mut replaced = 0usize;
    for rep in &effective {
        let cur = sqlx::query("SELECT content_hash FROM files WHERE rel_path = ?")
            .bind(&rep.rel_path)
            .fetch_optional(&ctx.db)
            .await?;
        let cur_hash: Option<String> = match cur {
            Some(r) => r.get("content_hash"),
            None => None,
        };
        if cur_hash.as_deref() != Some(rep.object_hash.as_str()) {
            let tmp = tmp_dir.join(format!("{}.part", uuid::Uuid::new_v4()));
            ctx.store
                .get_to_file(&Storage::object_key(&rep.object_hash), &tmp)
                .await?;
            let (h, _) = scan::hash_file(&tmp)?;
            if h != rep.object_hash {
                let _ = tokio::fs::remove_file(&tmp).await;
                anyhow::bail!("下载对象校验失败: {}", rep.rel_path);
            }
            let dest = ctx.root.join(&rep.rel_path);
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::rename(&tmp, &dest)
                .await
                .with_context(|| format!("替换 {}", rep.rel_path))?;
            replaced += 1;
        }
        upsert_file_pointer(ctx, &part_id, rep, &target_id, &now).await?;
    }

    // 指针：高于目标且为 current 的降级，目标升级为 current（回滚后 head = target）
    sqlx::query("UPDATE revisions SET status='archived' WHERE part_id=? AND status='current' AND seq>?")
        .bind(&part_id)
        .bind(target_seq)
        .execute(&ctx.db)
        .await?;
    sqlx::query("UPDATE revisions SET status='current' WHERE part_id=? AND seq=?")
        .bind(&part_id)
        .bind(target_seq)
        .execute(&ctx.db)
        .await?;

    tracing::info!("part {part_name} 回滚到 v{target_seq}, 落盘 {replaced} 个文件");
    Ok(replaced)
}

async fn upsert_file_pointer(
    ctx: &EngineCtx,
    part_id: &str,
    rep: &RepRow,
    rev_id: &str,
    now: &str,
) -> Result<()> {
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
    .bind(rev_id)
    .bind(rev_id)
    .bind(&rep.object_hash)
    .bind(now)
    .execute(&ctx.db)
    .await?;
    Ok(())
}
