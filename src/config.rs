//! 配置层：机器级（device_id / 工作区注册表）+ 工作区级（config.toml）。
//! 设计文档 §8.3。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// 机器级配置目录：%LOCALAPPDATA%\pdm（无 LOCALAPPDATA 时回退 ~/.pdm）
pub fn machine_dir() -> PathBuf {
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(la).join("pdm");
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".pdm")
}

/// 机器级 device_id：一次生成，所有工作区共用（冲突展示 / tie-break 标识"哪台机器"）
pub fn ensure_device_id() -> Result<String> {
    let dir = machine_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join("device_id");
    if let Ok(s) = fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    fs::write(&path, &id)?;
    Ok(id)
}

// ---------- 工作区注册表 ----------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RegistryEntry {
    pub name: String,
    pub path: String,
    pub added_at: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Registry {
    #[serde(default)]
    pub workspaces: Vec<RegistryEntry>,
}

pub fn registry_path() -> PathBuf {
    machine_dir().join("workspaces.toml")
}

pub fn load_registry() -> Result<Registry> {
    let p = registry_path();
    if !p.exists() {
        return Ok(Registry::default());
    }
    let s = fs::read_to_string(&p).context("读取工作区注册表失败")?;
    toml::from_str(&s).context("解析工作区注册表失败")
}

/// 追加工作区（名字冲突自动加后缀）
pub fn registry_add(path: &std::path::Path, name_hint: Option<&str>) -> Result<String> {
    let mut reg = load_registry()?;
    let mut name = name_hint
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "ws".into()));
    let mut i = 2;
    while reg.workspaces.iter().any(|e| e.name == name) {
        name = format!("{name}-{i}");
        i += 1;
    }
    reg.workspaces.push(RegistryEntry {
        name: name.clone(),
        path: path.to_string_lossy().into_owned(),
        added_at: chrono::Utc::now().to_rfc3339(),
    });
    fs::create_dir_all(machine_dir())?;
    fs::write(registry_path(), toml::to_string_pretty(&reg)?)?;
    Ok(name)
}

pub fn registry_remove(name: &str) -> Result<bool> {
    let mut reg = load_registry()?;
    let before = reg.workspaces.len();
    reg.workspaces.retain(|e| e.name != name);
    if reg.workspaces.len() != before {
        fs::create_dir_all(machine_dir())?;
        fs::write(registry_path(), toml::to_string_pretty(&reg)?)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---------- 工作区级配置 ----------

/// OSS 连接配置。字段语义与 ltl-manage oss.py 对齐（.env 可直接复用）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct OssConfig {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub access_key_secret: String,
    #[serde(default)]
    pub prefix: String,
    /// oss | local（local: 对象落 store_root 或 <ws>/.pdm/store，M0 测试与离线用）
    #[serde(default = "default_backend")]
    pub backend: String,
    /// local 后端的自定义存储根（如共享盘目录，双工作区/双机同步测试用）
    #[serde(default)]
    pub store_root: String,
}

fn default_backend() -> String {
    "oss".into()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WsConfig {
    pub oss: OssConfig,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default = "default_prune_n")]
    pub prune_n: i64,
    #[serde(default = "default_prune_t")]
    pub prune_t_days: i64,
    /// 自动定期裁剪（M2）：按 auto_prune_hours 周期执行 §7.2 保护规则下的裁剪
    #[serde(default)]
    pub auto_prune: bool,
    #[serde(default = "default_ap_hours")]
    pub auto_prune_hours: i64,
}

fn default_port() -> u16 {
    8420
}
fn default_user() -> String {
    "local".into()
}
fn default_prune_n() -> i64 {
    10
}
fn default_prune_t() -> i64 {
    90
}
fn default_ap_hours() -> i64 {
    24
}

pub fn ws_config_path(ws_root: &std::path::Path) -> PathBuf {
    ws_root.join(".pdm").join("config.toml")
}

pub fn load_ws_config(ws_root: &std::path::Path) -> Result<WsConfig> {
    let s = fs::read_to_string(ws_config_path(ws_root)).context("读取工作区 config.toml 失败")?;
    toml::from_str(&s).context("解析工作区 config.toml 失败")
}

pub fn save_ws_config(ws_root: &std::path::Path, cfg: &WsConfig) -> Result<()> {
    let p = ws_config_path(ws_root);
    fs::create_dir_all(p.parent().unwrap())?;
    fs::write(p, toml::to_string_pretty(cfg)?).context("写入 config.toml 失败")
}
