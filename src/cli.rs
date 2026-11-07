//! CLI 子命令实现（与 Web UI / API 同层调用底层模块，不经过 HTTP）。
//! CLI 与 pdm serve 可并存：SQLite WAL + busy_timeout 保证短命令安全排队。

use crate::config;
use crate::deps;
use crate::engine::{self, WorkspaceHandle};
use crate::fork;
use crate::history;
use crate::prune;
use crate::storage::Storage;
use crate::sync;
use anyhow::{bail, Result};
use sqlx::Row;
use std::path::Path;
use std::sync::Arc;

fn hash12(h: &str) -> String {
    h.chars().take(12).collect()
}

async fn load(name: &str) -> Result<Arc<WorkspaceHandle>> {
    let reg = config::load_registry()?;
    let Some(entry) = reg.workspaces.iter().find(|e| e.name == name) else {
        let names: Vec<String> = reg.workspaces.iter().map(|e| e.name.clone()).collect();
        bail!("工作区不存在: {name}（已注册: {names:?}）");
    };
    Ok(Arc::new(engine::load_handle(&entry.name, Path::new(&entry.path)).await?))
}

fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

pub async fn workspaces(json: bool) -> Result<()> {
    let reg = config::load_registry()?;
    let mut items = Vec::new();
    for e in &reg.workspaces {
        let mut o = serde_json::json!({"name": e.name, "path": e.path});
        match load(&e.name).await {
            Ok(h) => {
                let parts: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM parts WHERE deleted_at IS NULL")
                        .fetch_one(&h.ctx.db).await?;
                let files: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM files WHERE deleted_at IS NULL")
                        .fetch_one(&h.ctx.db).await?;
                let conflicts: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM conflicts WHERE status='open'")
                        .fetch_one(&h.ctx.db).await?;
                o["backend"] = h.ctx.store.backend.clone().into();
                o["parts"] = parts.into();
                o["files"] = files.into();
                o["conflicts"] = conflicts.into();
            }
            Err(err) => {
                o["error"] = format!("{err:#}").into();
            }
        }
        items.push(o);
    }
    if json {
        return print_json(&items);
    }
    if items.is_empty() {
        println!("（无已注册工作区，先 pdm init）");
        return Ok(());
    }
    println!("{:<16} {:<7} {:>5} {:>5} {:>5}  path", "name", "backend", "parts", "files", "conf");
    for o in &items {
        println!(
            "{:<16} {:<7} {:>5} {:>5} {:>5}  {}",
            o["name"].as_str().unwrap_or(""),
            o["backend"].as_str().unwrap_or("-"),
            o["parts"].as_i64().unwrap_or(-1),
            o["files"].as_i64().unwrap_or(-1),
            o["conflicts"].as_i64().unwrap_or(-1),
            o["path"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

pub async fn status(name: &str, json: bool) -> Result<()> {
    let h = load(name).await?;
    let parts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM parts WHERE deleted_at IS NULL")
        .fetch_one(&h.ctx.db).await?;
    let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files WHERE deleted_at IS NULL")
        .fetch_one(&h.ctx.db).await?;
    let revs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM revisions")
        .fetch_one(&h.ctx.db).await?;
    let objects: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM objects WHERE upload_state='done'")
        .fetch_one(&h.ctx.db).await?;
    let bytes: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(size),0) FROM objects")
        .fetch_one(&h.ctx.db).await?;
    let conflicts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conflicts WHERE status='open'")
        .fetch_one(&h.ctx.db).await?;
    let v = serde_json::json!({
        "name": h.name, "backend": h.ctx.store.backend,
        "device_id": h.ctx.device_id,
        "parts": parts, "files": files, "revisions": revs,
        "objects": objects, "bytes": bytes, "open_conflicts": conflicts,
    });
    if json {
        return print_json(&v);
    }
    println!(
        "{}: 后端 {} / device {}\n  零件 {} · 文件 {} · 修订 {} · 对象 {}（{:.1} MB）· 未解决冲突 {}",
        h.name, h.ctx.store.backend, &h.ctx.device_id[..8],
        parts, files, revs, objects, bytes as f64 / 1048576.0, conflicts,
    );
    Ok(())
}

pub async fn files(name: &str, json: bool) -> Result<()> {
    let h = load(name).await?;
    let rows = sqlx::query(
        "SELECT f.rel_path, p.name AS part, f.local_state, f.content_hash, f.updated_at, \
                v.seq AS cur_seq, \
                (SELECT MAX(v2.seq) FROM revisions v2 WHERE v2.part_id=f.part_id) AS max_seq, \
                EXISTS(SELECT 1 FROM pending_updates pu WHERE pu.rel_path=f.rel_path) AS pending \
         FROM files f JOIN parts p ON p.id=f.part_id \
         LEFT JOIN revisions v ON v.id=f.current_revision_id \
         WHERE f.deleted_at IS NULL ORDER BY f.rel_path",
    )
    .fetch_all(&h.ctx.db)
    .await?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let cur: Option<i64> = r.get("cur_seq");
            let max: Option<i64> = r.get("max_seq");
            let hash: String = r.get("content_hash");
            serde_json::json!({
                "rel_path": r.get::<String,_>("rel_path"),
                "part": r.get::<String,_>("part"),
                "state": r.get::<String,_>("local_state"),
                "hash": hash12(&hash),
                "seq": cur,
                "remote_ahead": max.is_some() && cur.is_some() && max > cur,
                "pending_update": r.get::<i64,_>("pending") > 0,
                "updated_at": r.get::<Option<String>,_>("updated_at"),
            })
        })
        .collect();
    if json {
        return print_json(&items);
    }
    if items.is_empty() {
        println!("（空）");
        return Ok(());
    }
    println!("{:<44} {:<14} {:<10} {:>4}  hash", "path", "part", "state", "v");
    for f in &items {
        let mut flags = String::new();
        if f["remote_ahead"].as_bool().unwrap_or(false) {
            flags.push_str(" ⚠远端更新");
        }
        if f["pending_update"].as_bool().unwrap_or(false) {
            flags.push_str(" 待更新");
        }
        println!(
            "{:<44} {:<14} {:<10} {:>4}  {}{}",
            f["rel_path"].as_str().unwrap_or(""),
            f["part"].as_str().unwrap_or(""),
            f["state"].as_str().unwrap_or(""),
            f["seq"].as_i64().unwrap_or(-1),
            f["hash"].as_str().unwrap_or(""),
            flags,
        );
    }
    Ok(())
}

pub async fn history(name: &str, part: &str, json: bool) -> Result<()> {
    let h = load(name).await?;
    let revs = history::list_history(&h.ctx, part).await?;
    if json {
        return print_json(&revs);
    }
    if revs.is_empty() {
        println!("（无修订）");
        return Ok(());
    }
    for r in &revs {
        if r.status == "pruned" {
            println!("v{:<4} 已按策略清理（对象已裁剪，可从 native 重新导出）", r.seq);
            continue;
        }
        println!(
            "v{:<4} {:<9} {:<20} {:<10} {}",
            r.seq, r.status, r.message, r.author, r.created_at,
        );
        for rep in &r.reps {
            println!("      {} {} ({})", rep.kind, rep.rel_path, hash12(&rep.object_hash));
        }
    }
    Ok(())
}

pub async fn pull(name: &str) -> Result<()> {
    let h = load(name).await?;
    let r = sync::pull(&h).await?;
    println!(
        "拉取完成：+{} 修订，应用 {}，推迟 {}，新冲突 {}，墓碑 {}，解决事件 {}，意向 {}，gc {}",
        r.new_revisions, r.applied_updates, r.deferred, r.conflicts,
        r.tombstones, r.resolutions, r.reservations, r.gc,
    );
    Ok(())
}

pub async fn rescan(name: &str) -> Result<()> {
    let h = load(name).await?;
    let n = engine::rescan(&h).await?;
    let edges = deps::rebuild(&h.ctx).await?;
    println!("重扫完成：创建 {} 个修订，依赖图 {} 条边", n, edges);
    Ok(())
}

pub async fn rollback(name: &str, part: &str, seq: i64) -> Result<()> {
    let h = load(name).await?;
    let n = history::rollback(&h.ctx, part, seq).await?;
    println!("回滚完成：{} v{seq}，落盘 {} 个文件（此后新修订基于 v{seq}）", part, n);
    Ok(())
}

pub async fn conflicts(name: &str, json: bool) -> Result<()> {
    let h = load(name).await?;
    let rows = sqlx::query(
        "SELECT c.id, p.name AS part, lv.seq AS local_seq, rv.seq AS remote_seq, c.created_at \
         FROM conflicts c JOIN parts p ON p.id=c.part_id \
         LEFT JOIN revisions lv ON lv.id=c.local_revision_id \
         LEFT JOIN revisions rv ON rv.id=c.remote_revision_id \
         WHERE c.status='open' ORDER BY c.created_at",
    )
    .fetch_all(&h.ctx.db)
    .await?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<String,_>("id"),
                "part": r.get::<String,_>("part"),
                "local_seq": r.get::<Option<i64>,_>("local_seq"),
                "remote_seq": r.get::<Option<i64>,_>("remote_seq"),
                "created_at": r.get::<String,_>("created_at"),
            })
        })
        .collect();
    if json {
        return print_json(&items);
    }
    if items.is_empty() {
        println!("（无未解决冲突）");
        return Ok(());
    }
    for c in &items {
        println!(
            "{}  {}  本地 v{} vs 远端 v{}  {}",
            c["id"].as_str().unwrap_or(""),
            c["part"].as_str().unwrap_or(""),
            c["local_seq"].as_i64().unwrap_or(-1),
            c["remote_seq"].as_i64().unwrap_or(-1),
            c["created_at"].as_str().unwrap_or(""),
        );
    }
    println!("解决：pdm resolve {} --id <id> --mine|--theirs|--fork [--fork-name N]", name);
    Ok(())
}

