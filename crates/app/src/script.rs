//! 恢复脚本：生成、写盘、清单登记。
//!
//! 目录约定（指南 §5）：RCGUI 专用 `scripts/` 目录 + `manifest.json` 记录自己生成的文件，
//! 不按文件名前缀去猜、也不删除任何未经清单确认的文件。
//! 本模块**只写文件**：不执行脚本、不注册开机启动、不建计划任务。

use crate::config;
use crate::profiles;
use anyhow::{Context, Result};
use rcgui_profile::restore;
use std::path::PathBuf;

pub fn scripts_dir() -> PathBuf {
    config::data_dir().join("scripts")
}

pub fn script_path() -> PathBuf {
    scripts_dir().join("restore.ps1")
}

pub fn manifest_path() -> PathBuf {
    scripts_dir().join("manifest.json")
}

/// 生成并保存恢复脚本，返回 (脚本路径, 脚本正文)。
pub fn generate_and_save(cfg: &config::AppConfig) -> Result<(PathBuf, String)> {
    let doc = profiles::current_doc(cfg);
    let body = restore::generate(&doc, &cfg.rclone.url);
    let dir = scripts_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("创建 {}", dir.display()))?;
    let path = script_path();
    std::fs::write(&path, body.as_bytes()).with_context(|| format!("写入 {}", path.display()))?;
    update_manifest(&path, &body)?;
    Ok((path, body))
}

/// 只登记 RCGUI 自己生成的文件（同名条目原位更新，不重复累积）。
fn update_manifest(path: &std::path::Path, body: &str) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("restore.ps1")
        .to_string();
    let mut files: Vec<serde_json::Value> = std::fs::read(manifest_path())
        .ok()
        .and_then(|r| serde_json::from_slice::<serde_json::Value>(&r).ok())
        .and_then(|v| v.get("files").and_then(|f| f.as_array()).cloned())
        .unwrap_or_default();
    files.retain(|e| e.get("path").and_then(|p| p.as_str()) != Some(name.as_str()));
    files.push(serde_json::json!({
        "path": name,
        "purpose": "restore",
        "sha256": sha256_hex(body.as_bytes()),
        "generated_at": rcgui_profile::now_iso(),
        "app_version": env!("CARGO_PKG_VERSION"),
    }));
    let doc = serde_json::json!({ "schema_version": 1, "files": files });
    config::write_atomic(&manifest_path(), doc.to_string().as_bytes())
        .context("更新 scripts/manifest.json 失败")?;
    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}
