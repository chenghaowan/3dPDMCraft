//! 存储层：opendal Operator（oss / local 双后端）。
//! 对象 key 内容寻址：objects/<2>/<2>/<hash>，永不覆盖（§4.1/§4.2）。
//! local 后端把对象落在 <ws>/.pdm/store，供离线开发与冒烟测试（同 ltl-manage sync_dir 的回退思路）。

use crate::config::OssConfig;
use anyhow::{anyhow, Context, Result};
use opendal::services::{Fs, Oss};
use opendal::Operator;
use std::path::{Path, PathBuf};

pub struct Storage {
    pub op: Operator,
    pub backend: String,
}

impl Storage {
    pub fn new(cfg: &OssConfig, ws_root: &Path) -> Result<Self> {
        match cfg.backend.as_str() {
            "local" => {
                let root = if cfg.store_root.is_empty() {
                    ws_root.join(".pdm").join("store")
                } else {
                    PathBuf::from(&cfg.store_root)
                };
                std::fs::create_dir_all(&root)?;
                let b = Fs::default().root(&root.to_string_lossy());
                let op = Operator::new(b).context("构建 local storage 失败")?;
                Ok(Self { op, backend: "local".into() })
            }
            _ => {
                if cfg.endpoint.is_empty() || cfg.bucket.is_empty() {
                    return Err(anyhow!("OSS 未配置：endpoint/bucket 为空"));
                }
                let ep = if cfg.endpoint.starts_with("http") {
                    cfg.endpoint.clone()
                } else {
                    format!("https://{}", cfg.endpoint)
                };
                let mut b = Oss::default().endpoint(&ep).bucket(&cfg.bucket);
                b = b.access_key_id(&cfg.access_key_id).access_key_secret(&cfg.access_key_secret);
                if !cfg.prefix.is_empty() {
                    b = b.root(cfg.prefix.trim_matches('/'));
                }
                let op = Operator::new(b).context("构建 OSS Operator 失败")?;
                Ok(Self { op, backend: "oss".into() })
            }
        }
    }

    /// 内容寻址 key：objects/aa/aabb...
    pub fn object_key(hash: &str) -> String {
        format!("objects/{}/{}/{}", &hash[..2], &hash[2..4], hash)
    }

    pub async fn put_bytes(&self, key: &str, data: Vec<u8>) -> Result<()> {
        self.op.write(key, data).await.map_err(|e| anyhow!("写入 {key}: {e}"))?;
        Ok(())
    }

    pub async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        // M0: 整文件读入内存。大 CAD 文件的流式分片（write_from）在 M1 优化。
        let data = tokio::fs::read(path).await.with_context(|| format!("读取 {}", path.display()))?;
        self.op.write(key, data).await.map_err(|e| anyhow!("上传 {key}: {e}"))?;
        Ok(())
    }

    pub async fn get_to_file(&self, key: &str, dest: &Path) -> Result<()> {
        let buf = self.op.read(key).await.map_err(|e| anyhow!("下载 {key}: {e}"))?;
        tokio::fs::write(dest, buf.to_vec()).await?;
        Ok(())
    }

    pub async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        let buf = self.op.read(key).await.map_err(|e| anyhow!("读取 {key}: {e}"))?;
        Ok(buf.to_vec())
    }

    /// 列出某前缀下的 .json 事件 key（事件量级见设计文档风险 #4，M1 全量 list 足够）
    pub async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let entries = self.op.list(prefix).await.map_err(|e| anyhow!("list {prefix}: {e}"))?;
        Ok(entries
            .iter()
            .map(|e| e.path().to_string())
            .filter(|p| p.ends_with(".json") && !p.ends_with('/'))
            .collect())
    }

    pub async fn exists(&self, key: &str) -> bool {
        self.op.stat(key).await.is_ok()
    }

    /// 测试连接：试传/读/删一个探针对象，返回耗时 ms（§8.2）
    pub async fn probe(&self) -> Result<u128> {
        let key = format!("meta/_probe/{}.txt", uuid::Uuid::new_v4());
        let t0 = std::time::Instant::now();
        self.put_bytes(&key, b"pdm-probe".to_vec()).await?;
        let _ = self.op.read(&key).await.map_err(|e| anyhow!("读取探针: {e}"))?;
        self.op.delete(&key).await.map_err(|e| anyhow!("删除探针: {e}"))?;
        Ok(t0.elapsed().as_millis())
    }
}