pub async fn resolve(
    name: &str,
    id: &str,
    mine: bool,
    theirs: bool,
    fork: bool,
    fork_name: Option<String>,
) -> Result<()> {
    let choice = match (mine, theirs, fork) {
        (true, false, false) => "mine",
        (false, true, false) => "theirs",
        (false, false, true) => "fork",
        _ => bail!("--mine / --theirs / --fork 恰好选一个"),
    };
    let h = load(name).await?;
    let r = sync::resolve(&h, id, choice, fork_name).await?;
    println!(
        "已解决（{}）：part {}，胜者 {}（解决事件已广播）",
        choice, r["part"].as_str().unwrap_or(""), r["winner"].as_str().unwrap_or(""),
    );
    Ok(())
}

pub async fn fork(name: &str, part: &str, new_name: &str, seq: Option<i64>) -> Result<()> {
    let h = load(name).await?;
    let rev_id = match seq {
        Some(s) => sqlx::query(
            "SELECT v.id FROM revisions v JOIN parts p ON p.id=v.part_id WHERE p.name=? AND v.seq=?",
        )
        .bind(part)
        .bind(s)
        .fetch_optional(&h.ctx.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("修订不存在: {part} v{s}"))?
        .get::<String, _>("id"),
        None => crate::db::part_head(&h.ctx.db, &crate::db::part_id_by_name(&h.ctx.db, part)
            .await?
            .ok_or_else(|| anyhow::anyhow!("零件不存在: {part}"))?)
        .await?
        .ok_or_else(|| anyhow::anyhow!("零件无修订: {part}"))?
        .0,
    };
    let info = fork::create_fork(&h.ctx, &rev_id, new_name).await?;
    println!("fork 完成：{} → {}（零拷贝，{} 个文件）", part, info.new_part, info.files.len());
    for f in &info.files {
        println!("  {}", f);
    }
    Ok(())
}

