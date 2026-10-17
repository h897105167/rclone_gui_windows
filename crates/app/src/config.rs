//! 应用配置：数据目录解析 + config.json 读写（原子写）
//! 便携模式：exe 旁存在 "portable" 文件时数据目录 = exe目录\data

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// rclone 连接配置。
///
/// **语义（重构指南 §1/§3）**：RCGUI 只连接用户指定的 RC endpoint，
/// 不启动、不终止、不重启 rclone 进程。因此这里没有 exe / 托管模式的概念，
/// 只有「连哪里、用什么凭据、启动时要不要自动连」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RcloneCfg {
    /// 完整 RC endpoint，如 `http://127.0.0.1:5572`
    #[serde(default = "d_rc_url")]
    pub url: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    /// 启动时自动连接（只是连 API，不启动任何进程）
    #[serde(default = "d_true")]
    pub connect_on_start: bool,
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

/// 自动挂载项（配置跟随远程：远程被删除时级联删除）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoMount {
    /// 远程名（不含冒号，如 "teldrive"）
    pub remote: String,
    /// 远程内子目录（相对路径；空串 = 远程根目录）
    #[serde(default)]
    pub sub_path: String,
    pub point: String,
    #[serde(default)]
    pub ro: bool,
    #[serde(default = "d_cache")]
    pub cache: String,
    /// 是否启用：停用后启动软件时不自动挂载（配置保留）
    #[serde(default = "d_true")]
    pub enabled: bool,
}
fn d_cache() -> String {
    "writes".into()
}

/// 自动服务项（配置跟随远程：远程被删除时级联删除）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoServe {
    pub stype: String,
    /// 远程名（不含冒号）
    pub remote: String,
    /// 远程内子目录（空串 = 远程根目录）
    #[serde(default)]
    pub sub_path: String,
    pub addr: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    /// 是否启用：停用后启动软件时不自动启动
    #[serde(default = "d_true")]
    pub enabled: bool,
}

/// 服务自定义名称映射（按 协议+远程+子路径+地址 匹配）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServeName {
    pub stype: String,
    pub remote: String,
    #[serde(default)]
    pub sub_path: String,
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
    ///
    /// **不再写入 config.json**：唯一落盘位置是档案文档
    /// `<数据目录>/profiles/default.json`（见 `profiles.rs`）。这里保留字段是为了
    /// 让既有 UI 代码继续可用，同时仍能读取旧版 config.json 里的同名字段。
    #[serde(default, skip_serializing)]
    pub auto_mounts: Vec<AutoMount>,
    /// 自动服务列表（同上：只从档案文档落盘）
    #[serde(default, skip_serializing)]
    pub auto_serves: Vec<AutoServe>,
    /// 远程路径历史（最近 10 条，最新在前）
    #[serde(default)]
    pub path_history: Vec<String>,
    /// 服务自定义名称映射（同上：只从档案文档落盘）
    #[serde(default, skip_serializing)]
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

/// 应用数据根目录。
///
/// - 便携模式（exe 旁边有 `portable` 文件）：`exe目录\data`
/// - 否则：`%APPDATA%\RCGUI`，并在首次启动时从旧目录 `%APPDATA%\rclone-gui` **复制**迁移。
///
/// 迁移原则（指南 §4）：复制而不是移动，旧目录**不删除、不修改**，随时可回滚；
/// 迁移失败则继续使用旧目录，绝不因为换目录导致用户数据「消失」。
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        if dir.join("portable").exists() {
            return dir.join("data");
        }
    }
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    // 展开可能的 8.3 短名（ADMINI~1 → Administrator），保证目录名与提示都正确
    let appdata = long_path(&PathBuf::from(appdata));
    resolve_data_dir(appdata.join("RCGUI"), appdata.join("rclone-gui"))
}


