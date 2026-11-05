//! 历史裁剪（设计文档 §7）：dry-run 默认先行；对象删除走保护规则 + gc 事件广播。
//! 保护规则（§7.2）：非 current、非 base/current 引用、无 open 冲突引用、
//! seq <= head_seq - N 且 created_at < now - T 天、未 pin。

use crate::db::utc_now;
use crate::engine::EngineCtx;
use crate::storage::Storage;
use anyhow::Result;
use serde::Serialize;
use sqlx::Row;

#[derive(Serialize, Clone)]
pub struct PruneItem {
    pub part: String,
    pub seq: i64,
    pub revision_id: String,
    pub created_at: String,
    pub bytes: i64,
}

#[derive(Serialize)]
pub struct PruneReport {
    pub dry_run: bool,
    pub items: Vec<PruneItem>,
    pub total_bytes: i64,
    pub deleted_objects: usize,
    pub kept_by_reference: usize,
}

pub async fn prune(ctx: &EngineCtx, dry_run: bool, n: i64, t_days: i64) -> Result<PruneReport> {
    let now = chrono::Utc::now();
    let cutoff = (now - chrono::Duration::days(t_days)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // 候选：archived + 窗口外 + 未 pin + 不被 files 指针引用 + 不被 open 冲突引用
    let rows = sqlx::query(
        "SELECT v.id, v.seq, v.created_at, p.name AS part, p.id AS part_id \
         FROM revisions v JOIN parts p ON p.id = v.part_id \
         WHERE v.status='archived' AND v.pinned=0 AND v.created_at < ? \
           AND v.seq <= (SELECT COALESCE(MAX(seq),0) FROM revisions h WHERE h.part_id=v.part_id AND h.status='current') - ? \
           AND v.id NOT IN (SELECT COALESCE(current_revision_id,'') FROM files WHERE deleted_at IS NULL \
                            UNION SELECT COALESCE(base_revision_id,'') FROM files WHERE deleted_at IS NULL) \
           AND v.id NOT IN (SELECT local_revision_id FROM conflicts WHERE status='open') \
           AND v.id NOT IN (SELECT remote_revision_id FROM conflicts WHERE status='open') \
         ORDER BY p.name, v.seq",
    )
    .bind(&cutoff)
    .bind(n)
    .fetch_all(&ctx.db)
    .await?;

    let mut items = Vec::new();
    for r in &rows {
        let rid: String = r.get("id");
        let bytes: i64 = sqlx::query(
            "SELECT COALESCE(SUM(size),0) FROM representations WHERE revision_id=?",
        )
        .bind(&rid)
        .fetch_one(&ctx.db)
        .await?
        .get(0);
        items.push(PruneItem {
            part: r.get("part"),
            seq: r.get("seq"),
            revision_id: rid,
            created_at: r.get("created_at"),
            bytes,
        });
    }
    let total_bytes: i64 = items.iter().map(|i| i.bytes).sum();

    let mut report = PruneReport {
        dry_run,
        items: items.clone(),
        total_bytes,
        deleted_objects: 0,
        kept_by_reference: 0,
    };
    if dry_run || items.is_empty() {
        return Ok(report);
    }

    // 保留侧引用计数：任何未裁剪修订引用的 hash 不能删（§7.4 幂等顺序）。
    // 全量载入 (revision, hash) 对，在内存里做集合差，避免动态 SQL（sqlx 0.9 注入审计）。
    let pairs = sqlx::query("SELECT revision_id, object_hash FROM representations")
        .fetch_all(&ctx.db)
        .await?;
    let pruned_set: std::collections::HashSet<&str> =
        items.iter().map(|i| i.revision_id.as_str()).collect();
    let kept_hashes: std::collections::HashSet<String> = pairs
        .iter()
        .filter(|r| !pruned_set.contains(r.get::<String, _>("revision_id").as_str()))
        .map(|r| r.get::<String, _>("object_hash"))
        .collect();

    let mut delete_hashes: Vec<(String, i64)> = Vec::new();
    for item in &items {
        let reps = sqlx::query("SELECT object_hash, size FROM representations WHERE revision_id=?")
            .bind(&item.revision_id)
            .fetch_all(&ctx.db)
            .await?;
        for rep in reps {
            let hash: String = rep.get("object_hash");
            let size: i64 = rep.get("size");
            if kept_hashes.contains(&hash) {
                report.kept_by_reference += 1;
            } else {
                delete_hashes.push((hash, size));
            }
        }
    }

    // 执行：OSS 删对象 → SQLite 置残根 → 写 gc 事件（§7.4）
    let now_str = utc_now();
    let mut deleted_objects = 0usize;
    for (hash, _size) in &delete_hashes {
        let key = Storage::object_key(hash);
        match ctx.store.op.delete(&key).await {
            Ok(()) => deleted_objects += 1,
            Err(e) => tracing::warn!("删除对象 {key}（可能已不存在）: {e}"),
        }
        sqlx::query("UPDATE objects SET deleted_at=? WHERE hash=?")
            .bind(&now_str)
            .bind(hash)
            .execute(&ctx.db)
            .await?;
    }
    for item in &items {
        sqlx::query("DELETE FROM representations WHERE revision_id=?")
            .bind(&item.revision_id)
            .execute(&ctx.db)
            .await?;
        sqlx::query("UPDATE revisions SET status='pruned' WHERE id=?")
            .bind(&item.revision_id)
            .execute(&ctx.db)
            .await?;
    }

    let event = serde_json::json!({
        "type": "gc",
        "pruned_revision_ids": items.iter().map(|i| i.revision_id.clone()).collect::<Vec<_>>(),
        "deleted_object_hashes": delete_hashes.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>(),
        "device_id": ctx.device_id, "created_at": now_str,
    });
    let key = format!("meta/gc/{}.json", uuid::Uuid::new_v4());
    if let Err(e) = ctx.store.put_bytes(&key, serde_json::to_vec_pretty(&event)?).await {
        tracing::warn!("写 gc 事件失败: {e}");
    }

    report.deleted_objects = deleted_objects;
    tracing::info!(
        "裁剪执行：{} 个修订、{} 个对象、释放 {} bytes（保留侧引用 {} 个 hash 不删）",
        items.len(), deleted_objects, total_bytes, report.kept_by_reference
    );
    Ok(report)
}
