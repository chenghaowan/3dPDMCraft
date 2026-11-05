//! 单零件 fork（设计文档 §6.6）：以某修订为起点创建新零件，零拷贝（复用对象 hash），
//! 工作区落新文件名；不做 Git 式分支切换。

use crate::db::utc_now;
use crate::engine::EngineCtx;
use crate::revs::RepInput;
use crate::scan;
use crate::storage::Storage;
use crate::sync;
use anyhow::{bail, Result};
use sqlx::Row;

#[derive(serde::Serialize)]
pub struct ForkInfo {
    pub new_part: String,
    pub revision_id: String,
    pub files: Vec<String>,
}

pub async fn create_fork(ctx: &EngineCtx, src_rev_id: &str, new_name: &str) -> Result<ForkInfo> {
    let now = utc_now();
    let src = sqlx::query(
        "SELECT v.seq, v.part_id, p.name AS part_name FROM revisions v JOIN parts p ON p.id=v.part_id WHERE v.id=?",
    )
    .bind(src_rev_id)
    .fetch_optional(&ctx.db)
    .await?
    .ok_or_else(|| anyhow::anyhow!("源修订不存在: {src_rev_id}"))?;
    let src_seq: i64 = src.get("seq");
    let src_part_name: String = src.get("part_name");

    if crate::db::part_id_by_name(&ctx.db, new_name).await?.is_some() {
        bail!("零件已存在: {new_name}");
    }

    // 新零件（血缘记 forked_from_revision_id）
    let part_id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO parts (id,name,forked_from_revision_id,created_at) VALUES (?,?,?,?)")
        .bind(&part_id)
        .bind(new_name)
        .bind(src_rev_id)
        .bind(&now)
        .execute(&ctx.db)
        .await?;

    // 初始修订 seq=1（parent NULL），表示 = 源修订点的有效表示（零拷贝）
    let rev_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO revisions (id,part_id,seq,parent_id,author,device_id,message,status,pinned,created_at,origin) \
         VALUES (?,?,1,NULL,?,?,?,'current',0,?,'local')",
    )
    .bind(&rev_id)
    .bind(&part_id)
    .bind(&ctx.author)
    .bind(&ctx.device_id)
    .bind(format!("fork from {src_part_name} v{src_seq}"))
    .bind(&now)
    .execute(&ctx.db)
    .await?;

    let mut out_files = Vec::new();
    let mut ev_reps: Vec<RepInput> = Vec::new();
    let src_part_id: String = src.get("part_id");
    for (rel, kind, hash, _size) in sync::effective_reps_at(ctx, &src_part_id, src_seq).await? {
        // native/Housing.CATPart + part "Housing" → 新 rel "native/<new_name>.CATPart"
        let old_prefix = format!("{kind}/{src_part_name}");
        let new_rel = if rel.starts_with(&old_prefix) {
            format!("{kind}/{new_name}{}", &rel[old_prefix.len()..])
        } else {
            rel.clone()
        };
        materialize_file(ctx, &rel, &hash, &new_rel).await?;
        sqlx::query(
            "INSERT INTO representations (revision_id,kind,rel_path,object_hash,size) VALUES (?,?,?,?,?)",
        )
        .bind(&rev_id)
        .bind(&kind)
        .bind(&new_rel)
        .bind(&hash)
        .bind(_size)
        .execute(&ctx.db)
        .await?;
        sqlx::query(
            "INSERT INTO files (rel_path,part_id,kind,current_revision_id,base_revision_id,local_state,content_hash,updated_at) \
             VALUES (?,?,?,?,?,'synced',?,?)",
        )
        .bind(&new_rel)
        .bind(&part_id)
        .bind(&kind)
        .bind(&rev_id)
        .bind(&rev_id)
        .bind(&hash)
        .bind(&now)
        .execute(&ctx.db)
        .await?;
        out_files.push(new_rel.clone());
        ev_reps.push(RepInput { kind, rel_path: new_rel, object_hash: hash, size: _size as u64 });
    }

    // 广播 fork 的初始修订事件，其他设备才能看见新零件
    sync::emit_revision_event(ctx, &rev_id, new_name, 1, None, &format!("fork from {src_part_name} v{src_seq}"), &now, &ev_reps).await?;
    tracing::info!("fork: {src_part_name} v{src_seq} → 新零件 {new_name}（零拷贝，{} 个表示）", ev_reps.len());
    Ok(ForkInfo { new_part: new_name.into(), revision_id: rev_id, files: out_files })
}