pub async fn fork_tree(name: &str, part: &str, suffix: Option<String>) -> Result<()> {
    let h = load(name).await?;
    let infos = fork::create_fork_tree(&h.ctx, part, suffix.as_deref().unwrap_or("方案B")).await?;
    println!("整装配 fork 完成：{} 个零件", infos.len());
    for i in &infos {
        println!("  {}（{} 个文件）", i.new_part, i.files.len());
    }
    Ok(())
}

pub async fn prune_cmd(name: &str, execute: bool) -> Result<()> {
    let h = load(name).await?;
    let r = prune::prune(&h.ctx, !execute, h.cfg.prune_n, h.cfg.prune_t_days).await?;
    if !execute {
        println!("[dry-run 预览] 加 --execute 执行。");
    }
    println!(
        "可裁 {} 个修订 / {:.1} MB（保护规则：N={}, T={}天，当前未命中保留引用 {} 个 hash）",
        r.items.len(), r.total_bytes as f64 / 1048576.0, h.cfg.prune_n, h.cfg.prune_t_days, r.kept_by_reference,
    );
    for i in &r.items {
        println!("  {:<20} v{:<5} {:>10} bytes  {}", i.part, i.seq, i.bytes, i.created_at);
    }
    if execute {
        println!("已执行：删除 {} 个对象", r.deleted_objects);
    }
    Ok(())
}

