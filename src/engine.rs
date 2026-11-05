//! 工作区引擎：watcher + 上传路径 + 回环抑制（设计文档 §5）。
//! 幂等规则：工作区 hash == files.content_hash → no-op（回滚落盘、pull 落盘都靠它防回环）。

use crate::config::WsConfig;
use crate::revs::{self, RepInput};
use crate::scan;
use crate::storage::Storage;
use anyhow::Result;
use sqlx::{Pool, Row, Sqlite};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct EngineCtx {
    pub db: Pool<Sqlite>,
    pub store: Storage,
    pub root: PathBuf,
    pub author: String,
    pub device_id: String,
}

pub struct WorkspaceHandle {
    pub name: String,
    pub cfg: WsConfig,
    pub ignore_rules: Vec<String>,
    pub ctx: EngineCtx,
}

pub async fn load_handle(name: &str, root: &Path) -> Result<WorkspaceHandle> {
    let cfg = crate::config::load_ws_config(root)?;
    let pool = crate::db::open_db(root).await?;
    let store = Storage::new(&cfg.oss, root)?;
    let device_id = crate::config::ensure_device_id()?;
    Ok(WorkspaceHandle {
        name: name.to_string(),
        ignore_rules: scan::load_ignore_rules(root),
        ctx: EngineCtx {
            db: pool,
            store,
            root: root.to_path_buf(),
            author: cfg.user.clone(),
            device_id,
        },
        cfg,
    })
}

/// 启动 watcher（notify-debouncer 独立线程）+ 批处理消费任务。
/// 批内同 part 的多文件变更聚合为一个修订（§5：一次静默窗口 = 一次修订）。
pub fn spawn_watcher(h: Arc<WorkspaceHandle>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
    let root = h.ctx.root.clone();
    let name = h.name.clone();
    std::thread::spawn(move || {
        use notify_debouncer_full::notify::RecursiveMode;
        let res = notify_debouncer_full::new_debouncer(
            std::time::Duration::from_secs(2),
            None,
            {
                let tx = tx.clone();
                move |events: notify_debouncer_full::DebounceEventResult| {
                    if let Ok(events) = events {
                        for ev in events {
                            for p in &ev.paths {
                                let _ = tx.send(p.clone());
                            }
                        }
                    }
                }
            },
        );
        match res {
            Ok(mut deb) => {
                if let Err(e) = deb.watch(&root, RecursiveMode::Recursive) {
                    tracing::error!("[{name}] watch 失败: {e}");
                    return;
                }
                tracing::info!("[{name}] watcher 已启动: {}", root.display());
                loop {
                    std::thread::park(); // 保活 debouncer
                }
            }
            Err(e) => tracing::error!("[{name}] debouncer 初始化失败: {e}"),
        }
    });

    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            // 聚合窗口：再收 500ms 内到达的路径
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(500);
            while let Ok(p) = tokio::time::timeout_at(deadline, rx.recv()).await {
                if let Some(p) = p {
                    batch.push(p);
                }
            }
            batch.sort();
            batch.dedup();
            if let Err(e) = reconcile_batch(&h, &batch).await {
                tracing::warn!("[{}] 处理变更失败: {e:#}", h.name);
            }
        }
    });
}

