//! 应用配置：数据目录解析 + config.json 读写（原子写）
//! 便携模式：exe 旁存在 "portable" 文件时数据目录 = exe目录\data

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RcloneCfg {
    /// 0 = 托管（自动启动 rcd），1 = 连接已有实例
    #[serde(default)]
    pub mode: u8,
    #[serde(default)]
    pub exe: String,
    #[serde(default = "d_rc_port")]
    pub port: u16,
    #[serde(default = "d_rc_user")]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    /// attach 模式地址
    #[serde(default = "d_rc_url")]
    pub url: String,
}

fn d_rc_port() -> u16 {
    5574
}
fn d_rc_user() -> String {
    "gui".into()
}
fn d_rc_url() -> String {
    "http://127.0.0.1:5572".into()
}

impl Default for RcloneCfg {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_http_port")]
    pub port: u16,
}
fn d_true() -> bool {
    true
}
fn d_http_port() -> u16 {
    5273
}
impl Default for HttpCfg {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub rclone: RcloneCfg,
    /// 0 跟随系统 / 1 浅色 / 2 深色
    #[serde(default)]
    pub theme_mode: u8,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub silent_start: bool,
    #[serde(default)]
    pub http: HttpCfg,
    /// 插件启用表
    #[serde(default)]
    pub plugin_enabled: std::collections::HashMap<String, bool>,
}

impl Default for AppConfig {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

pub fn data_dir() -> PathBuf {
    let exe = std::env::current_exe().ok().map(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(Some(dir)) = exe {
        if dir.join("portable").exists() {
            return dir.join("data");
        }
    }
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(appdata).join("rclone-gui")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn write_atomic(file: &PathBuf, data: &[u8]) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

pub fn load() -> AppConfig {
    let p = config_path();
    if let Ok(raw) = std::fs::read(&p) {
        if let Ok(cfg) = serde_json::from_slice::<AppConfig>(&raw) {
            return cfg;
        }
    }
    AppConfig::default()
}

pub fn save(cfg: &AppConfig) -> Result<()> {
    let data = serde_json::to_vec_pretty(cfg)?;
    write_atomic(&config_path(), &data)
}
