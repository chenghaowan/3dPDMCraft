//! PDM Lite —— 轻量 CAD 数据管理（设计方案见 docs/design.md v0.6）
//! M0 范围：配置页 + init、双文件夹 + part 归组、watcher+BLAKE3、
//! schema v2、OSS 上传、修订历史+回滚、回环抑制。

mod api;
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
        /// UI 端口
        #[arg(long, default_value_t = 8420)]
        port: u16,
    },
    /// 初始化一个工作区（CLI 方式；也可在 Web UI 里初始化）
    Init {
        /// 工作区目录
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
        /// local 后端的共享存储根（双工作区/双机同步测试）
        #[arg(long, default_value = "")]
        store_root: String,
        /// oss | local（local 用于离线测试）
        #[arg(long, default_value = "oss")]
        backend: String,
        #[arg(long, default_value = "local")]
        user: String,
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
                "init 完成: 工作区「{name}」 {path}\n  零件 {parts} / 文件 {files} / 对象 {objects} / {bytes} bytes / 后端 {backend}",
                name = summary.name,
                path = summary.path,
                parts = summary.parts,
                files = summary.files,
                objects = summary.objects,
                bytes = summary.bytes,
                backend = summary.backend,
            );
            Ok(())
        }
    }
}
