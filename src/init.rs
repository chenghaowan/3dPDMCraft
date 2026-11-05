//! 初始化流程（设计文档 §8.2 ③）：建目录 → 建库 → 拉取远端事件（clone 语义）→
//! 首扫对账（远端有历史则接管；无则建基线修订上传）→ 写注册表。

use crate::config::OssConfig;
use crate::db;
use crate::engine::EngineCtx;
use crate::revs::{self, RepInput};
use crate::scan;
use crate::storage::Storage;
use crate::sync;
use anyhow::{Context, Result};
use sqlx::Row;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct InitOptions {
    pub path: PathBuf,
    pub name: Option<String>,
    pub oss: OssConfig,
    pub user: String,
    pub takeover: bool,
}

#[derive(serde::Serialize)]
pub struct InitSummary {
    pub name: String,
    pub path: String,
    pub parts: usize,
    pub files: usize,
    pub objects: usize,
    pub bytes: u64,
    pub backend: String,
    pub adopted_from_remote: bool,
}

pub async fn init_workspace(opts: InitOptions) -> Result<InitSummary> {
    // 1) 校验目录
    if !opts.path.exists() {
        tokio::fs::create_dir_all(&opts.path).await?;
    }
    let root = opts
        .path
        .canonicalize()
        .with_context(|| format!("解析路径 {}", opts.path.display()))?;
    let registry = crate::config::load_registry()?;
    for e in &registry.workspaces {
        let other = PathBuf::from(&e.path);
        // 目录已不存在的注册项视为残留，不参与嵌套校验
        if other.exists() && (root.starts_with(&other) || other.starts_with(&root)) {
            anyhow::bail!("路径与已注册工作区「{}」嵌套", e.name);
        }
    }
    let pdm_dir = root.join(".pdm");
    if pdm_dir.exists() && !opts.takeover {
        anyhow::bail!("目录已初始化（存在 .pdm），接管需显式确认");
    }

    // 2) 建目录与配置
    tokio::fs::create_dir_all(root.join("native")).await?;
    tokio::fs::create_dir_all(root.join("neutral")).await?;
    tokio::fs::create_dir_all(&pdm_dir).await?;
    let ignore_path = pdm_dir.join("ignore.conf");
    if !ignore_path.exists() {
        let mut rules = scan::default_ignore_rules().join("\n");
        rules.push('\n');
        tokio::fs::write(&ignore_path, rules).await?;
    }
    let cfg = crate::config::WsConfig {
        oss: opts.oss.clone(),
        port: 8420,
        user: opts.user.clone(),
        prune_n: 10,
        prune_t_days: 90,
        auto_prune: false,
        auto_prune_hours: 24,
    };
    crate::config::save_ws_config(&root, &cfg)?;

    // 3) 建库 + 存储
    let pool = db::open_db(&root).await?;
    let store = Storage::new(&cfg.oss, &root)?;
    let device_id = crate::config::ensure_device_id()?;
    let ctx = EngineCtx {
        db: pool,
        store,
        root: root.clone(),
        author: cfg.user.clone(),
        device_id,
    };

    // 4) 拉取远端已有修订事件（clone 语义：第二台设备接管历史，不重建基线）
    let mut rev_events: Vec<sync::RevEvent> = Vec::new();
    for (key, v) in sync::fetch_events(&ctx.store, "meta/revisions/").await? {
        match serde_json::from_value::<sync::RevEvent>(v) {
            Ok(ev) => {
                rev_events.push(ev);
                sync::mark_applied(&ctx, &key).await?;
            }
            Err(e) => tracing::warn!("解析远端修订事件 {key}: {e}"),
        }
    }
    rev_events.sort_by_key(|e| e.revision.created_at.clone());
    for ev in &rev_events {
        sync::import_revision(&ctx, ev).await?;
    }
    let adopted = !rev_events.is_empty();

    // 5) 首扫 + 对账
    let rules = scan::load_ignore_rules(&root);
    let files = scan::scan_workspace(&root, &rules).await?;
    let groups: BTreeMap<String, Vec<scan::ScannedFile>> = scan::group_by_part(&files);
    let mut parts = 0usize;
    let mut nfiles = 0usize;
    let mut nobjects = 0usize;
    let mut bytes = 0u64;

    for (part_key, group) in &groups {
        let now = db::utc_now();
        let pid = revs::ensure_part(&ctx.db, part_key, &now).await?;
        let mut seen_hashes = std::collections::HashSet::new();
        for f in group {
            if seen_hashes.insert(f.hash.clone()) {
                nobjects += 1;
                bytes += f.size;
            }
        }
        nfiles += group.len();

        // 远端该 part 的 head（有 → 接管；无 → 建基线）
        let remote_head = rev_events
            .iter()
            .filter(|e| e.revision.part == *part_key)
            .max_by_key(|e| e.revision.seq)
            .cloned();

        match remote_head {
            Some(head_ev) => {
                sync::promote(&ctx, part_key, &head_ev.revision.id).await?;
                let head_seq = head_ev.revision.seq;
                let effective = sync::effective_reps_at(&ctx, &pid, head_seq).await?;
                for f in group {
                    adopt_file(&ctx, &pid, part_key, f, head_ev.revision.id.clone(), head_seq, &effective).await?;
                }
                tracing::info!("part {part_key} 接管远端历史（head v{head_seq}）");
            }
            None => {
                let reps: Vec<RepInput> = group
                    .iter()
                    .map(|f| RepInput {
                        kind: f.kind.clone(),
                        rel_path: f.rel_path.clone(),
                        object_hash: f.hash.clone(),
                        size: f.size,
                    })
                    .collect();
                revs::create_revision(&ctx, &pid, part_key, "init baseline", reps).await?;
            }
        }
        parts += 1;
    }

    // 6) 写注册表
    let name = crate::config::registry_add(&root, opts.name.as_deref())?;
    Ok(InitSummary {
        name,
        path: root.to_string_lossy().into_owned(),
        parts,
        files: nfiles,
        objects: nobjects,
        bytes,
        backend: ctx.store.backend.clone(),
        adopted_from_remote: adopted,
    })
}