/// 把 8.3 短名（如 `ADMINI~1`）解析成真实长路径。
///
/// `APPDATA` / `TEMP` 这类环境变量在某些会话里是短名形式，拼出来的路径
/// 人类看不懂，传给资源管理器也可能打不开。`canonicalize` 会展开短名。
fn long_path(p: &std::path::Path) -> PathBuf {
    match std::fs::canonicalize(p) {
        Ok(c) => {
            let s = c.to_string_lossy().to_string();
            PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
        }
        Err(_) => p.to_path_buf(),
    }
}/// 选择数据目录，必要时执行一次性迁移。
fn resolve_data_dir(new_dir: PathBuf, legacy_dir: PathBuf) -> PathBuf {
    // 新目录已存在（已迁移过/全新安装）→ 直接用
    if new_dir.exists() || !legacy_dir.exists() {
        return new_dir;
    }
    match copy_tree(&legacy_dir, &new_dir) {
        Ok((files, bytes, skipped)) => {
            let marker = serde_json::json!({
                "schema_version": 1,
                "from": legacy_dir.display().to_string(),
                "to": new_dir.display().to_string(),
                "migrated_at": rcgui_profile::now_iso(),
                "files": files,
                "bytes": bytes,
                "skipped": skipped,
                // 回滚 = 删除新目录即可，旧目录从未被修改
                "rollback": "删除该 RCGUI 目录后重启，程序会回到旧目录 %APPDATA%\\rclone-gui"
            });
            let _ = write_atomic(&new_dir.join("migration.json"), marker.to_string().as_bytes());
            let mut msg = format!(
                "数据已从旧目录迁移到 {}（{} 个文件 / {:.1} MB；旧目录保留，可回滚）",
                new_dir.display(),
                files,
                bytes as f64 / 1024.0 / 1024.0
            );
            if !skipped.is_empty() {
                msg.push_str(&format!("；{} 个条目被跳过（详见 migration.json）", skipped.len()));
            }
            crate::profiles::push_notice(msg);
            new_dir
        }
        Err(e) => {
            // 只清理本次自己创建的不完整副本（进入本函数时 new_dir 尚不存在）
            let _ = std::fs::remove_dir_all(&new_dir);
            crate::profiles::push_notice(format!(
                "数据目录迁移失败（{e}），本次继续使用旧目录 {}",
                legacy_dir.display()
            ));
            legacy_dir
        }
    }
}

/// 递归复制目录树（跳过符号链接等特殊条目）。
///
/// **尽力而为**：单个文件失败（典型情况是 `rcd.log` 仍被运行中的 rclone 占用）
/// 只记为 skipped，不中断整个迁移——否则用户只要开着旧版 rclone，数据目录就永远迁不过去。
/// 返回 (文件数, 字节数, 跳过的条目)。
fn copy_tree(src: &Path, dst: &Path) -> Result<(u64, u64, Vec<String>)> {
    fn walk(
        src: &Path,
        dst: &Path,
        files: &mut u64,
        bytes: &mut u64,
        skipped: &mut Vec<String>,
    ) {
        if let Err(e) = std::fs::create_dir_all(dst) {
            skipped.push(format!("{}（{e}）", dst.display()));
            return;
        }
        let rd = match std::fs::read_dir(src) {
            Ok(rd) => rd,
            Err(e) => {
                skipped.push(format!("{}（{e}）", src.display()));
                return;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    skipped.push(format!("{} 内部条目（{e}）", src.display()));
                    continue;
                }
            };
            let path = entry.path();
            let to = dst.join(entry.file_name());
            // 用 symlink_metadata 判定类型：不跟随符号链接，避免复制目标逃出数据目录
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    skipped.push(format!("{}（{e}）", path.display()));
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                skipped.push(format!("{}（符号链接）", path.display()));
            } else if meta.is_dir() {
                walk(&path, &to, files, bytes, skipped);
            } else if meta.is_file() {
                match std::fs::copy(&path, &to) {
                    Ok(n) => {
                        *files += 1;
                        *bytes += n;
                    }
                    Err(e) => skipped.push(format!("{}（{e}）", path.display())),
                }
            } else {
                skipped.push(format!("{}（特殊文件）", path.display()));
            }
        }
    }
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut skipped: Vec<String> = Vec::new();
    // 只有「连根目录都建不出来」才算致命错误
    std::fs::create_dir_all(dst)
        .map_err(|e| anyhow::anyhow!("创建 {} 失败: {e}", dst.display()))?;
    walk(src, dst, &mut files, &mut bytes, &mut skipped);
    Ok((files, bytes, skipped))
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
    let mut cfg = load_raw();
    // 档案文档是「挂载 / 服务 / 预设名」的权威来源。
    // 档案不存在时，profiles 模块会把 config.json 里的旧字段迁移过去并落盘。
    if let Some(doc) = crate::profiles::load_doc(&cfg) {
        crate::profiles::apply_doc(&doc, &mut cfg);
    }
    cfg
}