pub async fn deps_list(name: &str, json: bool) -> Result<()> {
    let h = load(name).await?;
    let edges = deps::list_edges(&h.ctx).await?;
    if json {
        let items: Vec<serde_json::Value> = edges
            .iter()
            .map(|(p, d, s)| serde_json::json!({"part": p, "depends_on": d, "source": s}))
            .collect();
        return print_json(&items);
    }
    if edges.is_empty() {
        println!("（无边；pdm rescan {} 自动重建）", name);
        return Ok(());
    }
    for (p, d, s) in &edges {
        println!("{:<20} → {:<20} {}", p, d, s);
    }
    Ok(())
}

pub async fn impact(name: &str, part: &str, json: bool) -> Result<()> {
    let h = load(name).await?;
    let affected = deps::impact(&h.ctx, part).await?;
    if json {
        return print_json(&serde_json::json!({"part": part, "affected": affected}));
    }
    if affected.is_empty() {
        println!("（无装配引用 {}）", part);
    } else {
        println!("修改 {} 将影响 {} 个装配：", part, affected.len());
        for a in &affected {
            println!("  {}", a);
        }
    }
    Ok(())
}

pub async fn remove(name: &str) -> Result<()> {
    let removed = config::registry_remove(name)?;
    println!(
        "{}",
        if removed { format!("已从注册表移除 {name}（不删除任何数据）") } else { format!("注册表中无 {name}") },
    );
    Ok(())
}

pub async fn test_oss(
    endpoint: &str,
    bucket: &str,
    ak: &str,
    sk: &str,
    prefix: Option<String>,
) -> Result<()> {
    let cfg = config::OssConfig {
        endpoint: endpoint.into(),
        bucket: bucket.into(),
        access_key_id: ak.into(),
        access_key_secret: sk.into(),
        prefix: prefix.unwrap_or_default(),
        backend: "oss".into(),
        store_root: String::new(),
    };
    let store = Storage::new(&cfg, &std::env::temp_dir())?;
    match store.probe().await {
        Ok(ms) => println!("连接 OK（{}），往返 {} ms（探针对象已删除）", store.backend, ms),
        Err(e) => bail!("连接失败: {e:#}"),
    }
    Ok(())
}
