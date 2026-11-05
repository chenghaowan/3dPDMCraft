//! 装配依赖图（M2）：解析 neutral STEP 文本中的 PRODUCT 实体名，
//! 建立装配 → 零件的"包含"关系（启发式：文件内出现的 PRODUCT 名匹配其他 part 的名字）。
//! 用途：受影响零件检查（改了 Bracket，哪些装配引用它）。CATProduct 为私有二进制格式，不解析。

use crate::engine::EngineCtx;
use anyhow::Result;
use sqlx::Row;

/// 从 STEP 文本提取 PRODUCT 实体名（标准导出为大写 PRODUCT('name'...）
pub fn extract_product_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("PRODUCT('") {
        let start = pos + "PRODUCT('".len();
        match rest[start..].find('\'') {
            Some(i) => {
                let name = rest[start..start + i].trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
                rest = &rest[start + i..];
            }
            None => break,
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 重建依赖表：扫描全部 neutral STEP 文件 → 名称匹配其他 part
/// 返回边数。文件读失败/超限跳过并记日志。
pub async fn rebuild(ctx: &EngineCtx) -> Result<usize> {
    sqlx::query("DELETE FROM dependencies").execute(&ctx.db).await?;
    let step_files = sqlx::query(
        "SELECT f.rel_path, f.part_id, p.name AS part_name FROM files f JOIN parts p ON p.id=f.part_id \
         WHERE f.deleted_at IS NULL AND (LOWER(f.rel_path) LIKE '%.step' OR LOWER(f.rel_path) LIKE '%.stp')",
    )
    .fetch_all(&ctx.db)
    .await?;
    let all_parts = sqlx::query("SELECT name FROM parts WHERE deleted_at IS NULL")
        .fetch_all(&ctx.db)
        .await?;
    let all_part_names: Vec<String> = all_parts.iter().map(|r| r.get::<String, _>("name")).collect();

    let mut edges = 0usize;
    for r in &step_files {
        let rel: String = r.get("rel_path");
        let part_id: String = r.get("part_id");
        let part_name: String = r.get("part_name");
        let path = ctx.root.join(&rel);
        // STEP 为 ASCII 文本；超大文件跳过（M2 启发式不做流式解析）
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > 200 * 1024 * 1024 {
            tracing::warn!("[deps] {rel} 超过 200MB，跳过依赖扫描");
            continue;
        }
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("[deps] 读取 {rel} 失败: {e}");
                continue;
            }
        };
        let product_names = extract_product_names(&text);
        let names: std::collections::HashSet<&str> =
            product_names.iter().map(|s| s.as_str()).collect();
        for pname in &all_part_names {
            if *pname == part_name {
                continue;
            }
            // 匹配完整相对名或末段名（末段匹配是启发式，同名零件会误连，文档已注明）
            let leaf = pname.rsplit('/').next().unwrap_or(pname);
            if names.contains(pname.as_str()) || names.contains(leaf) {
                sqlx::query(
                    "INSERT OR IGNORE INTO dependencies (part_id, depends_on, source_rel_path) VALUES (?,?,?)",
                )
                .bind(&part_id)
                .bind(pname)
                .bind(&rel)
                .execute(&ctx.db)
                .await?;
                edges += 1;
                tracing::info!("[deps] {part_name} → {pname} ({rel})");
            }
        }
    }
    tracing::info!("[deps] 依赖图重建完成：{edges} 条边");
    Ok(edges)
}

/// 影响分析：改了 part_name，哪些装配（直接/传递）受影响
pub async fn impact(ctx: &EngineCtx, part_name: &str) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "WITH RECURSIVE up(name) AS ( \
           SELECT ? \
           UNION \
           SELECT p.name FROM dependencies d JOIN parts p ON p.id = d.part_id \
             JOIN up u ON d.depends_on = u.name WHERE p.deleted_at IS NULL \
         ) SELECT name FROM up WHERE name != ? ORDER BY name",
    )
    .bind(part_name)
    .bind(part_name)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("name")).collect())
}

/// 全部边（UI 展示）
pub async fn list_edges(ctx: &EngineCtx) -> Result<Vec<(String, String, String)>> {
    let rows = sqlx::query(
        "SELECT p.name AS part, d.depends_on, d.source_rel_path FROM dependencies d \
         JOIN parts p ON p.id = d.part_id ORDER BY p.name, d.depends_on",
    )
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("part"), r.get("depends_on"), r.get("source_rel_path")))
        .collect())
}