/// 批量对账：哈希 → 幂等过滤 → 按 part 聚合 → 每部分一个修订。
/// 删除 → 写墓碑（M1）；修订前后写/释放意向声明（M1，§6.3）。
/// 返回创建的修订数。
pub async fn reconcile_batch(h: &Arc<WorkspaceHandle>, paths: &[PathBuf]) -> Result<usize> {
    let mut changed: HashMap<String, Vec<RepInput>> = HashMap::new();
    let mut deletions: Vec<String> = Vec::new();
    for p in paths {
        let rel = match p.strip_prefix(&h.ctx.root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if scan::is_ignored(&rel, &h.ignore_rules) {
            continue;
        }
        if !(rel.starts_with("native/") || rel.starts_with("neutral/")) {
            continue;
        }
        let meta = match tokio::fs::metadata(p).await {
            Ok(m) => m,
            Err(_) => {
                deletions.push(rel);
                continue;
            }
        };
        if !meta.is_file() {
            continue;
        }
        let (hash, size) = scan::hash_file_async(p.clone()).await?;
        if let Some(row) = sqlx::query("SELECT content_hash FROM files WHERE rel_path = ?")
            .bind(&rel)
            .fetch_optional(&h.ctx.db)
            .await?
        {
            let ch: Option<String> = row.get("content_hash");
            if ch.as_deref() == Some(hash.as_str()) {
                continue; // 幂等 / 回环抑制（§5）
            }
        }
        let (kind, part_key) = scan::part_key_for(&rel)
            .ok_or_else(|| anyhow::anyhow!("无法归组: {rel}"))?;
        changed
            .entry(part_key)
            .or_default()
            .push(RepInput { kind, rel_path: rel, object_hash: hash, size });
    }

    // 删除 → 墓碑事件 + 本地标记（pull 对账后广播）
    for rel in deletions {
        handle_deletion(h, &rel).await;
    }

    let mut n = 0;
    for (part_key, reps) in changed {
        let now = crate::db::utc_now();
        let part_id = revs::ensure_part(&h.ctx.db, &part_key, &now).await?;
        // 意向声明：claim → 提交 → release（§6.3，非阻塞警示）
        crate::sync::warn_foreign_claims(&h.ctx, &part_key).await;
        let _ = crate::sync::claim(&h.ctx, &part_key).await;
        let r = revs::create_revision(&h.ctx, &part_id, &part_key, "modified", reps).await;
        let _ = crate::sync::release(&h.ctx, &part_key).await;
        r?;
        n += 1;
    }
    Ok(n)
}

/// 本地文件删除：仅对已同步文件写墓碑；有本地改动的保留并标记冲突
async fn handle_deletion(h: &Arc<WorkspaceHandle>, rel: &str) {
    let row = sqlx::query(
        "SELECT f.local_state, f.deleted_at, p.name AS part FROM files f JOIN parts p ON p.id=f.part_id WHERE f.rel_path=?",
    )
    .bind(rel)
    .fetch_optional(&h.ctx.db)
    .await;
    let Ok(Some(row)) = row else { return };
    let state: String = row.get("local_state");
    let deleted: Option<String> = row.get("deleted_at");
    let part: String = row.get("part");
    if deleted.is_some() {
        return; // 已是墓碑，幂等
    }
    let now = crate::db::utc_now();
    if state == "synced" {
        if let Err(e) = crate::sync::emit_tombstone(&h.ctx, rel, &part).await {
            tracing::warn!("[{}] 写墓碑失败 {rel}: {e}", h.name);
            return;
        }
        let _ = sqlx::query("UPDATE files SET local_state='deleted', deleted_at=?, updated_at=? WHERE rel_path=?")
            .bind(&now)
            .bind(&now)
            .bind(rel)
            .execute(&h.ctx.db)
            .await;
        tracing::info!("[{}] 墓碑: {rel} (part {part})", h.name);
    } else if state != "deleted" {
        tracing::warn!("[{}] {} 本地状态 {state}，删除待人工确认（暂不写墓碑）", h.name, rel);
        let _ = sqlx::query("UPDATE files SET local_state='conflicted', updated_at=? WHERE rel_path=?")
            .bind(&now)
            .bind(rel)
            .execute(&h.ctx.db)
            .await;
    }
}

/// 手动全量重扫
pub async fn rescan(h: &Arc<WorkspaceHandle>) -> Result<usize> {
    let files = scan::scan_workspace(&h.ctx.root, &h.ignore_rules).await?;
    let paths: Vec<PathBuf> = files
        .iter()
        .map(|f| h.ctx.root.join(&f.rel_path))
        .collect();
    reconcile_batch(h, &paths).await
}
