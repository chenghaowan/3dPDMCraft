//! axum 本地 Web UI（127.0.0.1，仅本机）：工作区列表 / 配置与 init 引导 / 状态与历史（设计文档 §8）。

use crate::config::{self, OssConfig};
use crate::engine::{self, WorkspaceHandle};
use crate::history;
use crate::init;
use crate::scan;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use sqlx::Row;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub struct App {
    pub device_id: String,
    pub workspaces: RwLock<HashMap<String, Arc<WorkspaceHandle>>>,
}

const INDEX_HTML: &str = include_str!("ui/index.html");
const WS_HTML: &str = include_str!("ui/ws.html");

fn html(s: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], s)
}

fn api_err(e: anyhow::Error) -> (StatusCode, String) {
    tracing::warn!("api error: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

pub async fn serve(port: u16) -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let device_id = config::ensure_device_id()?;
    let app = Arc::new(App {
        device_id,
        workspaces: RwLock::new(HashMap::new()),
    });

    // 加载注册表里已初始化的工作区 + 启动 watcher + 拉取循环
    for entry in config::load_registry()?.workspaces {
        let root = PathBuf::from(&entry.path);
        match engine::load_handle(&entry.name, &root).await {
            Ok(h) => {
                let h = Arc::new(h);
                engine::spawn_watcher(h.clone());
                spawn_pull_loop(h.clone());
                spawn_auto_prune(h.clone());
                app.workspaces
                    .write()
                    .unwrap()
                    .insert(entry.name.clone(), h);
                tracing::info!("工作区已加载: {}", entry.name);
            }
            Err(e) => tracing::warn!("加载工作区 {} 失败: {e:#}", entry.name),
        }
    }

    let router = Router::new()
        .route("/", get(|| async { html(INDEX_HTML) }))
        .route(
            "/ws/{name}",
            get(|AxPath(_name): AxPath<String>| async move { html(WS_HTML) }),
        )
        .route("/api/workspaces", get(list_workspaces))
        .route("/api/oss/test", post(test_oss))
        .route("/api/workspaces/init", post(init_ws))
        .route("/api/workspaces/{name}/remove", post(remove_ws))
        .route("/api/ws/{name}/status", get(ws_status))
        .route("/api/ws/{name}/files", get(ws_files))
        .route("/api/ws/{name}/history", get(ws_history))
        .route("/api/ws/{name}/rescan", post(ws_rescan))
        .route("/api/ws/{name}/rollback", post(ws_rollback))
        .route("/api/ws/{name}/pull", post(ws_pull))
        .route("/api/ws/{name}/conflicts", get(ws_conflicts))
        .route("/api/ws/{name}/conflicts/resolve", post(ws_resolve))
        .route("/api/ws/{name}/fork", post(ws_fork))
        .route("/api/ws/{name}/fork-tree", post(ws_fork_tree))
        .route("/api/ws/{name}/prune", post(ws_prune))
        .route("/api/ws/{name}/reservations", get(ws_reservations))
        .route("/api/ws/{name}/deps", get(ws_deps))
        .route("/api/ws/{name}/deps/rescan", post(ws_deps_rescan))
        .route("/api/ws/{name}/deps/impact", get(ws_deps_impact))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("PDM Lite UI: http://127.0.0.1:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}

// ---------- 请求体 ----------

#[derive(Deserialize)]
struct OssReq {
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    bucket: String,
    #[serde(default)]
    access_key_id: String,
    #[serde(default)]
    access_key_secret: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    backend: String,
    #[serde(default)]
    store_root: String,
}

impl OssReq {
    fn into_config(self) -> OssConfig {
        OssConfig {
            endpoint: self.endpoint,
            bucket: self.bucket,
            access_key_id: self.access_key_id,
            access_key_secret: self.access_key_secret,
            prefix: self.prefix,
            backend: if self.backend.is_empty() { "oss".into() } else { self.backend },
            store_root: self.store_root,
        }
    }
}

#[derive(Deserialize)]
struct InitReq {
    #[serde(default)]
    name: Option<String>,
    path: String,
    #[serde(default)]
    user: Option<String>,
    oss: OssReq,
}

#[derive(Deserialize)]
struct RollbackReq {
    part: String,
    seq: i64,
}

// ---------- handlers ----------

async fn list_workspaces(
    State(app): State<Arc<App>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let reg = config::load_registry().map_err(api_err)?;
    let mut out = Vec::new();
    for entry in reg.workspaces {
        let handle = app.workspaces.read().unwrap().get(&entry.name).cloned();
        let mut item = serde_json::json!({
            "name": entry.name, "path": entry.path,
            "status": if handle.is_some() { "running" } else { "加载失败" },
        });
        if let Some(h) = handle {
            let backend = h.ctx.store.backend.clone();
            let parts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM parts WHERE deleted_at IS NULL")
                .fetch_one(&h.ctx.db).await.unwrap_or(0);
            let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files").fetch_one(&h.ctx.db).await.unwrap_or(0);
            let conflicts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files WHERE local_state='conflicted'")
                .fetch_one(&h.ctx.db).await.unwrap_or(0);
            let last: Option<String> = sqlx::query_scalar(
                "SELECT MAX(created_at) FROM revisions").fetch_one(&h.ctx.db).await.unwrap_or(None);
            item["backend"] = backend.into();
            item["parts"] = parts.into();
            item["files"] = files.into();
            item["conflicts"] = conflicts.into();
            item["last_revision"] = last.into();
        }
        out.push(item);
    }
    Ok(Json(serde_json::Value::Array(out)))
}

async fn test_oss(
    Json(req): Json<OssReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cfg = req.into_config();
    let root = std::env::temp_dir().join("pdm-probe");
    let store = crate::storage::Storage::new(&cfg, &root).map_err(api_err)?;
    match store.probe().await {
        Ok(ms) => Ok(Json(serde_json::json!({"ok": true, "latency_ms": ms, "backend": store.backend}))),
        Err(e) => Ok(Json(serde_json::json!({"ok": false, "error": format!("{e:#}"), "backend": store.backend}))),
    }
}

async fn init_ws(
    State(app): State<Arc<App>>,
    Json(req): Json<InitReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cfg = req.oss.into_config();
    if cfg.backend == "oss"
        && (cfg.endpoint.is_empty() || cfg.bucket.is_empty() || cfg.access_key_id.is_empty())
    {
        return Err((StatusCode::BAD_REQUEST, "OSS 后端需要 endpoint/bucket/AK/SK".into()));
    }
    let summary = init::init_workspace(init::InitOptions {
        path: PathBuf::from(&req.path),
        name: req.name,
        oss: cfg,
        user: req.user.unwrap_or_else(|| "local".into()),
        takeover: false,
    })
    .await
    .map_err(api_err)?;

    // 挂载 + 启动 watcher + 拉取循环
    let root = PathBuf::from(&summary.path);
    if let Ok(h) = engine::load_handle(&summary.name, &root).await {
        let h = Arc::new(h);
        engine::spawn_watcher(h.clone());
        spawn_pull_loop(h.clone());
        spawn_auto_prune(h.clone());
        app.workspaces
            .write()
            .unwrap()
            .insert(summary.name.clone(), h);
    }
    Ok(Json(serde_json::to_value(&summary).unwrap()))
}

/// 自动定期裁剪（M2 §7.4）：config.toml 中 auto_prune=true 时按 auto_prune_hours 周期执行
fn spawn_auto_prune(h: Arc<WorkspaceHandle>) {
    if !h.cfg.auto_prune || h.cfg.auto_prune_hours <= 0 {
        return;
    }
    let period = std::time::Duration::from_secs(h.cfg.auto_prune_hours as u64 * 3600);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(period).await;
            match crate::prune::prune(&h.ctx, false, h.cfg.prune_n, h.cfg.prune_t_days).await {
                Ok(r) => tracing::info!(
                    "[{}] 自动裁剪：{} 修订 / {} 对象 / {} bytes",
                    h.name, r.items.len(), r.deleted_objects, r.total_bytes
                ),
                Err(e) => tracing::warn!("[{}] 自动裁剪失败: {e:#}", h.name),
            }
        }
    });
}

