//! M1 同步核心（设计文档 §4.1 events manifest、§5 更新分级、§6 冲突管理）。
//!
//! 事件类型（均为不可变 JSON 对象）：
//! - meta/revisions/*.json   修订
//! - meta/tombstones/*.json  删除墓碑
//! - meta/reservations/*.json 意向声明 claim/release
//! - meta/resolutions/*.json 冲突解决（winner/loser）
//! - meta/gc/*.json          裁剪记录
//!
//! 本地 SQLite 是缓存/索引；OSS 事件是事实源。applied_events 表保证拉取幂等。

use crate::db::{part_head, utc_now};
use crate::engine::{EngineCtx, WorkspaceHandle};
use crate::revs::RepInput;
use crate::scan;
use crate::storage::Storage;
use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::Row;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Default, Debug, serde::Serialize)]
pub struct PullReport {
    pub new_revisions: usize,
    pub applied_updates: usize,
    pub deferred: usize,
    pub conflicts: usize,
    pub tombstones: usize,
    pub resolutions: usize,
    pub reservations: usize,
    pub gc: usize,
}

// ---------- 事件结构 ----------

#[derive(Deserialize, Debug, Clone)]
pub struct RevEventReps {
    pub kind: String,
    pub rel_path: String,
    pub object_hash: String,
    pub size: i64,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RevEvent {
    pub revision: RevEventRev,
    pub representations: Vec<RevEventReps>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RevEventRev {
    pub id: String,
    pub part: String,
    pub seq: i64,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct TombstoneEvent {
    pub rel_path: String,
    #[serde(default)]
    pub part: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ReservationEvent {
    pub action: String, // claim | release
    pub part: String,
    pub device_id: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub expires_at: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ResolutionEvent {
    pub part: String,
    pub winner_id: String,
    pub loser_id: String,
    #[serde(default)]
    pub choice: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct GcEvent {
    #[serde(default)]
    pub pruned_revision_ids: Vec<String>,
    #[serde(default)]
    pub deleted_object_hashes: Vec<String>,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub created_at: String,
}

// ---------- 事件读写 ----------

pub async fn fetch_events(store: &Storage, dir: &str) -> Result<Vec<(String, serde_json::Value)>> {
    let keys = store.list_prefix(dir).await?;
    let mut out = Vec::new();
    for key in keys {
        let bytes = store.get_bytes(&key).await?;
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(v) => out.push((key, v)),
            Err(e) => tracing::warn!("解析事件 {key} 失败: {e}"),
        }
    }
    Ok(out)
}

pub async fn mark_applied(ctx: &EngineCtx, key: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO applied_events (key, applied_at) VALUES (?,?)")
        .bind(key)
        .bind(utc_now())
        .execute(&ctx.db)
        .await?;
    Ok(())
}

async fn is_applied(ctx: &EngineCtx, key: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM applied_events WHERE key = ?")
        .bind(key)
        .fetch_one(&ctx.db)
        .await?;
    Ok(n > 0)
}

pub async fn emit_revision_event(
    ctx: &EngineCtx,
    rev_id: &str,
    part_name: &str,
    seq: i64,
    parent_id: Option<String>,
    message: &str,
    created_at: &str,
    reps: &[RepInput],
) -> Result<String> {
    let event = serde_json::json!({
        "type": "revision",
        "revision": {
            "id": rev_id, "part": part_name, "seq": seq, "parent_id": parent_id,
            "author": ctx.author, "device_id": ctx.device_id, "message": message,
            "status": "current", "created_at": created_at,
        },
        "representations": reps,
    });
    let key = format!("meta/revisions/{rev_id}.json");
    ctx.store
        .put_bytes(&key, serde_json::to_vec_pretty(&event)?)
        .await?;
    mark_applied(ctx, &key).await?; // 自己写的事件自己标记，避免 pull 重复处理
    Ok(key)
}

// ---------- 拉取主流程 ----------

pub async fn pull(h: &Arc<WorkspaceHandle>) -> Result<PullReport> {
    let ctx = &h.ctx;
    let mut report = PullReport::default();

    // 1) 修订事件：按 created_at 排序导入
    let mut revs_ev = fetch_events(&ctx.store, "meta/revisions/").await?;
    revs_ev.sort_by_key(|(_, v)| {
        v["revision"]["created_at"].as_str().unwrap_or("").to_string()
    });
    let mut new_by_part: HashMap<String, Vec<String>> = HashMap::new();
    for (key, v) in &revs_ev {
        if is_applied(ctx, key).await? {
            continue;
        }
        match serde_json::from_value::<RevEvent>(v.clone()) {
            Ok(ev) => {
                if import_revision(ctx, &ev).await? {
                    new_by_part
                        .entry(ev.revision.part.clone())
                        .or_default()
                        .push(ev.revision.id.clone());
                    report.new_revisions += 1;
                }
                mark_applied(ctx, key).await?;
            }
            Err(e) => tracing::warn!("修订事件 {key} 结构异常: {e}"),
        }
    }

    // 2) 仅对本轮新导入的修订做分歧判定（linear → 应用；diverged → 冲突）。
    //    已知旧修订不重评：否则已解决的分歧会被重建为冲突。
    for (part_name, new_ids) in &new_by_part {
        match reconcile_part(h, part_name, new_ids).await {
            Ok((applied, deferred, conflicts)) => {
                report.applied_updates += applied;
                report.deferred += deferred;
                report.conflicts += conflicts;
            }
            Err(e) => tracing::warn!("[{}] part {part_name} 对账失败: {e:#}", h.name),
        }
    }

    // 3) 墓碑
    for (key, v) in fetch_events(&ctx.store, "meta/tombstones/").await? {
        if is_applied(ctx, &key).await? {
            continue;
        }
        if let Ok(ev) = serde_json::from_value::<TombstoneEvent>(v) {
            apply_tombstone(ctx, &ev).await?;
            report.tombstones += 1;
        }
        mark_applied(ctx, &key).await?;
    }

    // 4) 意向
    for (key, v) in fetch_events(&ctx.store, "meta/reservations/").await? {
        if is_applied(ctx, &key).await? {
            continue;
        }
        if let Ok(ev) = serde_json::from_value::<ReservationEvent>(v) {
            apply_reservation(ctx, &ev).await?;
            report.reservations += 1;
        }
        mark_applied(ctx, &key).await?;
    }

    // 5) 冲突解决事件
    for (key, v) in fetch_events(&ctx.store, "meta/resolutions/").await? {
        if is_applied(ctx, &key).await? {
            continue;
        }
        if let Ok(ev) = serde_json::from_value::<ResolutionEvent>(v) {
            apply_resolution(ctx, &ev).await?;
            report.resolutions += 1;
        }
        mark_applied(ctx, &key).await?;
    }

    // 6) gc
    for (key, v) in fetch_events(&ctx.store, "meta/gc/").await? {
        if is_applied(ctx, &key).await? {
            continue;
        }
        if let Ok(ev) = serde_json::from_value::<GcEvent>(v) {
            apply_gc(ctx, &ev).await?;
            report.gc += 1;
        }
        mark_applied(ctx, &key).await?;
    }

    // 7) 重试上次推迟的更新
    let retried = retry_pending(ctx).await?;
    report.applied_updates += retried;

    Ok(report)
}

/// 导入一条远端修订行（只插入，不提升、不动文件）。返回是否为新导入。
pub async fn import_revision(ctx: &EngineCtx, ev: &RevEvent) -> Result<bool> {
    let r = &ev.revision;
    let now = utc_now();
    let part_id = crate::revs::ensure_part(&ctx.db, &r.part, &now).await?;
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM revisions WHERE id = ?")
        .bind(&r.id)
        .fetch_one(&ctx.db)
        .await?;
    if exists > 0 {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO revisions (id,part_id,seq,parent_id,author,device_id,message,status,pinned,created_at,origin) \
         VALUES (?,?,?,?,?,?,?,'archived',0,?,'remote')",
    )
    .bind(&r.id)
    .bind(&part_id)
    .bind(r.seq)
    .bind(&r.parent_id)
    .bind(&r.author)
    .bind(&r.device_id)
    .bind(&r.message)
    .bind(if r.created_at.is_empty() { &now } else { &r.created_at })
    .execute(&ctx.db)
    .await?;
    for rep in &ev.representations {
        sqlx::query(
            "INSERT OR IGNORE INTO representations (revision_id,kind,rel_path,object_hash,size) VALUES (?,?,?,?,?)",
        )
        .bind(&r.id)
        .bind(&rep.kind)
        .bind(&rep.rel_path)
        .bind(&rep.object_hash)
        .bind(rep.size)
        .execute(&ctx.db)
        .await?;
    }
    tracing::info!("导入远端修订 part={} v{} ({})", r.part, r.seq, r.author);
    Ok(true)
}

/// part 级对账：本轮新导入的远端修订与本地 head 的关系 → linear 应用 / diverged 冲突
/// 返回 (applied, deferred, new_conflicts)
async fn reconcile_part(
    h: &Arc<WorkspaceHandle>,
    part_name: &str,
    new_ids: &[String],
) -> Result<(usize, usize, usize)> {
    let ctx = &h.ctx;
    let mut applied = 0usize;
    let mut deferred = 0usize;
    let mut conflicts = 0usize;

    // 该 part 在本地已知的、来自远端且 status='archived'（未处理）的修订，按 seq 升序
    let pending = sqlx::query(
        "SELECT v.id, v.seq, v.parent_id FROM revisions v JOIN parts p ON p.id=v.part_id \
         WHERE p.name=? AND v.origin='remote' AND v.status='archived' ORDER BY v.seq",
    )
    .bind(part_name)
    .fetch_all(&ctx.db)
    .await?;
    // 只处理本轮新导入的
    let pending: Vec<_> = pending
        .into_iter()
        .filter(|r| new_ids.contains(&r.get::<String, _>("id")))
        .collect();
    if pending.is_empty() {
        return Ok((0, 0, 0));
    }

    for row in pending {
        let rev_id: String = row.get("id");
        let rev_seq: i64 = row.get("seq");
        let parent_id: Option<String> = row.get("parent_id");
        let head = part_head(&ctx.db, &part_id_of(ctx, part_name).await?).await?;
        let Some((head_id, _head_seq)) = head else {
            // 本地无 head（part 全新）：直接提升
            promote(ctx, part_name, &rev_id).await?;
            let (a, d) = apply_revision_files(ctx, part_name, &rev_id).await?;
            applied += a;
            deferred += d;
            continue;
        };

        if head_id == rev_id {
            continue;
        }
        // 远端链是否包含本地 head（远端领先）
        let remote_ahead = chain_contains(ctx, &rev_id, &head_id).await?;
        // 本地链是否包含远端（本地领先，远端该追我们）
        let local_ahead = chain_contains(ctx, &head_id, &rev_id).await?;

        if remote_ahead {
            // 更新分级（§5）：工作区干净 → 自动应用；脏 → 冲突
            let (a, d) = apply_revision_files(ctx, part_name, &rev_id).await?;
            promote(ctx, part_name, &rev_id).await?;
            applied += a;
            deferred += d;
        } else if local_ahead {
            // 落后事件，无需处理
        } else {
            let part_id = part_id_of(ctx, part_name).await?;
            // §2/§6.4：双方都只含 neutral（派生物）的分歧 → 自动解决（最新者胜），不打扰用户
            let local_only_neutral = only_neutral(ctx, &head_id).await;
            let remote_only_neutral = only_neutral(ctx, &rev_id).await;
            if local_only_neutral && remote_only_neutral {
                auto_resolve_neutral(ctx, part_name, &part_id, &head_id, &rev_id).await?;
                continue;
            }
            // 分歧：同 parent 兄弟（或更深分叉）→ 冲突（§6.3 基线检测）。
            // 同 (local, remote) 对已有记录（含已解决）则不重建。
            let dup: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM conflicts WHERE part_id=? AND local_revision_id=? AND remote_revision_id=?",
            )
            .bind(&part_id)
            .bind(&head_id)
            .bind(&rev_id)
            .fetch_one(&ctx.db)
            .await?;
            let open: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM conflicts WHERE part_id=? AND status='open'",
            )
            .bind(&part_id)
            .fetch_one(&ctx.db)
            .await?;
            if dup == 0 && open == 0 {
                let base = sqlx::query("SELECT parent_id FROM revisions WHERE id=?")
                    .bind(&head_id)
                    .fetch_one(&ctx.db)
                    .await
                    .ok()
                    .map(|r| r.get::<Option<String>, _>("parent_id"))
                    .flatten();
                let cid = uuid::Uuid::new_v4().to_string();
                sqlx::query(
                    "INSERT INTO conflicts (id,part_id,local_revision_id,remote_revision_id,base_revision_id,status,created_at) \
                     VALUES (?,?,?,?,?,'open',?)",
                )
                .bind(&cid)
                .bind(&part_id)
                .bind(&head_id)
                .bind(&rev_id)
                .bind(&base)
                .bind(utc_now())
                .execute(&ctx.db)
                .await?;
                sqlx::query(
                    "UPDATE files SET local_state='conflicted' WHERE part_id=? AND deleted_at IS NULL AND local_state='synced'",
                )
                .bind(&part_id_of(ctx, part_name).await?)
                .execute(&ctx.db)
                .await?;
                conflicts += 1;
                tracing::warn!(
                    "[{}] part {part_name} 冲突：本地 v{} 与远端 v{} 分歧（同基线 {}），等待人工解决",
                    h.name, rev_seq_of(ctx, &head_id).await, rev_seq, base.as_deref().unwrap_or("?")
                );
                let _ = parent_id;
            }
        }
    }
    Ok((applied, deferred, conflicts))
}

async fn part_id_of(ctx: &EngineCtx, part_name: &str) -> Result<String> {
    crate::db::part_id_by_name(&ctx.db, part_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("part 不存在: {part_name}"))
}

async fn rev_seq_of(ctx: &EngineCtx, rev_id: &str) -> i64 {
    sqlx::query("SELECT seq FROM revisions WHERE id=?")
        .bind(rev_id)
        .fetch_optional(&ctx.db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<i64, _>("seq").ok())
        .unwrap_or(0)
}

/// 提升某修订为 part 的 current（原 current 降级 archived）
pub async fn promote(ctx: &EngineCtx, part_name: &str, rev_id: &str) -> Result<()> {
    let part_id = part_id_of(ctx, part_name).await?;
    let mut tx = ctx.db.begin().await?;
    sqlx::query("UPDATE revisions SET status='archived' WHERE part_id=? AND status='current'")
        .bind(&part_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE revisions SET status='current' WHERE id=?")
        .bind(rev_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// 从 start_id 沿 parent 链向上，是否包含 target_id
pub async fn chain_contains(ctx: &EngineCtx, start_id: &str, target_id: &str) -> Result<bool> {
    if start_id == target_id {
        return Ok(true);
    }
    let rows = sqlx::query(
        "WITH RECURSIVE anc(id) AS ( \
           SELECT parent_id FROM revisions WHERE id = ? AND parent_id IS NOT NULL \
           UNION \
           SELECT r.parent_id FROM revisions r JOIN anc a ON r.id = a.id WHERE r.parent_id IS NOT NULL \
         ) SELECT COUNT(*) FROM anc WHERE id = ?",
    )
    .bind(start_id)
    .bind(target_id)
    .fetch_one(&ctx.db)
    .await?;
    let n: i64 = rows.get(0);
    Ok(n > 0)
}

/// 应用某修订的文件（更新分级 §5）：
/// 工作区 hash 已一致 → 仅指针对齐；synced → 下载替换（失败进 pending_updates）；其余状态不动。
/// 返回 (applied, deferred)
pub async fn apply_revision_files(
    ctx: &EngineCtx,
    part_name: &str,
    rev_id: &str,
) -> Result<(usize, usize)> {
    let reps = sqlx::query(
        "SELECT kind, rel_path, object_hash, size FROM representations WHERE revision_id = ?",
    )
    .bind(rev_id)
    .fetch_all(&ctx.db)
    .await?;
    let now = utc_now();
    let mut applied = 0usize;
    let mut deferred = 0usize;
    for rep in reps {
        let rel: String = rep.get("rel_path");
        let hash: String = rep.get("object_hash");
        let kind: String = rep.get("kind");
        let file = sqlx::query("SELECT local_state, content_hash FROM files WHERE rel_path=?")
            .bind(&rel)
            .fetch_optional(&ctx.db)
            .await?;
        let (state, cur_hash): (String, Option<String>) = match file {
            Some(r) => (r.get("local_state"), r.get("content_hash")),
            None => ("missing".into(), None),
        };
        let part_id = part_id_of(ctx, part_name).await?;

        if cur_hash.as_deref() == Some(hash.as_str()) {
            // 内容已一致（含 pull 自己落盘后的回环）：仅指针对齐
            upsert_pointer(ctx, &rel, &part_id, &kind, rev_id, &hash, &now).await?;
            applied += 1;
            continue;
        }
        if state == "synced" || state == "missing" {
            match fetch_and_replace(ctx, &rel, &hash).await {
                Ok(()) => {
                    upsert_pointer(ctx, &rel, &part_id, &kind, rev_id, &hash, &now).await?;
                    applied += 1;
                }
                Err(e) => {
                    // os.replace 失败（CAD 占用等）→ 推迟，角标可见，之后重试（§5 分级）
                    tracing::warn!("[pull] 替换 {rel} 失败，进待更新: {e:#}");
                    sqlx::query(
                        "INSERT INTO pending_updates (rel_path,revision_id,object_hash,created_at) VALUES (?,?,?,?) \
                         ON CONFLICT(rel_path) DO UPDATE SET revision_id=excluded.revision_id, object_hash=excluded.object_hash",
                    )
                    .bind(&rel)
                    .bind(rev_id)
                    .bind(&hash)
                    .bind(&now)
                    .execute(&ctx.db)
                    .await?;
                    deferred += 1;
                }
            }
        } else {
            // modified / conflicted / deleted：不动文件，走冲突或人工路径
        }
    }
    Ok((applied, deferred))
}

/// 指针对齐（init/回滚/解决共用）
pub async fn upsert_pointer(
    ctx: &EngineCtx,
    rel: &str,
    part_id: &str,
    kind: &str,
    rev_id: &str,
    hash: &str,
    now: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO files (rel_path,part_id,kind,current_revision_id,base_revision_id,local_state,content_hash,updated_at) \
         VALUES (?,?,?,?,?,'synced',?,?) \
         ON CONFLICT(rel_path) DO UPDATE SET current_revision_id=excluded.current_revision_id, \
         base_revision_id=excluded.base_revision_id, content_hash=excluded.content_hash, \
         local_state='synced', updated_at=excluded.updated_at",
    )
    .bind(rel)
    .bind(part_id)
    .bind(kind)
    .bind(rev_id)
    .bind(rev_id)
    .bind(hash)
    .bind(now)
    .execute(&ctx.db)
    .await?;
    Ok(())
}

/// 下载对象 → 验 BLAKE3 → 原子替换
pub async fn fetch_and_replace(ctx: &EngineCtx, rel: &str, hash: &str) -> Result<()> {
    let tmp_dir = ctx.root.join(".pdm").join("tmp");
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let tmp = tmp_dir.join(format!("{}.part", uuid::Uuid::new_v4()));
    ctx.store
        .get_to_file(&Storage::object_key(hash), &tmp)
        .await?;
    let (h, _) = scan::hash_file(&tmp)?;
    if h != hash {
        let _ = tokio::fs::remove_file(&tmp).await;
        anyhow::bail!("对象校验失败: {rel}");
    }
    let dest = ctx.root.join(rel);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(&tmp, &dest).await?;
    Ok(())
}

/// 目标修订点的有效表示（每个 rel_path 取 seq<=target 的最新 representation）
pub async fn effective_reps_at(
    ctx: &EngineCtx,
    part_id: &str,
    target_seq: i64,
) -> Result<Vec<(String, String, String, i64)>> {
    let rows = sqlx::query(
        "SELECT r.rel_path, r.kind, r.object_hash, r.size \
         FROM representations r JOIN revisions v ON v.id = r.revision_id \
         WHERE v.part_id=? AND v.seq<=? ORDER BY v.seq DESC",
    )
    .bind(part_id)
    .bind(target_seq)
    .fetch_all(&ctx.db)
    .await?;
    let mut out: Vec<(String, String, String, i64)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in rows {
        let rel: String = r.get("rel_path");
        if seen.insert(rel.clone()) {
            out.push((rel, r.get("kind"), r.get("object_hash"), r.get("size")));
        }
    }
    Ok(out)
}

// ---------- 墓碑 ----------

pub async fn emit_tombstone(ctx: &EngineCtx, rel_path: &str, part_name: &str) -> Result<()> {
    let event = serde_json::json!({
        "type": "tombstone", "rel_path": rel_path, "part": part_name,
        "device_id": ctx.device_id, "author": ctx.author, "created_at": utc_now(),
    });
    let key = format!("meta/tombstones/{}.json", uuid::Uuid::new_v4());
    ctx.store.put_bytes(&key, serde_json::to_vec_pretty(&event)?).await?;
    mark_applied(ctx, &key).await?;
    Ok(())
}

async fn apply_tombstone(ctx: &EngineCtx, ev: &TombstoneEvent) -> Result<()> {
    let row = sqlx::query("SELECT local_state, content_hash, part_id, kind FROM files WHERE rel_path=?")
        .bind(&ev.rel_path)
        .fetch_optional(&ctx.db)
        .await?;
    let Some(r) = row else { return Ok(()) };
    let state: String = r.get("local_state");
    let hash: Option<String> = r.get("content_hash");
    let part_id: String = r.get("part_id");
    let kind: String = r.get("kind");
    let now = utc_now();

    if state == "synced" {
        // 按墓碑删本地文件（文件缺失视为已删，幂等）
        let p = ctx.root.join(&ev.rel_path);
        if let Err(e) = tokio::fs::remove_file(&p).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("删除 {} 失败: {e}", ev.rel_path);
            }
        }
        sqlx::query("UPDATE files SET local_state='deleted', deleted_at=?, updated_at=? WHERE rel_path=?")
            .bind(&now)
            .bind(&now)
            .bind(&ev.rel_path)
            .execute(&ctx.db)
            .await?;
        tracing::info!("[tombstone] 删除本地文件 {}", ev.rel_path);
    } else {
        // 本地有改动/已冲突：保留文件，标记冲突状态等人工处理（墓碑先行的例外）
        tracing::warn!("[tombstone] {} 本地状态为 {state}，保留待人工处理", ev.rel_path);
        if state != "conflicted" {
            sqlx::query("UPDATE files SET local_state='conflicted', updated_at=? WHERE rel_path=?")
                .bind(&now)
                .bind(&ev.rel_path)
                .execute(&ctx.db)
                .await?;
        }
    }
    let _ = (hash, part_id, kind);
    Ok(())
}

// ---------- 意向声明 ----------

pub async fn claim(ctx: &EngineCtx, part_name: &str) -> Result<()> {
    let expires = (chrono::Utc::now() + chrono::Duration::hours(48)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let event = serde_json::json!({
        "type": "reservation", "action": "claim", "part": part_name,
        "device_id": ctx.device_id, "author": ctx.author,
        "expires_at": expires, "created_at": utc_now(),
    });
    let key = format!("meta/reservations/{}.json", uuid::Uuid::new_v4());
    ctx.store.put_bytes(&key, serde_json::to_vec_pretty(&event)?).await?;
    mark_applied(ctx, &key).await?;
    Ok(())
}

pub async fn release(ctx: &EngineCtx, part_name: &str) -> Result<()> {
    let event = serde_json::json!({
        "type": "reservation", "action": "release", "part": part_name,
        "device_id": ctx.device_id, "author": ctx.author,
        "expires_at": "", "created_at": utc_now(),
    });
    let key = format!("meta/reservations/{}.json", uuid::Uuid::new_v4());
    ctx.store.put_bytes(&key, serde_json::to_vec_pretty(&event)?).await?;
    mark_applied(ctx, &key).await?;
    Ok(())
}

async fn apply_reservation(ctx: &EngineCtx, ev: &ReservationEvent) -> Result<()> {
    // reservations.part_id 列存 part 名称（事件以名为键；表无外键）
    if ev.action == "release" {
        sqlx::query("DELETE FROM reservations WHERE part_id=? AND device_id=? AND created_at <= ?")
            .bind(&ev.part)
            .bind(&ev.device_id)
            .bind(if ev.created_at.is_empty() { utc_now() } else { ev.created_at.clone() })
            .execute(&ctx.db)
            .await?;
    } else {
        // claim：同一 (part, device) 只留最新一条
        sqlx::query("DELETE FROM reservations WHERE part_id=? AND device_id=?")
            .bind(&ev.part)
            .bind(&ev.device_id)
            .execute(&ctx.db)
            .await?;
        sqlx::query(
            "INSERT INTO reservations (part_id,device_id,author,expires_at,created_at) VALUES (?,?,?,?,?)",
        )
        .bind(&ev.part)
        .bind(&ev.device_id)
        .bind(&ev.author)
        .bind(&ev.expires_at)
        .bind(if ev.created_at.is_empty() { utc_now() } else { ev.created_at.clone() })
        .execute(&ctx.db)
        .await?;
    }
    Ok(())
}

/// 上传前警示（非阻塞，§6.3）：目标 part 是否有他人的未过期意向
pub async fn warn_foreign_claims(ctx: &EngineCtx, part_name: &str) {
    let now = utc_now();
    let rows = sqlx::query(
        "SELECT author, device_id FROM reservations \
         WHERE part_id=? AND expires_at > ? AND device_id != ?",
    )
    .bind(part_name)
    .bind(&now)
    .bind(&ctx.device_id)
    .fetch_all(&ctx.db)
    .await;
    if let Ok(rows) = rows {
        for r in rows {
            let author: String = r.get("author");
            tracing::warn!("[意向] part {part_name} 正被 {author} 编辑（未过期意向），本次提交未受阻但可能产生冲突");
        }
    }
}

// ---------- 冲突解决 ----------

pub async fn resolve(
    h: &Arc<WorkspaceHandle>,
    conflict_id: &str,
    choice: &str,
    fork_name: Option<String>,
) -> Result<serde_json::Value> {
    let ctx = &h.ctx;
    let c = sqlx::query("SELECT * FROM conflicts WHERE id=? AND status='open'")
        .bind(conflict_id)
        .fetch_optional(&ctx.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("冲突不存在或已解决: {conflict_id}"))?;
    let part_id: String = c.get("part_id");
    let local_rev: String = c.get("local_revision_id");
    let remote_rev: String = c.get("remote_revision_id");
    let part_name: String = sqlx::query("SELECT name FROM parts WHERE id=?")
        .bind(&part_id)
        .fetch_one(&ctx.db)
        .await?
        .get("name");
    let now = utc_now();

    let winner = match choice {
        "mine" => local_rev.clone(),
        "theirs" => remote_rev.clone(),
        "fork" => {
            // 都保留 = 隐式 fork（§6.6）：本地修订存为新零件，原 part 用对方
            let name = fork_name.unwrap_or_else(|| format!("{part_name}.fork"));
            crate::fork::create_fork(ctx, &local_rev, &name).await?;
            remote_rev.clone()
        }
        _ => anyhow::bail!("未知选项: {choice}"),
    };
    let loser = if choice == "mine" { remote_rev.clone() } else { local_rev.clone() };

    // 提升赢家；输家已 archived（导入时即 archived）
    promote(ctx, &part_name, &winner).await?;

    // 工作区对齐赢家内容
    let winner_seq = rev_seq_of(ctx, &winner).await;
    let reps = effective_reps_at(ctx, &part_id, winner_seq).await?;
    for (rel, kind, hash, _size) in &reps {
        let dest = ctx.root.join(rel);
        let wh = if dest.exists() {
            scan::hash_file(&dest).ok().map(|(h, _)| h)
        } else {
            None
        };
        if wh.as_deref() != Some(hash.as_str()) {
            fetch_and_replace(ctx, rel, hash).await?;
        }
        upsert_pointer(ctx, rel, &part_id, kind, &winner, hash, &now).await?;
    }
    sqlx::query("UPDATE files SET local_state='synced', updated_at=? WHERE part_id=? AND local_state='conflicted'")
        .bind(&now)
        .bind(&part_id)
        .execute(&ctx.db)
        .await?;

    // 解决事件广播（把网盘同步，§6.4）
    let event = serde_json::json!({
        "type": "resolution", "part": part_name, "winner_id": winner, "loser_id": loser,
        "choice": choice, "device_id": ctx.device_id, "author": ctx.author, "created_at": now,
    });
    let key = format!("meta/resolutions/{}.json", uuid::Uuid::new_v4());
    ctx.store.put_bytes(&key, serde_json::to_vec_pretty(&event)?).await?;
    mark_applied(ctx, &key).await?;

    sqlx::query("UPDATE conflicts SET status='resolved', choice=?, resolved_at=? WHERE id=?")
        .bind(choice)
        .bind(&now)
        .bind(conflict_id)
        .execute(&ctx.db)
        .await?;
    tracing::info!("冲突 {conflict_id} 已解决（{choice}），part={part_name} winner={winner}");
    Ok(serde_json::json!({"resolved": true, "choice": choice, "winner": winner}))
}

/// 该修订是否只含 neutral 表示（派生物）
async fn only_neutral(ctx: &EngineCtx, rev_id: &str) -> bool {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM representations WHERE revision_id=?")
        .bind(rev_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap_or(0);
    if n == 0 {
        return false;
    }
    let non: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM representations WHERE revision_id=? AND kind != 'neutral'",
    )
    .bind(rev_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap_or(0);
    non == 0
}

async fn rev_created_at(ctx: &EngineCtx, rev_id: &str) -> String {
    sqlx::query("SELECT created_at FROM revisions WHERE id=?")
        .bind(rev_id)
        .fetch_optional(&ctx.db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String, _>("created_at").ok())
        .unwrap_or_default()
}

/// neutral（派生物）分歧自动解决：最新者胜，强制换底并广播（对方同规则收敛）
async fn auto_resolve_neutral(
    ctx: &EngineCtx,
    part_name: &str,
    part_id: &str,
    head_id: &str,
    remote_id: &str,
) -> Result<()> {
    let lt = rev_created_at(ctx, head_id).await;
    let rt = rev_created_at(ctx, remote_id).await;
    let (winner, loser) = if rt > lt {
        (remote_id.to_string(), head_id.to_string())
    } else {
        (head_id.to_string(), remote_id.to_string())
    };
    let wseq = rev_seq_of(ctx, &winner).await;
    if winner != head_id {
        promote(ctx, part_name, &winner).await?;
        let now = utc_now();
        for (rel, kind, hash, _s) in effective_reps_at(ctx, part_id, wseq).await? {
            let dest = ctx.root.join(&rel);
            let wh = dest
                .exists()
                .then(|| scan::hash_file(&dest).ok())
                .flatten()
                .map(|(h, _)| h);
            if wh.as_deref() != Some(hash.as_str()) {
                if let Err(e) = fetch_and_replace(ctx, &rel, &hash).await {
                    tracing::warn!("[neutral 自动解决] 替换 {rel} 失败: {e:#}");
                    continue;
                }
            }
            upsert_pointer(ctx, &rel, part_id, &kind, &winner, &hash, &now).await?;
        }
    }
    let event = serde_json::json!({
        "type": "resolution", "part": part_name, "winner_id": winner, "loser_id": loser,
        "choice": "auto-neutral", "device_id": ctx.device_id, "author": ctx.author,
        "created_at": utc_now(),
    });
    let key = format!("meta/resolutions/{}.json", uuid::Uuid::new_v4());
    ctx.store.put_bytes(&key, serde_json::to_vec_pretty(&event)?).await?;
    mark_applied(ctx, &key).await?;
    tracing::info!("[neutral 自动解决] part {part_name}: 最新修订 v{wseq} 胜出（派生物可再生，无需人工）");
    Ok(())
}

/// 应用他端的解决事件：胜者提升、败者归档（若本地当前正是败者则工作区换底）
async fn apply_resolution(ctx: &EngineCtx, ev: &ResolutionEvent) -> Result<()> {
    let Some(part_id) = crate::db::part_id_by_name(&ctx.db, &ev.part).await? else {
        return Ok(());
    };
    let head = part_head(&ctx.db, &part_id).await?;
    let now = utc_now();

    if let Some((head_id, _)) = &head {
        if head_id == &ev.loser_id {
            // 我方正是败者：换底到胜者
            promote(ctx, &ev.part, &ev.winner_id).await?;
            let wseq = rev_seq_of(ctx, &ev.winner_id).await;
            for (rel, kind, hash, _s) in effective_reps_at(ctx, &part_id, wseq).await? {
                let dest = ctx.root.join(&rel);
                let wh = dest.exists().then(|| scan::hash_file(&dest).ok()).flatten().map(|(h, _)| h);
                if wh.as_deref() != Some(hash.as_str()) {
                    if let Err(e) = fetch_and_replace(ctx, &rel, &hash).await {
                        tracing::warn!("解决事件应用：替换 {rel} 失败: {e:#}");
                        continue;
                    }
                }
                upsert_pointer(ctx, &rel, &part_id, &kind, &ev.winner_id, &hash, &now).await?;
            }
            sqlx::query("UPDATE files SET local_state='synced', updated_at=? WHERE part_id=? AND local_state='conflicted'")
                .bind(&now)
                .bind(&part_id)
                .execute(&ctx.db)
                .await?;
            tracing::info!("[resolution] part {} 本地为败者，已换底到胜者 {}", ev.part, ev.winner_id);
        } else if head_id == &ev.winner_id {
            // 我方即胜者：冲突关闭，文件状态复位（此前只归档败者导致 conflicted 卡住）
            sqlx::query("UPDATE files SET local_state='synced', updated_at=? WHERE part_id=? AND local_state='conflicted'")
                .bind(&now)
                .bind(&part_id)
                .execute(&ctx.db)
                .await?;
            tracing::info!("[resolution] part {} 本地为胜者，冲突关闭", ev.part);
        } else {
            // 本地已另行分叉：仅确保败者 archived，冲突留给本地队列
            sqlx::query("UPDATE revisions SET status='archived' WHERE id=? AND status='current'")
                .bind(&ev.loser_id)
                .execute(&ctx.db)
                .await?;
        }
    }
    // 关闭对应冲突记录
    sqlx::query(
        "UPDATE conflicts SET status='resolved', resolved_at=? \
         WHERE status='open' AND part_id=? AND (remote_revision_id=? OR local_revision_id=? OR remote_revision_id=? OR local_revision_id=?)",
    )
    .bind(&now)
    .bind(&part_id)
    .bind(&ev.loser_id)
    .bind(&ev.loser_id)
    .bind(&ev.winner_id)
    .bind(&ev.winner_id)
    .execute(&ctx.db)
    .await?;
    Ok(())
}

// ---------- 裁剪 gc 事件应用 ----------

async fn apply_gc(ctx: &EngineCtx, ev: &GcEvent) -> Result<()> {
    let now = utc_now();
    for rid in &ev.pruned_revision_ids {
        // 只归档态的裁剪；本地 current 命中则跳过并告警（不该发生：裁剪保护规则含 current）
        let n = sqlx::query("UPDATE revisions SET status='pruned' WHERE id=? AND status='archived'")
            .bind(rid)
            .execute(&ctx.db)
            .await?
            .rows_affected();
        if n == 0 {
            tracing::warn!("[gc] 修订 {rid} 非归档态，跳过本地裁剪");
            continue;
        }
        sqlx::query("DELETE FROM representations WHERE revision_id=?").bind(rid).execute(&ctx.db).await?;
    }
    for hash in &ev.deleted_object_hashes {
        sqlx::query("UPDATE objects SET deleted_at=? WHERE hash=? AND deleted_at IS NULL")
            .bind(&now)
            .bind(hash)
            .execute(&ctx.db)
            .await?;
    }
    tracing::info!("[gc] 应用裁剪：{} 修订 / {} 对象", ev.pruned_revision_ids.len(), ev.deleted_object_hashes.len());
    Ok(())
}

// ---------- 待更新重试 ----------

async fn retry_pending(ctx: &EngineCtx) -> Result<usize> {
    let rows = sqlx::query("SELECT rel_path, revision_id, object_hash FROM pending_updates")
        .fetch_all(&ctx.db)
        .await?;
    let mut ok = 0usize;
    for r in rows {
        let rel: String = r.get("rel_path");
        let rev: String = r.get("revision_id");
        let hash: String = r.get("object_hash");
        match fetch_and_replace(ctx, &rel, &hash).await {
            Ok(()) => {
                let part_id: String = sqlx::query("SELECT part_id FROM revisions WHERE id=?")
                    .bind(&rev)
                    .fetch_one(&ctx.db)
                    .await?
                    .get("part_id");
                let kind: String = sqlx::query("SELECT kind FROM representations WHERE revision_id=? AND rel_path=?")
                    .bind(&rev)
                    .bind(&rel)
                    .fetch_one(&ctx.db)
                    .await?
                    .get("kind");
                upsert_pointer(ctx, &rel, &part_id, &kind, &rev, &hash, &utc_now()).await?;
                sqlx::query("DELETE FROM pending_updates WHERE rel_path=?").bind(&rel).execute(&ctx.db).await?;
                ok += 1;
            }
            Err(_) => continue, // 仍被占用，下次再试
        }
    }
    Ok(ok)
}

// ---------- 引擎用的便捷封装 ----------

/// 提交本地修订的前后意向（claim → create → release），非阻塞警示他人意向
pub async fn with_reservation_guard(
    ctx: &EngineCtx,
    part_name: &str,
    f: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    warn_foreign_claims(ctx, part_name).await;
    if let Err(e) = claim(ctx, part_name).await {
        tracing::warn!("写意向失败: {e}");
    }
    let r = f.await;
    if let Err(e) = release(ctx, part_name).await {
        tracing::warn!("释放意向失败: {e}");
    }
    r
}

pub fn rep_input(kind: String, rel_path: String, object_hash: String, size: u64) -> RepInput {
    RepInput { kind, rel_path, object_hash, size }
}

#[allow(dead_code)]
pub fn unused(_: PathBuf) {}
