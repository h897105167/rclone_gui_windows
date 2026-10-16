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
    /// 用户是否手动指定了 exe 路径（true 则不覆盖）
    #[serde(default)]
    pub exe_custom: bool,
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

/// 自动挂载项（勾选"自动挂载"后保存，下次启动自动执行）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoMount {
    pub fs: String,
    pub point: String,
    #[serde(default)]
    pub ro: bool,
    #[serde(default = "d_cache")]
    pub cache: String,
}
fn d_cache() -> String {
    "writes".into()
}

/// 自动服务项（勾选"自动启动"后保存，下次启动自动执行）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoServe {
    pub stype: String,
    pub fs: String,
    pub addr: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
}

/// 服务自定义名称映射（按 协议+远程+地址 匹配，便于跨重启稳定识别）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServeName {
    pub stype: String,
    pub fs: String,
    pub addr: String,
    pub name: String,
}

fn d_unit() -> i32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub rclone: RcloneCfg,
    /// 0 跟随系统 / 1 浅色 / 2 深色
    #[serde(default)]
    pub theme_mode: u8,
    /// 强调色：0 蓝 / 1 绿 / 2 紫 / 3 橙
    #[serde(default)]
    pub accent_idx: u8,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub silent_start: bool,
    #[serde(default)]
    pub http: HttpCfg,
    /// 插件启用表
    #[serde(default)]
    pub plugin_enabled: std::collections::HashMap<String, bool>,
    /// 自动挂载列表
    #[serde(default)]
    pub auto_mounts: Vec<AutoMount>,
    /// 自动服务列表
    #[serde(default)]
    pub auto_serves: Vec<AutoServe>,
    /// 远程路径历史（最近 10 条，最新在前）
    #[serde(default)]
    pub path_history: Vec<String>,
    /// 服务自定义名称映射
    #[serde(default)]
    pub serve_names: Vec<ServeName>,
    /// 全局上传限速（数值文本，空 = 不限；rclone 仅支持全局 tx:rx 限速）
    #[serde(default)]
    pub bw_up: String,
    #[serde(default = "d_unit")]
    pub bw_up_unit: i32,
    /// 全局下载限速（数值文本，空 = 不限）
    #[serde(default)]
    pub bw_down: String,
    #[serde(default = "d_unit")]
    pub bw_down_unit: i32,
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
        // 容忍 UTF-8 BOM（外部工具编辑配置文件时可能引入）
        let raw: &[u8] = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) { &raw[3..] } else { &raw };
        if let Ok(cfg) = serde_json::from_slice::<AppConfig>(raw) {
            return cfg;
        }
    }
    AppConfig::default()
}

pub fn save(cfg: &AppConfig) -> Result<()> {
    let data = serde_json::to_vec_pretty(cfg)?;
    write_atomic(&config_path(), &data)
}