/// 周期拉取（M1：60s 定时 + 手动按钮；两设备对账经由共享的 OSS 事件前缀）
fn spawn_pull_loop(h: Arc<WorkspaceHandle>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match crate::sync::pull(&h).await {
                Ok(r) if r.new_revisions + r.tombstones + r.resolutions + r.gc > 0 => {
                    tracing::info!("[{}] pull: +{}修订 {}应用 {}推迟 {}冲突 {}墓碑 {}解决 {}gc",
                        h.name, r.new_revisions, r.applied_updates, r.deferred, r.conflicts,
                        r.tombstones, r.resolutions, r.gc);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("[{}] pull 失败: {e:#}", h.name),
            }
        }
    });
}

async fn remove_ws(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let removed = config::registry_remove(&name).map_err(api_err)?;
    app.workspaces.write().unwrap().remove(&name);
    Ok(Json(serde_json::json!({"removed": removed})))
}

fn handle_of(
    app: &App,
    name: &str,
) -> Result<Arc<WorkspaceHandle>, (StatusCode, String)> {
    app.workspaces
        .read()
        .unwrap()
        .get(name)
        .cloned()
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("工作区不存在: {name}")))
}

async fn ws_status(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let parts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM parts WHERE deleted_at IS NULL")
        .fetch_one(&h.ctx.db).await.map_err(|e| api_err(e.into()))?;
    let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files").fetch_one(&h.ctx.db).await.map_err(|e| api_err(e.into()))?;
    let revs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM revisions").fetch_one(&h.ctx.db).await.map_err(|e| api_err(e.into()))?;
    let objects: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM objects WHERE upload_state='done'").fetch_one(&h.ctx.db).await.map_err(|e| api_err(e.into()))?;
    let bytes: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(size),0) FROM objects").fetch_one(&h.ctx.db).await.map_err(|e| api_err(e.into()))?;
    Ok(Json(serde_json::json!({
        "name": h.name, "backend": h.ctx.store.backend, "device_id": h.ctx.device_id,
        "parts": parts, "files": files, "revisions": revs, "objects": objects, "bytes": bytes,
    })))
}

