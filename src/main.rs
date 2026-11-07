//! PDM Lite —— 轻量 CAD 数据管理（设计方案见 docs/design.md v0.8）
//! M0+M1+M2 已完成。CLI 覆盖全部操作；serve 提供本地 Web UI。

mod api;
mod cli;
mod config;
mod db;
mod deps;
mod engine;
mod fork;
mod history;
mod init;
mod prune;
mod revs;
mod scan;
mod storage;
mod sync;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pdm", version, about = "PDM Lite — 轻量 CAD 数据管理")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动常驻服务（watcher + 同步引擎 + 本地 Web UI）
    Serve {
        #[arg(long, default_value_t = 8420)]
        port: u16,
    },
    /// 初始化工作区（第二台设备会自动接管远端历史）
    Init {
        path: PathBuf,
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        bucket: String,
        #[arg(long, default_value = "")]
        ak: String,
        #[arg(long, default_value = "")]
        sk: String,
        #[arg(long, default_value = "")]
        prefix: String,
        /// oss | local
        #[arg(long, default_value = "oss")]
        backend: String,
        /// local 后端的共享存储根（双工作区/双机同步测试）
        #[arg(long, default_value = "")]
        store_root: String,
        #[arg(long, default_value = "local")]
        user: String,
    },
    /// 工作区列表
    Workspaces {
        #[arg(long)]
        json: bool,
    },
    /// 工作区状态统计
    Status {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// 文件列表
    Files {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// 零件修订历史
    History {
        name: String,
        #[arg(long)]
        part: String,
        #[arg(long)]
        json: bool,
    },
    /// 拉取远端事件并应用（pull）
    Pull { name: String },
    /// 手动重扫并重建依赖图
    Rescan { name: String },
    /// 回滚到指定修订
    Rollback {
        name: String,
        #[arg(long)]
        part: String,
        #[arg(long)]
        seq: i64,
    },
    /// 未解决冲突队列
    Conflicts {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// 解决冲突（恰好一个选择）
    Resolve {
        name: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        mine: bool,
        #[arg(long)]
        theirs: bool,
        #[arg(long)]
        fork: bool,
        /// --fork 时的新零件名
        #[arg(long)]
        fork_name: Option<String>,
    },
    /// 单零件 fork（默认当前 head，--seq 指定历史点）
    Fork {
        name: String,
        #[arg(long)]
        part: String,
        #[arg(long)]
        new_name: String,
        #[arg(long)]
        seq: Option<i64>,
    },
    /// 整装配 fork（按依赖图连带全部依赖零件）
    ForkTree {
        name: String,
        #[arg(long)]
        part: String,
        #[arg(long)]
        suffix: Option<String>,
    },
    /// 历史裁剪（默认 dry-run，--execute 执行）
    Prune {
        name: String,
        #[arg(long)]
        execute: bool,
    },
    /// 装配依赖图边列表
    Deps {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// 受影响装配查询（传递闭包）
    Impact {
        name: String,
        #[arg(long)]
        part: String,
        #[arg(long)]
        json: bool,
    },
    /// 从注册表移除工作区（不删除任何数据）
    Remove { name: String },
    /// 测试 OSS 连接（探针对象写/读/删）
    TestOss {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        bucket: String,
        #[arg(long)]
        ak: String,
        #[arg(long)]
        sk: String,
        #[arg(long)]
        prefix: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { port } => api::serve(port).await,
        Cmd::Init { path, endpoint, bucket, ak, sk, prefix, backend, user, store_root } => {
            tracing_subscriber::fmt::init();
            let summary = init::init_workspace(init::InitOptions {
                path,
                name: None,
                oss: config::OssConfig {
                    endpoint,
                    bucket,
                    access_key_id: ak,
                    access_key_secret: sk,
                    prefix,
                    backend,
                    store_root,
                },
                user,
                takeover: false,
            })
            .await?;
            println!(
                "init 完成: 「{name}」 {path}\n  零件 {parts} / 文件 {files} / 对象 {objects} / {bytes} bytes / 后端 {backend} / 接管远端历史 {adopted}",
                name = summary.name, path = summary.path, parts = summary.parts,
                files = summary.files, objects = summary.objects, bytes = summary.bytes,
                backend = summary.backend, adopted = summary.adopted_from_remote,
            );
            Ok(())
        }
        Cmd::Workspaces { json } => cli::workspaces(json).await,
        Cmd::Status { name, json } => cli::status(&name, json).await,
        Cmd::Files { name, json } => cli::files(&name, json).await,
        Cmd::History { name, part, json } => cli::history(&name, &part, json).await,
        Cmd::Pull { name } => cli::pull(&name).await,
        Cmd::Rescan { name } => cli::rescan(&name).await,
        Cmd::Rollback { name, part, seq } => cli::rollback(&name, &part, seq).await,
        Cmd::Conflicts { name, json } => cli::conflicts(&name, json).await,
        Cmd::Resolve { name, id, mine, theirs, fork, fork_name } => {
            cli::resolve(&name, &id, mine, theirs, fork, fork_name).await
        }
        Cmd::Fork { name, part, new_name, seq } => cli::fork(&name, &part, &new_name, seq).await,
        Cmd::ForkTree { name, part, suffix } => cli::fork_tree(&name, &part, suffix).await,
        Cmd::Prune { name, execute } => cli::prune_cmd(&name, execute).await,
        Cmd::Deps { name, json } => cli::deps_list(&name, json).await,
        Cmd::Impact { name, part, json } => cli::impact(&name, &part, json).await,
        Cmd::Remove { name } => cli::remove(&name).await,
        Cmd::TestOss { endpoint, bucket, ak, sk, prefix } => {
            cli::test_oss(&endpoint, &bucket, &ak, &sk, prefix).await
        }
    }
}