/// 整装配 fork（M2）：按依赖图批量 fork 可达集合（装配 + 全部传递依赖零件）。
/// 限制（§6.6）：不改写装配文件内部引用，CAD 软件中需重挂引用名。
pub async fn create_fork_tree(
    ctx: &EngineCtx,
    root_part: &str,
    suffix: &str,
) -> Result<Vec<ForkInfo>> {
    // 可达集合：root + 传递依赖（dependencies.part_id 是 id，depends_on 是名）
    let rows = sqlx::query(
        "WITH RECURSIVE down(name) AS ( \
           SELECT ? \
           UNION \
           SELECT d.depends_on FROM dependencies d JOIN parts p ON p.id = d.part_id \
             JOIN down u ON p.name = u.name \
         ) SELECT name FROM down WHERE name != ?",
    )
    .bind(root_part)
    .bind("") // 占位，下面过滤空串
    .fetch_all(&ctx.db)
    .await?;
    let mut names: Vec<String> = rows
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .filter(|n| !n.is_empty())
        .collect();
    if !names.iter().any(|n| n == root_part) {
        names.push(root_part.to_string());
    }

    // 预检：全部新名字可用才动手（all-or-nothing）
    for n in &names {
        let new_name = format!("{n}.{suffix}");
        if crate::db::part_id_by_name(&ctx.db, &new_name).await?.is_some() {
            anyhow::bail!("零件已存在: {new_name}，中止整树 fork");
        }
    }
    names.sort(); // 依赖零件名通常短于装配名；顺序无关（零拷贝），稳定排序即可

    let mut out = Vec::new();
    for n in &names {
        // 取该 part 当前 head 修订
        let part_id = crate::db::part_id_by_name(&ctx.db, n)
            .await?
            .ok_or_else(|| anyhow::anyhow!("零件不存在: {n}"))?;
        let head = crate::db::part_head(&ctx.db, &part_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("零件无修订: {n}"))?;
        let info = create_fork(ctx, &head.0, &format!("{n}.{suffix}")).await?;
        out.push(info);
    }
    tracing::info!("整装配 fork: {root_part} + {} 个依赖 → 后缀 .{suffix}", out.len() - 1);
    Ok(out)
}

/// 新路径落盘：源路径文件 hash 一致则本地复制，否则从对象存储下载
async fn materialize_file(ctx: &EngineCtx, src_rel: &str, hash: &str, new_rel: &str) -> Result<()> {
    let dest = ctx.root.join(new_rel);
    if dest.exists() {
        bail!("目标文件已存在: {new_rel}");
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let src_path = ctx.root.join(src_rel);
    let src_hash = src_path.exists().then(|| scan::hash_file(&src_path).ok()).flatten().map(|(h, _)| h);
    if src_hash.as_deref() == Some(hash) {
        tokio::fs::copy(&src_path, &dest).await?;
    } else {
        let tmp_dir = ctx.root.join(".pdm").join("tmp");
        tokio::fs::create_dir_all(&tmp_dir).await?;
        let tmp = tmp_dir.join(format!("{}.part", uuid::Uuid::new_v4()));
        ctx.store.get_to_file(&Storage::object_key(hash), &tmp).await?;
        tokio::fs::rename(&tmp, &dest).await?;
    }
    Ok(())
}