async fn ws_files(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let rows = sqlx::query(
        "SELECT f.rel_path, p.name AS part, f.local_state, f.content_hash, f.updated_at, \
                v.seq AS cur_seq, \
                (SELECT MAX(v2.seq) FROM revisions v2 WHERE v2.part_id = f.part_id) AS max_seq, \
                EXISTS(SELECT 1 FROM pending_updates pu WHERE pu.rel_path = f.rel_path) AS pending \
         FROM files f JOIN parts p ON p.id = f.part_id \
         LEFT JOIN revisions v ON v.id = f.current_revision_id \
         WHERE f.deleted_at IS NULL ORDER BY f.rel_path",
    )
    .fetch_all(&h.ctx.db)
    .await
    .map_err(|e| api_err(e.into()))?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let cur: Option<i64> = r.get("cur_seq");
            let max: Option<i64> = r.get("max_seq");
            let hash: String = r.get("content_hash");
            let pending: i64 = r.get("pending");
            serde_json::json!({
                "rel_path": r.get::<String,_>("rel_path"),
                "part": r.get::<String,_>("part"),
                "state": r.get::<String,_>("local_state"),
                "hash": &hash[..12.min(hash.len())],
                "seq": cur,
                "remote_ahead": max.is_some() && cur.is_some() && max > cur,
                "pending_update": pending > 0,
                "updated_at": r.get::<Option<String>,_>("updated_at"),
            })
        })
        .collect();
    Ok(Json(serde_json::Value::Array(items)))
}

async fn ws_history(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let part = q.get("part").ok_or((StatusCode::BAD_REQUEST, "缺少 part 参数".into()))?;
    let revs = history::list_history(&h.ctx, part).await.map_err(api_err)?;
    Ok(Json(serde_json::to_value(&revs).unwrap()))
}

async fn ws_rescan(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let n = engine::rescan(&h).await.map_err(api_err)?;
    let edges = crate::deps::rebuild(&h.ctx).await.map_err(api_err)?;
    Ok(Json(serde_json::json!({"revisions_created": n, "dep_edges": edges})))
}

async fn ws_rollback(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
    Json(req): Json<RollbackReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let n = history::rollback(&h.ctx, &req.part, req.seq)
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::json!({"replaced_files": n, "part": req.part, "seq": req.seq})))
}

// ---------- M1：pull / 冲突 / fork / 裁剪 / 意向 ----------

async fn ws_pull(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let r = crate::sync::pull(&h).await.map_err(api_err)?;
    Ok(Json(serde_json::to_value(&r).unwrap()))
}

async fn ws_conflicts(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let rows = sqlx::query(
        "SELECT c.id, p.name AS part, c.local_revision_id, c.remote_revision_id, \
                lv.seq AS local_seq, rv.seq AS remote_seq, c.created_at \
         FROM conflicts c JOIN parts p ON p.id=c.part_id \
         LEFT JOIN revisions lv ON lv.id=c.local_revision_id \
         LEFT JOIN revisions rv ON rv.id=c.remote_revision_id \
         WHERE c.status='open' ORDER BY c.created_at",
    )
    .fetch_all(&h.ctx.db)
    .await
    .map_err(|e| api_err(e.into()))?;
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
    Ok(Json(serde_json::Value::Array(items)))
}