/// init 时把一个本地扫描文件对账到远端历史：
/// hash == 远端 head → 指针对齐；匹配某远端旧修订 → 指到那（回滚式接管）；
/// 否则 → 本地修改，基于远端 head 提交修订。
async fn adopt_file(
    ctx: &EngineCtx,
    part_id: &str,
    part_key: &str,
    f: &scan::ScannedFile,
    head_rev_id: String,
    head_seq: i64,
    effective: &[(String, String, String, i64)],
) -> Result<()> {
    let now = db::utc_now();
    let head_hash = effective
        .iter()
        .find(|(rel, _, _, _)| *rel == f.rel_path)
        .map(|(_, _, h, _)| h.clone());

    let target_rev: Option<String> = match &head_hash {
        Some(h) if *h == f.hash => Some(head_rev_id.clone()),
        _ => {
            // 是否匹配该 part 某个远端修订的内容（如远端曾回滚过）
            let row = sqlx::query(
                "SELECT v.id FROM revisions v JOIN representations r ON r.revision_id=v.id \
                 WHERE v.part_id=? AND r.object_hash=? ORDER BY v.seq DESC LIMIT 1",
            )
            .bind(part_id)
            .bind(&f.hash)
            .fetch_optional(&ctx.db)
            .await?;
            row.map(|r| r.get::<String, _>("id"))
        }
    };

    match target_rev {
        Some(rid) => {
            let kind = f.kind.clone();
            sync::upsert_pointer(ctx, &f.rel_path, part_id, &kind, &rid, &f.hash, &now).await?;
        }
        None => {
            // 本地内容与远端 head 不同且非历史内容 → 本地修改（基于远端 head）
            let rep = RepInput {
                kind: f.kind.clone(),
                rel_path: f.rel_path.clone(),
                object_hash: f.hash.clone(),
                size: f.size,
            };
            revs::create_revision(ctx, part_id, part_key, "init adopt (local change)", vec![rep]).await?;
        }
    }
    let _ = head_seq;
    Ok(())
}