/// 只读 config.json（应用偏好）。旧版本里可能仍带有挂载/服务字段，由调用方用档案覆盖。
fn load_raw() -> AppConfig {
    let p = config_path();
    if let Ok(raw) = std::fs::read(&p) {
        // 容忍 UTF-8 BOM（外部工具编辑配置文件时可能引入）
        let raw: &[u8] = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) { &raw[3..] } else { &raw };
        if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(raw) {
            migrate(&mut v);
            if let Ok(cfg) = serde_json::from_value::<AppConfig>(v) {
                return cfg;
            }
        }
    }
    AppConfig::default()
}

/// 旧格式迁移：auto_mounts/auto_serves/serve_names 中的
/// `fs: "name:sub/path"` 拆为 `remote: "name"` + `sub_path: "sub/path"`
fn migrate(v: &mut serde_json::Value) {
    migrate_rclone(v);
    /// "name:sub" → ("name", "sub")；"name:" → ("name", "")
    fn split_fs(fs: &str) -> (String, String) {
        match fs.find(':') {
            Some(i) => (
                fs[..i].trim().to_string(),
                fs[i + 1..].trim_matches('/').trim().to_string(),
            ),
            None => (fs.trim_end_matches(':').trim().to_string(), String::new()),
        }
    }

    let keys = ["auto_mounts", "auto_serves", "serve_names"];
    for key in keys {
        let Some(arr) = v.get_mut(key).and_then(|x| x.as_array_mut()) else {
            continue;
        };
        for it in arr.iter_mut() {
            // 仅迁移仍是旧格式（有 fs 无 remote）的条目
            if it.get("remote").is_none() {
                if let Some(fs) = it.get("fs").and_then(|x| x.as_str()) {
                    let (remote, sub) = split_fs(fs);
                    if let Some(obj) = it.as_object_mut() {
                        obj.insert("remote".into(), serde_json::json!(remote));
                        obj.insert("sub_path".into(), serde_json::json!(sub));
                        obj.remove("fs");
                    }
                }
            }
        }
    }
}

/// 旧 `rclone.mode/port/exe` → 新连接模型（`url` + `connect_on_start`）。
///
/// 迁移规则（按重构任务要求）：
/// - 旧 `mode`（0 托管 / 1 连接已有实例）**一律降级为「连接」语义**：不再自动启动进程；
/// - 旧托管模式本来就只认 `127.0.0.1:<port>`，因此用 `port` 拼出 endpoint；
/// - 旧连接模式保留用户填的 `url`；
/// - `connect_on_start` 默认 true：启动时尝试连接这个 endpoint，连不上就进离线模式。
fn migrate_rclone(v: &mut serde_json::Value) {
    let Some(rc) = v.get_mut("rclone").and_then(|x| x.as_object_mut()) else {
        return;
    };
    let had_mode = rc.contains_key("mode");
    let had_port = rc.get("port").and_then(|p| p.as_u64());
    let had_exe = rc.contains_key("exe");
    // 已经是新格式（没有旧字段）→ 不做任何改动，避免把默认值写坏
    if !had_mode && had_port.is_none() && !had_exe {
        return;
    }
    let mode = rc.get("mode").and_then(|m| m.as_u64()).unwrap_or(1);
    let url = rc
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let fallback = d_rc_url();
    let port_url: Option<String> = had_port
        .filter(|p| *p > 0)
        .map(|p| format!("http://127.0.0.1:{p}"));
    let mut new_url = url.clone();
    if mode == 0 {
        // 旧托管模式：原先连的就是 127.0.0.1:<port>，优先用端口拼
        if let Some(p) = port_url.clone() {
            new_url = p;
        } else if new_url.is_empty() {
            new_url = fallback.clone();
        }
    } else if new_url.is_empty() {
        new_url = port_url.unwrap_or(fallback);
    }
    if let Some(obj) = v.get_mut("rclone").and_then(|x| x.as_object_mut()) {
        obj.insert("url".into(), serde_json::json!(new_url));
        obj.entry("connect_on_start").or_insert(serde_json::json!(true));
        // 旧字段清除：不再有「托管模式」和「自带 rclone.exe」这两个概念
        obj.remove("mode");
        obj.remove("port");
        obj.remove("exe");
        obj.remove("exe_custom");
    }
}

pub fn save(cfg: &AppConfig) -> Result<()> {
    // 顺序很重要：档案先写，失败就整体放弃。
    // 因为 config.json 已不再包含挂载/服务字段（skip_serializing），若先写 config.json
    // 而档案写失败，这些预设就只剩内存里一份，重启即丢。
    crate::profiles::save_from_cfg(cfg)?;
    // 第二步：应用偏好（已不含挂载/服务列表）
    let data = serde_json::to_vec_pretty(cfg)?;
    write_atomic(&config_path(), &data)
}