#[derive(Deserialize)]
struct ResolveReq {
    id: String,
    choice: String,
    #[serde(default)]
    fork_name: Option<String>,
}

async fn ws_resolve(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
    Json(req): Json<ResolveReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let r = crate::sync::resolve(&h, &req.id, &req.choice, req.fork_name)
        .await
        .map_err(api_err)?;
    Ok(Json(r))
}

#[derive(Deserialize)]
struct ForkReq {
    part: String,
    seq: i64,
    #[serde(default)]
    new_name: Option<String>,
}

async fn ws_fork(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
    Json(req): Json<ForkReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let rev_id: String = sqlx::query(
        "SELECT v.id FROM revisions v JOIN parts p ON p.id=v.part_id WHERE p.name=? AND v.seq=?",
    )
    .bind(&req.part)
    .bind(req.seq)
    .fetch_optional(&h.ctx.db)
    .await
    .map_err(|e| api_err(e.into()))?
    .ok_or((StatusCode::NOT_FOUND, format!("修订不存在: {} v{}", req.part, req.seq)))?
    .get("id");
    let default_name = format!("{}.fork", req.part);
    let info = crate::fork::create_fork(&h.ctx, &rev_id, req.new_name.as_deref().unwrap_or(&default_name))
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::to_value(&info).unwrap()))
}

#[derive(Deserialize)]
struct PruneReq {
    #[serde(default = "true_fn")]
    dry_run: bool,
}
fn true_fn() -> bool {
    true
}

async fn ws_prune(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
    Json(req): Json<PruneReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let r = crate::prune::prune(&h.ctx, req.dry_run, h.cfg.prune_n, h.cfg.prune_t_days)
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::to_value(&r).unwrap()))
}

async fn ws_reservations(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let now = crate::db::utc_now();
    let rows = sqlx::query(
        "SELECT part_id, author, device_id, expires_at, created_at FROM reservations \
         WHERE expires_at > ? ORDER BY created_at",
    )
    .bind(&now)
    .fetch_all(&h.ctx.db)
    .await
    .map_err(|e| api_err(e.into()))?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "part": r.get::<String,_>("part_id"),
                "author": r.get::<String,_>("author"),
                "device_id": r.get::<String,_>("device_id"),
                "expires_at": r.get::<String,_>("expires_at"),
            })
        })
        .collect();
    Ok(Json(serde_json::Value::Array(items)))
}

// ---------- M2：依赖图 / 整装配 fork ----------

async fn ws_deps(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let edges = crate::deps::list_edges(&h.ctx).await.map_err(api_err)?;
    let items: Vec<serde_json::Value> = edges
        .iter()
        .map(|(p, dep, src)| serde_json::json!({"part": p, "depends_on": dep, "source": src}))
        .collect();
    Ok(Json(serde_json::Value::Array(items)))
}

async fn ws_deps_rescan(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let edges = crate::deps::rebuild(&h.ctx).await.map_err(api_err)?;
    Ok(Json(serde_json::json!({"dep_edges": edges})))
}

async fn ws_deps_impact(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let part = q.get("part").ok_or((StatusCode::BAD_REQUEST, "缺少 part 参数".into()))?;
    let affected = crate::deps::impact(&h.ctx, part).await.map_err(api_err)?;
    Ok(Json(serde_json::json!({"part": part, "affected": affected})))
}

#[derive(Deserialize)]
struct ForkTreeReq {
    part: String,
    #[serde(default)]
    suffix: Option<String>,
}

async fn ws_fork_tree(
    State(app): State<Arc<App>>,
    AxPath(name): AxPath<String>,
    Json(req): Json<ForkTreeReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = handle_of(&app, &name)?;
    let suffix = req.suffix.unwrap_or_else(|| "方案B".into());
    let infos = crate::fork::create_fork_tree(&h.ctx, &req.part, &suffix)
        .await
        .map_err(api_err)?;
    let items: Vec<serde_json::Value> = infos
        .iter()
        .map(|i| serde_json::json!({"new_part": i.new_part, "files": i.files}))
        .collect();
    Ok(Json(serde_json::json!({"forked": items.len(), "parts": items})))
}

// 保留 scan 引用以备 UI 扩展（避免 unused 警告）
#[allow(unused)]
fn _touch() {
    let _ = scan::part_key_for("");
}
