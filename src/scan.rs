//! 扫描 / 哈希 / 归组（设计文档 §2、§5）。
//! - 只扫 native/ 与 neutral/ 两个顶层目录，kind 由顶层目录决定
//! - part 归组：相对路径 stem（native/Parts/Bracket.CATPart + neutral/Parts/Bracket.step → part "Parts/Bracket"）
//! - 忽略规则来自 <ws>/.pdm/ignore.conf（init 时写默认值）

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const NATIVE_EXTS: &[&str] = &[
    "catpart", "catproduct", "catshape", "catdrawing", "sldprt", "sldasm", "slddrw", "scdoc",
    "prt", "ipt", "iam", "idw",
];
pub const NEUTRAL_EXTS: &[&str] = &["step", "stp", "stl", "jt", "cgr", "x_t", "x_b", "igs", "iges"];

pub fn default_ignore_rules() -> Vec<String> {
    vec![
        ".pdm/".into(),
        "~$*".into(),
        "*.sld~".into(),
        "*.bak".into(),
        "*.tmp".into(),
        "*.swp".into(),
        "Thumbs.db".into(),
        "desktop.ini".into(),
        ".DS_Store".into(),
    ]
}

pub fn load_ignore_rules(ws_root: &Path) -> Vec<String> {
    let p = ws_root.join(".pdm").join("ignore.conf");
    match std::fs::read_to_string(&p) {
        Ok(s) => s
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect(),
        Err(_) => default_ignore_rules(),
    }
}

/// 极简 glob：仅支持 `*`（段内通配）。`.pdm/` 规则匹配路径中任一段。
pub fn is_ignored(rel: &str, rules: &[String]) -> bool {
    let segs: Vec<&str> = rel.split('/').collect();
    for rule in rules {
        if let Some(dir) = rule.strip_suffix('/') {
            if segs.iter().any(|s| *s == dir) {
                return true;
            }
            continue;
        }
        if rule.contains('*') {
            // 对文件名段与全路径都试
            let last = *segs.last().unwrap_or(&"");
            if glob_match(rule, last) || glob_match(rule, rel) {
                return true;
            }
        } else if rel == rule || segs.iter().any(|s| *s == rule) {
            return true;
        }
    }
    false
}

pub fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    if !rest.starts_with(parts[0]) {
        return false;
    }
    rest = &rest[parts[0].len()..];
    for (i, part) in parts.iter().enumerate().skip(1) {
        if i == parts.len() - 1 {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(idx) => rest = &rest[idx + part.len()..],
            None => return false,
        }
    }
    true
}

/// native/Parts/Bracket.CATPart → ("native", "Parts/Bracket")；其余返回 None
pub fn part_key_for(rel: &str) -> Option<(String, String)> {
    let (top, rest) = if let Some(r) = rel.strip_prefix("native/") {
        ("native", r)
    } else if let Some(r) = rel.strip_prefix("neutral/") {
        ("neutral", r)
    } else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }
    let stem = match rest.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => stem,
        _ => rest,
    };
    Some((top.to_string(), stem.to_string()))
}

pub fn ext_of(rel: &str) -> String {
    rel.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScannedFile {
    pub rel_path: String,
    pub kind: String,
    pub size: u64,
    pub hash: String,
}

pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 1024 * 256];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hasher.finalize().to_hex().to_string(), size))
}

pub async fn hash_file_async(path: PathBuf) -> Result<(String, u64)> {
    tokio::task::spawn_blocking(move || hash_file(&path))
        .await
        .map_err(|e| anyhow::anyhow!("hash task: {e}"))?
}

/// 阻塞式全量扫描（在 spawn_blocking 中执行）。忽略规则过滤 + BLAKE3。
pub fn scan_workspace_blocking(ws_root: &Path, rules: &[String]) -> Result<Vec<ScannedFile>> {
    let mut out = Vec::new();
    for top in ["native", "neutral"] {
        let dir = ws_root.join(top);
        if !dir.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = match entry.path().strip_prefix(ws_root) {
                Ok(p) => p.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if is_ignored(&rel, rules) {
                continue;
            }
            let (hash, size) = hash_file(entry.path())
                .with_context(|| format!("哈希 {rel}"))?;
            out.push(ScannedFile { rel_path: rel, kind: top.to_string(), size, hash });
        }
    }
    Ok(out)
}

pub async fn scan_workspace(ws_root: &Path, rules: &[String]) -> Result<Vec<ScannedFile>> {
    let root = ws_root.to_path_buf();
    let rules: Vec<String> = rules.to_vec();
    tokio::task::spawn_blocking(move || scan_workspace_blocking(&root, &rules))
        .await
        .map_err(|e| anyhow::anyhow!("scan task: {e}"))?
}

/// 按 part 归组
pub fn group_by_part(files: &[ScannedFile]) -> BTreeMap<String, Vec<ScannedFile>> {
    let mut map: BTreeMap<String, Vec<ScannedFile>> = BTreeMap::new();
    for f in files {
        if let Some((_, part)) = part_key_for(&f.rel_path) {
            map.entry(part).or_default().push(f.clone());
        }
    }
    map
}
