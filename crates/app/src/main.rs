// release 版不弹出控制台窗口（debug 保留便于调试）
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Rclone GUI（Windows 桌面版）：Rust + Slint + rclone RC + WASM 插件
//!
//! 架构：主线程跑 Slint UI；后台 std 线程内 tokio runtime 负责
//! rclone 生命周期 / 内嵌 HTTP / 统计轮询 / UI 命令处理；托盘独立线程。

mod config;
mod http;
mod kv;

use rclone_client::{discover_exe, generate_password, local_fs, RcloneManager};
use serde_json::{json, Value};
use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc;

slint::include_modules!();

// ---------------- 命令 ----------------

#[derive(Debug)]
enum Cmd {
    RefreshDashboard,
    FilesInit,
    /// 文件页刷新（seq: UI 请求序号，-1 表示总是应用结果）
    FilesRefresh { seq: i32 },
    /// 进入子目录（成功后才更新路径，防止网络慢时拼出不存在的路径）
    FilesEnter { name: String, seq: i32 },
    /// 返回上级目录
    FilesUp { seq: i32 },
    FilesDownload { name: String, local: PathBuf },
    FilesMkdir,
    FilesDel { name: String, is_dir: bool },
    FilesRename { from: String, to: String },
    FilesCopy { name: String },
    FilesCopyLink { name: String },
    MountsRefresh,
    MountAdd,
    MountRemove { point: String },
    /// 从历史记录里选择远程路径（scope: 0 挂载 / 1 服务）
    HistPick { scope: i32, path: String },
    BrowseOpen { scope: i32 },
    BrowseGoto { name: String },
    BrowseOk,
    RemotesRefresh,
    /// 搜索文本变化（防抖后触发；scope: 0 远程 / 1 挂载 / 2 服务）
    SearchApply { scope: i32 },
    RemoteAdd,
    RemoteEditLoad { name: String },
    RemoteDelete { name: String },
    RemoteSave,
    RemoteCancel,
    RemoteTypeChanged,
    ServesRefresh,
    ServeStart,
    ServeStop { id: String },
    /// 传输页：进入 / 刷新
    TransfersEnter,
    HideDoneChanged,
    /// 应用全局限速（上传/下载）
    LimitApply,
    JobStop { id: i64 },
    OptionsLoad,
    OptionsSave,
    PluginsRescan,
    PluginToggle { idx: i32 },
    IhSaveConfig,
    IhAddRoot,
    IhDelRoot { id: String },
    IhRootDefault { id: String },
    IhAddToken,
    IhRevokeToken { id: String },
    IhImagesRefresh,
    IhImageDel { path: String },
    CopyText { text: String },
    SettingsSave,
    SettingsTest,
    Quit,
}

struct Ctx {
    mgr: Arc<RcloneManager>,
    host: Arc<plugin_host::PluginHost>,
    store: Arc<kv::KvStore>,
    exe: PathBuf,
    cfg: Arc<std::sync::Mutex<config::AppConfig>>,
    http_restart: Arc<tokio::sync::Notify>,
    /// (name, type, usage) 全量远程缓存，筛选时避免重复请求
    remotes_cache: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    /// config/providers 结果缓存（后端类型参数模板）
    providers_cache: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    /// 活动挂载缓存（含挂载中的 pending 项），用于筛选与盘符重复检测
    mounts_cache: Arc<std::sync::Mutex<Vec<MountRow>>>,
    /// 正在挂载中的 (rclone fs, 挂载点)
    pending_mounts: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// 运行中服务缓存，用于筛选
    serves_cache: Arc<std::sync::Mutex<Vec<ServeRow>>>,
    /// 最近一次已下发的全局限速 rate 字符串（避免重复调用 core/bwlimit）
    last_bwlimit: Arc<std::sync::Mutex<String>>,
}

impl Ctx {
    fn caps(&self) -> Arc<kv::RealCaps> {
        Arc::new(kv::RealCaps {
            mgr: self.mgr.clone(),
            store: self.store.clone(),
        })
    }
}

// ---------------- 工具 ----------------

fn toast_ui(ui: &MainWindow, kind: i32, msg: impl Into<String>) {
    ui.global::<ToastG>().set_kind(kind);
    ui.global::<ToastG>().set_msg(msg.into().into());
    ui.global::<ToastG>().set_show(true);
    let weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(2600), move || {
        if let Some(u) = weak.upgrade() {
            u.global::<ToastG>().set_show(false);
        }
    });
}

fn toast(ui: &Weak<MainWindow>, kind: i32, msg: impl Into<String>) {
    let msg = msg.into();
    let _ = ui.upgrade_in_event_loop(move |ui| toast_ui(&ui, kind, msg));
}

/// 搜索框防抖：记录每个 scope 最近一次输入时间，由 UI 定时器在闲置 300ms 后触发。
/// scope: 0 远程 / 1 挂载 / 2 服务 / 3 传输-远程限速 / 4 传输-服务限速
#[derive(Clone, Default)]
struct SearchDebounce {
    at: Arc<std::sync::Mutex<[Option<std::time::Instant>; 5]>>,
}

impl SearchDebounce {
    fn bump(&self, scope: usize) {
        if let Ok(mut a) = self.at.lock() {
            a[scope] = Some(std::time::Instant::now());
        }
    }

    /// 若已闲置超过 wait，则取走该 scope 的触发标记并返回 true（只触发一次）
    fn due(&self, scope: usize, wait: std::time::Duration) -> bool {
        if let Ok(mut a) = self.at.lock() {
            if let Some(t) = a[scope] {
                if t.elapsed() >= wait {
                    a[scope] = None;
                    return true;
                }
            }
        }
        false
    }
}

/// 生成下一个默认服务名称 Serve_NN
fn next_serve_name(cfg: &config::AppConfig) -> String {
    let mut max_n = 0u32;
    for s in &cfg.serve_names {
        if let Some(rest) = s.name.strip_prefix("Serve_") {
            if let Ok(n) = rest.parse::<u32>() {
                max_n = max_n.max(n);
            }
        }
    }
    format!("Serve_{:02}", max_n + 1)
}

/// 按 (协议, fs, 地址) 查找或分配服务名称，并持久化映射
fn ensure_serve_name(ctx: &Arc<Ctx>, stype: &str, fs: &str, addr: &str) -> String {
    let mut cfg = ctx.cfg.lock().unwrap();
    if let Some(s) = cfg
        .serve_names
        .iter()
        .find(|s| s.stype == stype && s.fs == fs && s.addr == addr)
    {
        return s.name.clone();
    }
    let name = next_serve_name(&cfg);
    cfg.serve_names.push(config::ServeName {
        stype: stype.to_string(),
        fs: fs.to_string(),
        addr: addr.to_string(),
        name: name.clone(),
    });
    let _ = config::save(&cfg);
    name
}

/// 记录远程路径历史（去重、最新在前、最多 10 条），并同步到两个界面
async fn push_history(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, path: &str) {
    let path = path.trim().to_string();
    if path.is_empty() {
        return;
    }
    let history = {
        let mut cfg = ctx.cfg.lock().unwrap();
        cfg.path_history.retain(|p| p != &path);
        cfg.path_history.insert(0, path);
        cfg.path_history.truncate(10);
        let _ = config::save(&cfg);
        cfg.path_history.clone()
    };
    set_history(ui, &history);
}

fn set_history(ui: &Weak<MainWindow>, history: &[String]) {
    let items: Vec<slint::SharedString> = history.iter().map(|s| s.as_str().into()).collect();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let m = ModelRc::from(Rc::new(VecModel::from(items.clone())));
        ui.global::<MountsG>().set_history(m.clone());
        ui.global::<ServesG>().set_history(m);
    });
}

fn reg_dword(path: &str, name: &str) -> Option<u32> {
    use winreg::enums::HKEY_CURRENT_USER;
    let hk = winreg::RegKey::predef(HKEY_CURRENT_USER);
    hk.open_subkey(path).ok()?.get_value::<u32, _>(name).ok()
}

fn apps_use_light() -> bool {
    reg_dword(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "AppsUseLightTheme",
    )
    .map(|v| v == 1)
    .unwrap_or(true)
}

fn apply_theme(ui: &MainWindow, mode: u8) {
    let dark = match mode {
        1 => false,
        2 => true,
        _ => !apps_use_light(),
    };
    ui.global::<ThemeG>().set_dark(dark);
}

fn get_autostart() -> bool {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE};
    let hk = winreg::RegKey::predef(HKEY_CURRENT_USER);
    hk.open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", KEY_QUERY_VALUE)
        .ok()
        .and_then(|k| k.get_value::<String, _>("RcloneGUI").ok())
        .map(|v| v.contains("rclone-gui"))
        .unwrap_or(false)
}

fn set_autostart(on: bool, silent: bool) {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE};
    let hk = winreg::RegKey::predef(HKEY_CURRENT_USER);
    let Ok(run) = hk.open_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Run",
        KEY_SET_VALUE | KEY_QUERY_VALUE,
    ) else {
        return;
    };
    if on {
        if let Ok(exe) = std::env::current_exe() {
            let val = if silent {
                format!("\"{}\" --silent", exe.display())
            } else {
                format!("\"{}\"", exe.display())
            };
            let _ = run.set_value("RcloneGUI", &val);
        }
    } else {
        let _ = run.delete_value("RcloneGUI");
    }
}

fn clipboard_copy(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
}

fn fmt_bytes(n: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n;
    for u in units {
        if v < 1024.0 {
            return format!("{v:.1} {u}");
        }
        v /= 1024.0;
    }
    format!("{v:.1} PB")
}

fn winfsp_installed() -> (bool, String) {
    let paths = [
        r"C:\Program Files (x86)\WinFsp\bin",
        r"C:\Program Files\WinFsp\bin",
    ];
    for p in paths {
        if Path::new(p).exists() {
            return (true, String::new());
        }
    }
    (
        false,
        "未检测到 WinFsp。挂载功能需要安装 WinFsp：https://winfsp.dev/rel/ 安装后重启本程序。".into(),
    )
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn split_name(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

fn parse_kv_lines(text: &str) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(eq) = line.find('=') {
            let k = line[..eq].trim();
            let v = line[eq + 1..].trim();
            if !k.is_empty() {
                m.insert(k.to_string(), Value::String(v.to_string()));
            }
        }
    }
    m
}

fn fmt_duration(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}s", s)
    }
}

async fn apply_remotes_filter(ui: &Weak<MainWindow>, full: &[(String, String, String)]) {
    let filter = read_ui(ui, |u| u.global::<RemotesG>().get_filter().to_string())
        .await
        .unwrap_or_default()
        .to_lowercase();
    let rows: Vec<RemoteRow> = full
        .iter()
        .filter(|(n, _, _)| filter.is_empty() || n.to_lowercase().contains(&filter))
        .map(|(n, t, u)| RemoteRow {
            name: n.clone().into(),
            rtype: t.clone().into(),
            usage: u.clone().into(),
        })
        .collect();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.global::<RemotesG>()
            .set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    });
}

async fn apply_mounts_filter(ui: &Weak<MainWindow>, full: &[MountRow]) {
    let q = read_ui(ui, |u| u.global::<MountsG>().get_search().to_string())
        .await
        .unwrap_or_default()
        .to_lowercase();
    let rows: Vec<MountRow> = full
        .iter()
        .filter(|r| {
            q.is_empty()
                || r.fs.to_lowercase().contains(&q)
                || r.mount_point.to_lowercase().contains(&q)
        })
        .cloned()
        .collect();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.global::<MountsG>()
            .set_mounts(ModelRc::from(Rc::new(VecModel::from(rows))));
    });
}

async fn apply_serves_filter(ui: &Weak<MainWindow>, full: &[ServeRow]) {
    let q = read_ui(ui, |u| u.global::<ServesG>().get_search().to_string())
        .await
        .unwrap_or_default()
        .to_lowercase();
    let rows: Vec<ServeRow> = full
        .iter()
        .filter(|r| {
            q.is_empty()
                || r.id.to_lowercase().contains(&q)
                || r.name.to_lowercase().contains(&q)
                || r.stype.to_lowercase().contains(&q)
                || r.fs.to_lowercase().contains(&q)
                || r.addr.to_lowercase().contains(&q)
        })
        .cloned()
        .collect();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.global::<ServesG>()
            .set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    });
}

// ---------------- 传输页：限速 / 后台任务 ----------------

/// 推断 fs 对应的远程名；本地路径（盘符 / 以 //?/ 开头的 Windows 路径）返回 None
fn remote_name_of(fs: &str) -> Option<String> {
    let fs = fs.strip_prefix("//?/").unwrap_or(fs);
    match fs.find(':') {
        Some(i) => {
            let name = &fs[..i];
            if name.is_empty()
                || name.len() == 1
                || name.contains('/')
                || name.contains('\\')
            {
                None
            } else {
                Some(name.to_string())
            }
        }
        None => None,
    }
}

/// 速度格式化（单位随量级动态变化，最小 B；0 显示为 —）
fn fmt_speed(v: f64) -> String {
    if v <= 0.0 {
        "—".into()
    } else {
        format!("{}/s", fmt_bytes(v))
    }
}

/// 限速单位索引 → 倍率：0=B 1=KB 2=MB 3=GB
fn limit_unit_mul(u: i32) -> f64 {
    match u {
        0 => 1.0,
        1 => 1024.0,
        2 => 1024.0 * 1024.0,
        3 => 1024.0 * 1024.0 * 1024.0,
        _ => 1024.0,
    }
}

/// 解析限速草稿（数值 + 单位）为字节/秒；空或非正数返回 None（表示不限）
fn parse_limit(text: &str, unit: i32) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let n: f64 = t.parse().ok()?;
    if n <= 0.0 {
        return None;
    }
    Some(n * limit_unit_mul(unit))
}

/// 字节/秒 → rclone bwlimit rate 片段；None / 非正表示 "off"
fn byte_rate_str(v: Option<f64>) -> String {
    match v {
        Some(x) if x > 0.0 => format!("{}B", x.round() as i64),
        _ => "off".into(),
    }
}

/// 构建「后台任务」列表（job/list + job/status + core/transferred 关联推导）
async fn build_tasks(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let client = ctx.mgr.client.read().await.clone();
    let (hide, filter) = read_ui(ui, |u| {
        let g = u.global::<TransfersG>();
        (
            g.get_hide_done(),
            g.get_task_search().to_string().to_lowercase(),
        )
    })
    .await
    .unwrap_or((false, String::new()));
    let ids = client.job_list().await.unwrap_or_default();

    // group -> (文件名, srcFs, dstFs, 已完成字节)
    let mut tmap: std::collections::HashMap<String, (String, String, String, f64)> =
        std::collections::HashMap::new();
    if let Ok(v) = client.core_transferred().await {
        let arr = v
            .get("transferred")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        for it in arr {
            let group = it.get("group").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let src = it.get("srcFs").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let dst = it.get("dstFs").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let bytes = it.get("bytes").and_then(|x| x.as_f64()).unwrap_or(0.0);
            tmap.insert(group, (name, src, dst, bytes));
        }
    }

    // 运行中传输速度按 group 汇总；同时汇总全局上传/下载速度
    let stats = client.core_stats().await.ok();
    let mut rmap: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let (mut up_total, mut down_total) = (0.0f64, 0.0f64);
    if let Some(s) = &stats {
        for t in &s.transferring {
            if !t.group.is_empty() {
                *rmap.entry(t.group.clone()).or_insert(0.0) += t.speed;
            }
            // 上传：目标是远程；下载：来源是远程
            if remote_name_of(&t.dst_fs).is_some() {
                up_total += t.speed;
            }
            if remote_name_of(&t.src_fs).is_some() {
                down_total += t.speed;
            }
        }
    }
    {
        let (up, down) = (fmt_speed(up_total), fmt_speed(down_total));
        let _ = ui.upgrade_in_event_loop(move |ui| {
            let g = ui.global::<TransfersG>();
            g.set_up_speed(up.into());
            g.set_down_speed(down.into());
        });
    }

    let serves = ctx.serves_cache.lock().unwrap().clone();
    let mut rows: Vec<TaskRow> = Vec::new();
    for id in &ids {
        let st = client.job_status(*id).await.unwrap_or(json!({}));
        let finished = st.get("finished").and_then(|x| x.as_bool()).unwrap_or(false);
        let success = st.get("success").and_then(|x| x.as_bool()).unwrap_or(false);
        let duration = st.get("duration").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let group = format!("job/{}", id);
        let (fname, src, dst, bytes) = tmap
            .get(&group)
            .cloned()
            .unwrap_or((String::new(), String::new(), String::new(), 0.0));

        let src_remote = remote_name_of(&src);
        let dst_remote = remote_name_of(&dst);
        let (op, remote) = if dst_remote.is_some() && src_remote.is_none() {
            ("上传".to_string(), dst_remote.unwrap())
        } else if src_remote.is_some() && dst_remote.is_none() {
            ("下载".to_string(), src_remote.unwrap())
        } else if let Some(r) = dst_remote.or(src_remote) {
            (String::new(), r)
        } else {
            (String::new(), String::new())
        };

        let mut svc = String::new();
        for sv in &serves {
            if !sv.fs.is_empty()
                && (src.starts_with(sv.fs.as_str()) || dst.starts_with(sv.fs.as_str()))
            {
                svc = sv.name.as_str().to_string();
                break;
            }
        }

        if hide && finished {
            continue;
        }
        let status = if !finished {
            "运行中"
        } else if success {
            "已完成"
        } else {
            "失败"
        }
        .to_string();
        let speed = if !finished {
            rmap.get(&group).map(|v| fmt_speed(*v)).unwrap_or_else(|| "—".into())
        } else if duration > 0.0 {
            fmt_speed(bytes / duration)
        } else {
            "—".into()
        };
        rows.push(TaskRow {
            id: *id as i32,
            op: op.into(),
            remote: remote.into(),
            path: fname.into(),
            svc: svc.into(),
            status: status.into(),
            duration: fmt_duration(duration).into(),
            speed: speed.into(),
            done: finished,
        });
    }
    rows.sort_by(|a, b| b.id.cmp(&a.id));
    // 任务搜索过滤（匹配 标签/远程/路径/服务/状态）
    if !filter.is_empty() {
        rows.retain(|t| {
            t.op.as_str().contains(&filter)
                || t.remote.as_str().to_lowercase().contains(&filter)
                || t.path.as_str().to_lowercase().contains(&filter)
                || t.svc.as_str().to_lowercase().contains(&filter)
                || t.status.as_str().contains(&filter)
        });
    }

    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.global::<TransfersG>()
            .set_tasks(ModelRc::from(Rc::new(VecModel::from(rows))));
    });
}

/// 展示当前生效的全局限速（空字符串表示不展示）
fn set_effective(ui: &Weak<MainWindow>, text: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.global::<TransfersG>().set_effective(text.into());
    });
}

/// 把配置中的全局上传/下载限速下发到 rclone（tx:rx，值无变化时不重复调用）
async fn apply_global_limit(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let client = ctx.mgr.client.read().await.clone();
    let (up, down) = {
        let cfg = ctx.cfg.lock().unwrap();
        (
            parse_limit(&cfg.bw_up, cfg.bw_up_unit),
            parse_limit(&cfg.bw_down, cfg.bw_down_unit),
        )
    };
    let rate = format!("{}:{}", byte_rate_str(up), byte_rate_str(down));
    let changed = {
        let mut last = ctx.last_bwlimit.lock().unwrap();
        if *last == rate {
            false
        } else {
            *last = rate.clone();
            true
        }
    };
    if changed {
        let _ = client.bwlimit(&rate).await;
    }
    let disp = if up.is_none() && down.is_none() {
        String::new()
    } else {
        format!(
            "上传 {} · 下载 {}",
            up.map(fmt_bytes).unwrap_or_else(|| "不限".into()),
            down.map(fmt_bytes).unwrap_or_else(|| "不限".into())
        )
    };
    set_effective(ui, disp);
}

/// 从 config/providers 响应中提取指定类型的字段列表
fn build_fields_from_provider(providers: &Value, type_name: &str) -> (String, Vec<FieldRow>) {
    let empty = (String::new(), Vec::new());
    let list = match providers.get("providers").and_then(|p| p.as_array()) {
        Some(l) => l,
        None => return empty,
    };
    let provider = match list.iter().find(|p| {
        p.get("Name")
            .and_then(|n| n.as_str())
            .map(|n| n == type_name)
            .unwrap_or(false)
    }) {
        Some(p) => p,
        None => return empty,
    };
    let desc = provider
        .get("Description")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    let options = match provider.get("Options").and_then(|o| o.as_array()) {
        Some(o) => o,
        None => return (desc, Vec::new()),
    };
    let mut fields = Vec::new();
    for opt in options {
        let name = opt.get("Name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
        if name.is_empty() {
            continue;
        }
        let help = opt.get("Help").and_then(|h| h.as_str()).unwrap_or_default().to_string();
        let ty = opt.get("Type").and_then(|t| t.as_str()).unwrap_or("string");
        let required = opt.get("Required").and_then(|r| r.as_bool()).unwrap_or(false);
        let advanced = opt.get("Advanced").and_then(|a| a.as_bool()).unwrap_or(false);
        let sensitive = opt.get("Sensitive").and_then(|s| s.as_bool()).unwrap_or(false);
        let default = opt.get("Default").and_then(|d| match d {
            Value::String(s) => Some(s.clone()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }).unwrap_or_default();
        let kind = if ty == "bool" {
            "bool"
        } else if sensitive || ty == "password" {
            "password"
        } else if ty == "int" || ty == "float" || ty == "Duration" || ty == "SizeSuffix" {
            "number"
        } else {
            "text"
        };
        // 提取 label：Help 的第一句话或 Name
        let label = if !help.is_empty() {
            help.split('.').next().unwrap_or(&name).trim().to_string()
        } else {
            name.clone()
        };
        fields.push(FieldRow {
            name: name.into(),
            label: label.into(),
            kind: kind.into(),
            value: default.into(),
            required,
            help: help.into(),
            is_advanced: advanced,
            options: String::new().into(),
        });
    }
    (desc, fields)
}

/// 把 FieldRow 列表转为 rclone config 参数 JSON
fn fields_to_params(fields: &[FieldRow]) -> Value {
    let mut map = serde_json::Map::new();
    for f in fields {
        let val = f.value.to_string();
        if val.is_empty() && !f.required {
            continue;
        }
        match f.kind.as_str() {
            "bool" => {
                if val == "true" {
                    map.insert(f.name.to_string(), Value::Bool(true));
                }
            }
            "number" => {
                if let Ok(n) = val.parse::<i64>() {
                    map.insert(f.name.to_string(), Value::Number(n.into()));
                } else if !val.is_empty() {
                    map.insert(f.name.to_string(), Value::String(val));
                }
            }
            _ => {
                if !val.is_empty() {
                    map.insert(f.name.to_string(), Value::String(val));
                }
            }
        }
    }
    Value::Object(map)
}

/// 用现有远程参数值填充 FieldRow 列表
fn fill_fields_with_existing(fields: &mut [FieldRow], existing: &serde_json::Map<String, Value>) {
    for f in fields.iter_mut() {
        if let Some(v) = existing.get(f.name.as_str()) {
            f.value = match v {
                Value::String(s) => s.clone().into(),
                Value::Bool(b) => b.to_string().into(),
                Value::Number(n) => n.to_string().into(),
                _ => v.to_string().into(),
            };
        }
    }
}

/// 在 UI 线程执行闭包并取回结果。
/// Slint 的 Weak::upgrade() 有线程归属检查，后台线程调用必然返回 None；
/// 后台命令读取 UI 属性必须通过本函数桥接到 UI 线程。
async fn read_ui<T, F>(ui: &Weak<MainWindow>, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(MainWindow) -> T + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    if ui.upgrade_in_event_loop(move |u| {
        let _ = tx.send(f(u));
    }).is_err() {
        return None;
    }
    rx.await.ok()
}

/// 显示路径（"root://name/sub"）→ rclone fs（"name:sub"）；手输的 "name:sub" 原样返回
fn display_to_fs(d: &str) -> String {
    let d = d.trim();
    if let Some(rest) = d.strip_prefix("root://") {
        let rest = rest.trim_matches('/');
        if rest.is_empty() {
            return String::new();
        }
        match rest.find('/') {
            Some(i) => format!("{}:{}", &rest[..i], &rest[i + 1..]),
            None => format!("{}:", rest),
        }
    } else {
        d.to_string()
    }
}

/// rclone fs（"name:sub"）→ 显示路径（"root://name/sub"）
#[allow(dead_code)]
fn fs_to_display(fs: &str) -> String {
    match fs.find(':') {
        Some(i) => format!("root://{}/{}", &fs[..i], fs[i + 1..].trim_matches('/')),
        None => format!("root://{}", fs),
    }
}

/// 拆分显示路径 → (rclone fs, remote 子路径)；根返回 None
fn split_display(d: &str) -> Option<(String, String)> {
    let rest = d.trim().strip_prefix("root://")?.trim_matches('/');
    if rest.is_empty() {
        return None;
    }
    match rest.find('/') {
        Some(i) => Some((format!("{}:", &rest[..i]), rest[i + 1..].to_string())),
        None => Some((format!("{}:", rest), String::new())),
    }
}

/// 显示路径的上级；已是根则返回 None
fn display_parent(d: &str) -> Option<String> {
    let d = d.trim().trim_end_matches('/');
    let rest = d.strip_prefix("root://")?;
    match rest.rfind('/') {
        Some(_) => Some(format!("root://{}", rest.rsplit_once('/').unwrap().0)),
        None => Some("root://".into()),
    }
}

/// 挂载点规范化：单个字母或 "X:" 补全为 "X:\"，避免 rclone 把 "T" 当相对目录挂到软件目录下
fn normalize_mount_point(p: &str) -> String {
    let t = p.trim();
    let b = t.as_bytes();
    if b.len() == 1 && b[0].is_ascii_alphabetic() {
        return format!("{}:\\", b[0].to_ascii_uppercase() as char);
    }
    if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return format!("{}:\\", b[0].to_ascii_uppercase() as char);
    }
    if b.len() == 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') && b[0].is_ascii_alphabetic() {
        return format!("{}:\\", b[0].to_ascii_uppercase() as char);
    }
    t.to_string()
}

/// 列出显示路径的内容；dirs_only=true 只保留文件夹
/// 根路径 "root://" 列出所有仓库名
async fn list_display(ctx: &Arc<Ctx>, display: &str, dirs_only: bool) -> (Vec<(String, bool)>, String) {
    let d = display.trim();
    let rest = d.trim_end_matches('/').strip_prefix("root://").unwrap_or("");
    if rest.is_empty() {
        // 根：列出仓库
        let client = ctx.mgr.client.read().await.clone();
        let names = ctx
            .remotes_cache
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _, _)| n.clone())
            .collect::<Vec<_>>();
        let names = if names.is_empty() {
            client.list_remotes().await.unwrap_or_default()
        } else {
            names
        };
        return (names.into_iter().map(|n| (n, true)).collect(), "root://".into());
    }
    let Some((fs, remote)) = split_display(d) else {
        return (vec![], "root://".into());
    };
    let client = ctx.mgr.client.read().await.clone();
    let r = if remote.is_empty() { "".to_string() } else { remote.clone() };
    match client.list(&fs, &r, None).await {
        Ok(entries) => {
            let items: Vec<(String, bool)> = entries
                .into_iter()
                .filter(|e| !dirs_only || e.is_dir)
                .map(|e| (e.name, e.is_dir))
                .collect();
            (items, format!("root://{}", rest))
        }
        Err(_) => (vec![], format!("root://{}", rest)),
    }
}

/// 从 UI 读取当前 FieldRow 列表（必须在后台线程通过 read_ui 调用）
async fn read_fields_from_ui(ui: &Weak<MainWindow>) -> Vec<FieldRow> {
    read_ui(ui, |u| {
        let m = u.global::<RemotesG>().get_edit_fields();
        (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
    })
    .await
    .unwrap_or_default()
}

/// 获取 providers（优先缓存，否则网络请求）
async fn get_providers(ctx: &Arc<Ctx>) -> Value {
    {
        let cache = ctx.providers_cache.lock().unwrap();
        if let Some(v) = cache.as_ref() {
            return v.clone();
        }
    }
    let client = ctx.mgr.client.read().await.clone();
    let v = client.config_providers().await.unwrap_or(json!({}));
    *ctx.providers_cache.lock().unwrap() = Some(v.clone());
    v
}

/// 更新编辑器字段列表和描述
fn update_editor_fields(ui: &Weak<MainWindow>, providers: &Value, type_name: &str) {
    let (desc, fields) = build_fields_from_provider(providers, type_name);
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let g = ui.global::<RemotesG>();
        g.set_provider_desc(desc.into());
        g.set_edit_fields(ModelRc::from(Rc::new(VecModel::from(fields))));
    });
}

fn b64(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn fmt_mtime(mod_time: &str) -> String {
    // rclone 返回 RFC3339（如 2026-01-01T12:34:56.789Z）→ 截断到分钟
    if mod_time.len() >= 16 {
        format!("{} {}", &mod_time[..10], &mod_time[11..16])
    } else {
        mod_time.to_string()
    }
}

/// 释放内嵌 rclone.exe 到临时目录，返回路径
fn extract_embedded_rclone() -> Option<String> {
    let embedded = option_env!("RCLONE_EMBEDDED")?;
    if embedded.is_empty() {
        return None;
    }
    let bytes = std::fs::read(embedded).ok()?;
    let dir = std::env::temp_dir().join("rclone-gui-embedded");
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join("rclone.exe");
    if dest.exists() {
        if let Ok(meta) = std::fs::metadata(&dest) {
            if meta.len() as usize == bytes.len() {
                return Some(dest.display().to_string());
            }
        }
    }
    std::fs::write(&dest, &bytes).ok()?;
    Some(dest.display().to_string())
}

// ---------------- 主入口 ----------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let silent_flag = args.iter().any(|a| a == "--silent");

    // 单实例：命名互斥量；重复启动时通过本地端口通知已有实例弹出窗口
    if !acquire_single_instance() {
        notify_show();
        return;
    }

    // 配置
    let mut cfg = config::load();
    if cfg.rclone.mode == 0 && cfg.rclone.pass.is_empty() {
        cfg.rclone.pass = generate_password();
        let _ = config::save(&cfg);
    }
    // 内嵌 rclone 优先：如果内嵌可用且用户未手动指定路径，则使用内嵌版本
    {
        let embedded = extract_embedded_rclone();
        if embedded.is_some() && !cfg.rclone.exe_custom {
            cfg.rclone.exe = embedded.clone().unwrap();
            let _ = config::save(&cfg);
        } else if cfg.rclone.exe.is_empty() {
            if let Ok(exe) = discover_exe(embedded.as_deref()) {
                cfg.rclone.exe = exe.display().to_string();
                let _ = config::save(&cfg);
            }
        }
    }

    // Slint UI
    let ui = MainWindow::new().expect("UI 初始化失败");
    apply_theme(&ui, cfg.theme_mode);
    ui.global::<SettingsG>().set_mode_idx(cfg.rclone.mode as i32);
    ui.global::<SettingsG>().set_exe(cfg.rclone.exe.clone().into());
    ui.global::<SettingsG>().set_rc_port(cfg.rclone.port.to_string().into());
    ui.global::<SettingsG>().set_rc_user(cfg.rclone.user.clone().into());
    ui.global::<SettingsG>().set_rc_pass(cfg.rclone.pass.clone().into());
    ui.global::<SettingsG>().set_rc_url(cfg.rclone.url.clone().into());
    ui.global::<SettingsG>().set_theme_idx(cfg.theme_mode as i32);
    ui.global::<ThemeG>().set_accent_idx(cfg.accent_idx as i32);
    ui.global::<SettingsG>().set_accent_idx(cfg.accent_idx as i32);
    ui.global::<SettingsG>().set_autostart(get_autostart());
    ui.global::<SettingsG>().set_silent(cfg.silent_start);
    ui.global::<SettingsG>().set_http_enabled(cfg.http.enabled);
    ui.global::<SettingsG>().set_http_port(cfg.http.port.to_string().into());
    ui.global::<SettingsG>()
        .set_cfg_path(config::config_path().display().to_string().into());
    ui.global::<MountsG>().set_winfsp_ok(winfsp_installed().0);
    ui.global::<MountsG>().set_winfsp_msg(winfsp_installed().1.into());
    // 远程路径历史
    set_history(&ui.as_weak(), &cfg.path_history);

    // 内置插件释放
    embed_plugins();

    // 插件宿主
    let host = Arc::new(plugin_host::PluginHost::new().expect("wasm 引擎初始化失败"));
    let store = Arc::new(kv::KvStore::new(config::data_dir().join("plugins")));

    // 命令通道
    let (tx, mut rx) = mpsc::unbounded_channel::<Cmd>();

    // 后台 runtime
    let cfg_arc = Arc::new(std::sync::Mutex::new(cfg.clone()));
    let ctx = Arc::new(Ctx {
        mgr: Arc::new(RcloneManager::new(config::data_dir().join("rcd.log"))),
        host,
        store,
        exe: PathBuf::from(&cfg.rclone.exe),
        cfg: cfg_arc.clone(),
        http_restart: Arc::new(tokio::sync::Notify::new()),
        remotes_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
        providers_cache: Arc::new(std::sync::Mutex::new(None)),
        mounts_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
        pending_mounts: Arc::new(std::sync::Mutex::new(Vec::new())),
        serves_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
        last_bwlimit: Arc::new(std::sync::Mutex::new(String::new())),
    });

    {
        let ctx = ctx.clone();
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime 启动失败");
            rt.block_on(backend_main(ctx, ui_weak, &mut rx));
        });
    }

    let search_debounce = SearchDebounce::default();
    wire_callbacks(&ui, ctx, tx.clone(), search_debounce.clone());

    // 窗口关闭 → 优雅退出（卸载挂载 / 停止服务 / 退出 rclone 进程）
    {
        let tx_c = tx.clone();
        ui.window().on_close_requested(move || {
            let _ = tx_c.send(Cmd::Quit);
            slint::CloseRequestResponse::HideWindow
        });
    }

    // busy 状态帧驱动（Spinner 动画）+ 搜索防抖派发
    {
        let ui_busy = ui.as_weak();
        let tx_db = tx.clone();
        let db = search_debounce.clone();
        let busy_timer = Box::leak(Box::new(slint::Timer::default()));
        busy_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
                if let Some(u) = ui_busy.upgrade() {
                    let busy = u.global::<MountsG>().get_busy()
                        || u.global::<ServesG>().get_busy()
                        || u.global::<RemotesG>().get_loading()
                        || u.global::<FilesG>().get_loading();
                    if busy {
                        let f = u.global::<BusyG>().get_frame();
                        u.global::<BusyG>().set_frame((f + 1) % 10);
                    }
                }
                // 搜索防抖：闲置 300ms 后触发筛选
                let wait = std::time::Duration::from_millis(300);
                for scope in 0..5 {
                    if db.due(scope, wait) {
                        let _ = tx_db.send(Cmd::SearchApply { scope: scope as i32 });
                    }
                }
            },
        );
    }

    // 托盘
    spawn_tray(ui.as_weak(), tx.clone());

    if !silent_flag && !cfg.silent_start {
        ui.show().expect("窗口显示失败");
    }
    slint::run_event_loop_until_quit().expect("事件循环启动失败");
}

/// 内置 wasm 插件释放到 data/plugins（不存在时）
fn embed_plugins() {
    const WASM: &[u8] = include_bytes!(concat!(env!("IMAGEHOST_WASM")));
    let dir = config::data_dir().join("plugins");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("imagehost.wasm");
    if !p.exists() {
        let _ = std::fs::write(&p, WASM);
    }
}

// ---------------- 单实例 ----------------

const IPC_PORT: u16 = 52799;

fn acquire_single_instance() -> bool {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::HSTRING;
    unsafe {
        let _ = CreateMutexW(None, false, &HSTRING::from("rclone-gui-singleton"));
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

fn notify_show() {
    if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", IPC_PORT)) {
        use std::io::Write as _;
        let _ = s.write_all(b"show");
    }
}

// ---------------- 后台主循环 ----------------

async fn backend_main(ctx: Arc<Ctx>, ui: Weak<MainWindow>, rx: &mut mpsc::UnboundedReceiver<Cmd>) {
    // 1) 启动 rclone（托管或连接）
    {
        let cfg = ctx.cfg.lock().unwrap().clone();
        let res = if cfg.rclone.mode == 0 {
            ctx.mgr
                .start_managed(
                    Path::new(&cfg.rclone.exe),
                    cfg.rclone.port,
                    &cfg.rclone.user,
                    &cfg.rclone.pass,
                )
                .await
                .map(|_| ())
        } else {
            ctx.mgr
                .attach(
                    &cfg.rclone.url,
                    (!cfg.rclone.user.is_empty()).then(|| cfg.rclone.user.clone()),
                    (!cfg.rclone.pass.is_empty()).then(|| cfg.rclone.pass.clone()),
                )
                .await
                .map(|_| ())
        };
        if let Err(e) = res {
            toast(&ui, 2, format!("rclone 连接失败: {e}"));
        }
        let ver_info = ctx
            .mgr
            .client
            .read()
            .await
            .version()
            .await
            .unwrap_or_default();
        let version = ver_info.version;
        let os_info = format!("{} / {}", ver_info.os, ver_info.arch);
        let running = version.starts_with('v');
        let managed = cfg.rclone.mode == 0;
        let url = if managed {
            format!("http://127.0.0.1:{}", cfg.rclone.port)
        } else {
            cfg.rclone.url.clone()
        };
        let _ = ui.upgrade_in_event_loop(
            move |ui| {
                let g = ui.global::<DashboardG>();
                g.set_version(version.into());
                g.set_os_info(os_info.into());
                g.set_running(running);
                g.set_managed(managed);
                g.set_url(url.into());
            },
        );
    }

    // 1.5) 先刷新界面列表（远程 / 挂载 / 服务），保证“已连接的远程”先显示出来
    let ready = {
        let client = ctx.mgr.client.read().await.clone();
        client
            .version()
            .await
            .map(|v| v.version.starts_with('v'))
            .unwrap_or(false)
    };
    if ready {
        let _ = Box::pin(handle_cmd(&ctx, &ui, Cmd::RemotesRefresh)).await;
        let _ = Box::pin(handle_cmd(&ctx, &ui, Cmd::MountsRefresh)).await;
        let _ = Box::pin(handle_cmd(&ctx, &ui, Cmd::ServesRefresh)).await;
    }

    // 1.6) 启动流程：并发校验每个远程预设（≤20 并发），成功后异步执行其挂载/服务
    //      不阻塞命令循环，文件浏览等操作在预热期间即可正常响应
    if ready {
        let ctx2 = ctx.clone();
        let ui2 = ui.clone();
        tokio::spawn(async move {
            startup_flow(&ctx2, &ui2).await;
        });
    }

    // 2) 内嵌 HTTP
    let http_cfg = ctx.cfg.lock().unwrap().http.clone();
    if http_cfg.enabled {
        let port = http_cfg.port;
        spawn_http(&ctx, port, ui.clone()).await;
        let _ = ui.upgrade_in_event_loop(
            move |ui| {
                ui.global::<DashboardG>()
                    .set_url(format!("http://127.0.0.1:{port}").into());
            },
        );
    }

    // 3) 单实例 IPC（收到 show → 显示窗口）
    {
        let ui = ui.clone();
        tokio::spawn(async move {
            if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", IPC_PORT)).await {
                loop {
                    if let Ok((mut sock, _)) = listener.accept().await {
                        use tokio::io::AsyncReadExt as _;
                        let mut buf = [0u8; 16];
                        if sock.read(&mut buf).await.is_ok() {
                            let _ = ui.upgrade_in_event_loop(
                                |ui| {
                                    ui.show().ok();
                                    ui.window().set_minimized(false);
                                },
                            );
                        }
                    }
                }
            }
        });
    }

    // 4) 轮询 + 命令循环
    let mut tick: u64 = 0;
    let mut last_dark: Option<bool> = None;
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(1000));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                tick += 1;
                poll_stats(&ctx, &ui).await;
                // 传输页：后台任务列表 + 全局限速（每 2s）
                if tick % 2 == 0 {
                    build_tasks(&ctx, &ui).await;
                    apply_global_limit(&ctx, &ui).await;
                }
                // 主题跟随系统（每 3s 检查）
                if tick % 3 == 0 {
                    let mode = ctx.cfg.lock().unwrap().theme_mode;
                    if mode == 0 {
                        let dark = !apps_use_light();
                        if last_dark != Some(dark) {
                            last_dark = Some(dark);
                            let _ = ui.upgrade_in_event_loop(move |ui| {
                                ui.global::<ThemeG>().set_dark(dark);
                            });
                        }
                    }
                }
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                if handle_cmd(&ctx, &ui, cmd).await {
                    break;
                }
            }
            _ = ctx.http_restart.notified() => {
                let (enabled, port) = {
                    let c = ctx.cfg.lock().unwrap();
                    (c.http.enabled, c.http.port)
                };
                if enabled {
                    spawn_http(&ctx, port, ui.clone()).await;
                }
            }
        }
    }
    let _ = ctx.mgr.stop().await;
}

/// 启动流程：对每个远程预设并发执行（最大 20 并发，避免预设过多时网络拥堵）：
/// 连接(about) → 列根目录（校验+预热连接）→ 刷新文件列表 → 执行该远程的挂载/服务预设
/// 任一步骤失败的预设立即终止自身流程，不影响其他预设；全程不阻塞命令循环。
async fn startup_flow(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let client = ctx.mgr.client.read().await.clone();
    let remotes = client.list_remotes().await.unwrap_or_default();
    if remotes.is_empty() {
        return;
    }
    let (mounts, serves) = {
        let c = ctx.cfg.lock().unwrap();
        (c.auto_mounts.clone(), c.auto_serves.clone())
    };
    // 侧边栏"远程"图标 spinner（预热期间）
    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<NavG>().set_starting(true));

    // 已运行服务快照，防重复部署（如上一实例异常退出后的残留服务占用端口）
    let mut running: Vec<(String, String)> = Vec::new();
    if let Ok(v) = client.serve_list().await {
        if let Some(items) = v.get("list").and_then(|x| x.as_array()) {
            for it in items {
                let params = it.get("params").cloned().unwrap_or(json!({}));
                let getp = |keys: &[&str]| -> String {
                    for k in keys {
                        if let Some(s) = params.get(*k).and_then(|x| x.as_str()) {
                            return s.to_string();
                        }
                        if let Some(s) = it.get(*k).and_then(|x| x.as_str()) {
                            return s.to_string();
                        }
                    }
                    String::new()
                };
                running.push((
                    getp(&["fs", "Fs"]),
                    getp(&["addr", "Addr", "listen", "Listen"]),
                ));
            }
        }
    }

    let sem = Arc::new(tokio::sync::Semaphore::new(20));
    let mut handles = Vec::new();
    for name in &remotes {
        let ctx2 = ctx.clone();
        let ui2 = ui.clone();
        let client = client.clone();
        let sem = sem.clone();
        let running = running.clone();
        let name = name.clone();
        // 只处理属于该远程的挂载/服务预设（fs 形如 "name:"）
        let mounts: Vec<_> = mounts
            .iter()
            .filter(|m| m.fs.trim_end_matches(':') == name)
            .cloned()
            .collect();
        let serves: Vec<_> = serves
            .iter()
            .filter(|s| s.fs.trim_end_matches(':') == name)
            .cloned()
            .collect();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await;
            // 1) 连接校验（失败 → 该预设流程终止）
            if client.about(&format!("{name}:")).await.is_err() {
                toast(&ui2, 2, format!("预设 {name} 连接失败，已跳过其挂载/服务"));
                return;
            }
            // 2) 列根目录校验（同时预热远程连接，避免文件页首次访问被连接建立阻塞）
            if client.list(&format!("{name}:"), "", None).await.is_err() {
                toast(&ui2, 2, format!("预设 {name} 获取文件列表失败，已跳过其挂载/服务"));
                return;
            }
            // 3) 文件页当前显示的正是该远程 → 刷新列表
            let (cur_remote, _) = ui_files_pos(&ui2).await;
            if cur_remote == name {
                let _ = Box::pin(handle_cmd(&ctx2, &ui2, Cmd::FilesRefresh { seq: -1 })).await;
            }
            // 4) 该远程的挂载预设（并发执行，条目带 spinner）
            let mut mh = Vec::new();
            for m in mounts {
                let ctx3 = ctx2.clone();
                let ui3 = ui2.clone();
                let client3 = client.clone();
                mh.push(tokio::spawn(async move {
                    ctx3.pending_mounts
                        .lock()
                        .unwrap()
                        .push((m.fs.clone(), m.point.clone()));
                    let _ = Box::pin(handle_cmd(&ctx3, &ui3, Cmd::MountsRefresh)).await;
                    let res = client3
                        .mount(
                            &m.fs,
                            &m.point,
                            json!({ "vfs_cache_mode": m.cache, "read_only": m.ro }),
                        )
                        .await;
                    ctx3.pending_mounts
                        .lock()
                        .unwrap()
                        .retain(|(_, p)| p != &m.point);
                    match res {
                        Ok(actual) => toast(&ui3, 1, format!("已自动挂载 {} → {actual}", m.fs)),
                        Err(e) => toast(&ui3, 2, format!("自动挂载 {} 失败: {e}", m.fs)),
                    }
                    let _ = Box::pin(handle_cmd(&ctx3, &ui3, Cmd::MountsRefresh)).await;
                }));
            }
            // 5) 该远程的服务预设（先查重，避免重复部署报端口占用）
            for s in serves {
                if running.iter().any(|(f, a)| *f == s.fs && *a == s.addr) {
                    continue;
                }
                match client
                    .serve_start(&s.stype, &s.fs, &s.addr, &s.user, &s.pass)
                    .await
                {
                    Ok(id) => toast(&ui2, 1, format!("已自动启动服务 {id}")),
                    Err(e) => toast(&ui2, 2, format!("自动启动服务失败: {e}")),
                }
            }
            for h in mh {
                let _ = h.await;
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<NavG>().set_starting(false));
}

async fn spawn_http(ctx: &Arc<Ctx>, port: u16, ui: Weak<MainWindow>) {
    let caps = Arc::new(kv::RealCaps {
        mgr: ctx.mgr.clone(),
        store: ctx.store.clone(),
    });
    let st = http::HttpState {
        host: ctx.host.clone(),
        caps,
        mgr: ctx.mgr.clone(),
        exe: ctx.exe.clone(),
        cfg: ctx.cfg.clone(),
    };
    tokio::spawn(async move {
        if let Err(e) = http::serve(st, port).await {
            let msg = format!("HTTP 服务启动失败（端口 {port}）: {e}");
            let _ = ui.upgrade_in_event_loop(
                move |ui| toast_ui(&ui, 2, msg),
            );
        }
    });
}

async fn poll_stats(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let client = ctx.mgr.client.read().await.clone();
    let Ok(stats) = client.core_stats().await else { return };
    let speed = stats.speed;
    let bytes = stats.bytes;
    let transferring = stats.transferring;
    let mounts = client.mounts().await.map(|m| m.len()).unwrap_or(0);
    let _ = ui.upgrade_in_event_loop(
        move |ui| {
            let g = ui.global::<DashboardG>();
            g.set_speed(format!("{}/s", fmt_bytes(speed)).into());
            g.set_total_bytes(fmt_bytes(bytes).into());
            g.set_transfers(transferring.len() as i32);
            g.set_mounts(mounts as i32);
            let rows: Vec<XferRow> = transferring
                .iter()
                .map(|t| XferRow {
                    name: t.name.clone().into(),
                    size: fmt_bytes(t.bytes).into(),
                    speed: format!("{}/s", fmt_bytes(t.speed)).into(),
                    percent: t.percentage.clamp(0.0, 100.0) as i32,
                })
                .collect();
            let model = ModelRc::from(Rc::new(VecModel::from(rows)));
            g.set_xfers(model);
        },
    );
}

/// 返回 true 表示退出
async fn handle_cmd(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, cmd: Cmd) -> bool {
    match cmd {
        Cmd::Quit => {
            // 托管模式：先显式清理全部挂载与服务，再优雅退出 rclone，确保无残留
            let managed = ctx.cfg.lock().unwrap().rclone.mode == 0;
            if managed {
                let client = ctx.mgr.client.read().await.clone();
                if let Ok(ms) = client.mounts().await {
                    for m in ms {
                        let _ = client.unmount(&m.mount_point).await;
                    }
                }
                if let Ok(v) = client.serve_list().await {
                    if let Some(items) = v.get("list").and_then(|x| x.as_array()) {
                        for it in items {
                            if let Some(id) = it.get("id").and_then(|x| {
                                x.as_str().map(|s| s.to_string())
                                    .or_else(|| x.as_i64().map(|n| n.to_string()))
                            }) {
                                let _ = client.serve_stop(&id).await;
                            }
                        }
                    }
                }
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client.core_quit(),
                )
                .await;
                // 最多再等 4s 让进程自行退出
                for _ in 0..20 {
                    if !ctx.mgr.is_managed_running().await {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
            // 兜底杀进程（attach 模式无子进程，无操作）
            ctx.mgr.stop().await;
            let _ = slint::quit_event_loop();
            true
        }
        Cmd::RefreshDashboard => {
            let client = ctx.mgr.client.read().await.clone();
            let ver_info = client.version().await.unwrap_or_default();
            let version = ver_info.version;
            let os_info = format!("{} / {}", ver_info.os, ver_info.arch);
            let remotes = client.list_remotes().await.map(|r| r.len()).unwrap_or(0);
            let errs = client.core_stats().await.map(|s| s.errors).unwrap_or(0);
            let managed = ctx.cfg.lock().unwrap().rclone.mode == 0;
            let url = {
                let c = ctx.cfg.lock().unwrap();
                if c.rclone.mode == 0 {
                    format!("http://127.0.0.1:{}", c.rclone.port)
                } else {
                    c.rclone.url.clone()
                }
            };
            let running = version.starts_with('v');
            let _ = ui.upgrade_in_event_loop(
                move |ui| {
                    let g = ui.global::<DashboardG>();
                    g.set_version(version.into());
                    g.set_os_info(os_info.into());
                    g.set_remotes(remotes as i32);
                    g.set_errors(errs as i32);
                    g.set_running(running);
                    g.set_managed(managed);
                    g.set_url(url.into());
                },
            );
            false
        }
        Cmd::FilesInit => {
            let client = ctx.mgr.client.read().await.clone();
            let remotes = client.list_remotes().await.unwrap_or_default();
            let _ = ui.upgrade_in_event_loop(
                move |ui| {
                    ui.global::<FilesG>().set_remotes(ModelRc::from(Rc::new(VecModel::from(
                        remotes
                            .iter()
                            .map(|r| r.clone().into())
                            .collect::<Vec<slint::SharedString>>(),
                    ))));
                    ui.global::<FilesG>().set_remote_idx(0);
                },
            );
            // 给事件循环留时间完成 remotes 设置，避免 FilesRefresh 读到空 remote
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
            false
        }
        Cmd::FilesRefresh { seq } => {
            let (remote, path) = ui_files_pos(ui).await;
            // remotes 尚未加载完成时静默跳过，避免 fs=":" 报错
            if remote.is_empty() {
                return false;
            }
            let client = ctx.mgr.client.read().await.clone();
            let fs = format!("{remote}:");
            let res = client.list(&fs, &path, None).await;
            // 请求期间用户可能已发起更新操作（seq 变化），丢弃过期结果
            let cur_seq = read_ui(ui, |u| u.global::<FilesG>().get_seq()).await.unwrap_or(-2);
            if seq >= 0 && cur_seq != seq {
                let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
                return false;
            }
            let _ = ui.upgrade_in_event_loop(
                move |ui| {
                    let g = ui.global::<FilesG>();
                    match res {
                        Ok(entries) => {
                            let mut rows: Vec<FileRow> = entries
                                .iter()
                                .map(|e| FileRow {
                                    name: e.name.clone().into(),
                                    is_dir: e.is_dir,
                                    size: fmt_bytes(e.size as f64).into(),
                                    mtime: fmt_mtime(&e.mod_time).into(),
                                })
                                .collect();
                            rows.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
                            g.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
                        }
                        Err(e) => {
                            g.set_entries(ModelRc::from(Rc::new(VecModel::from(Vec::<FileRow>::new()))));
                            toast_ui(&ui, 2, format!("列目录失败: {e}"));
                        }
                    }
                    g.set_sel(-1);
                    g.set_loading(false);
                },
            );
            false
        }
        Cmd::FilesEnter { name, seq } => {
            let (remote, path) = ui_files_pos(ui).await;
            if remote.is_empty() {
                return false;
            }
            let target = join_path(&path, &name);
            let client = ctx.mgr.client.read().await.clone();
            let fs = format!("{remote}:");
            let res = client.list(&fs, &target, None).await;
            // 请求期间用户可能已切换远程或发起新操作，丢弃过期结果
            let cur_seq = read_ui(ui, |u| u.global::<FilesG>().get_seq()).await.unwrap_or(-2);
            if seq >= 0 && cur_seq != seq {
                let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
                return false;
            }
            let _ = ui.upgrade_in_event_loop(
                move |ui| {
                    let g = ui.global::<FilesG>();
                    g.set_loading(false);
                    match res {
                        Ok(entries) => {
                            let mut rows: Vec<FileRow> = entries
                                .iter()
                                .map(|e| FileRow {
                                    name: e.name.clone().into(),
                                    is_dir: e.is_dir,
                                    size: fmt_bytes(e.size as f64).into(),
                                    mtime: fmt_mtime(&e.mod_time).into(),
                                })
                                .collect();
                            rows.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
                            // 成功进入后才更新路径，避免网络慢时拼出不存在的路径
                            g.set_path(target.clone().into());
                            g.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
                        }
                        Err(e) => {
                            toast_ui(&ui, 2, format!("无法进入 {name}: {e}"));
                        }
                    }
                    g.set_sel(-1);
                },
            );
            false
        }
        Cmd::FilesUp { seq } => {
            let (remote, path) = ui_files_pos(ui).await;
            if remote.is_empty() {
                return false;
            }
            let parent = match path.rfind('/') {
                Some(idx) => path[..idx].to_string(),
                None => String::new(),
            };
            let client = ctx.mgr.client.read().await.clone();
            let fs = format!("{remote}:");
            let res = client.list(&fs, &parent, None).await;
            let cur_seq = read_ui(ui, |u| u.global::<FilesG>().get_seq()).await.unwrap_or(-2);
            if seq >= 0 && cur_seq != seq {
                let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
                return false;
            }
            let _ = ui.upgrade_in_event_loop(
                move |ui| {
                    let g = ui.global::<FilesG>();
                    g.set_loading(false);
                    match res {
                        Ok(entries) => {
                            let mut rows: Vec<FileRow> = entries
                                .iter()
                                .map(|e| FileRow {
                                    name: e.name.clone().into(),
                                    is_dir: e.is_dir,
                                    size: fmt_bytes(e.size as f64).into(),
                                    mtime: fmt_mtime(&e.mod_time).into(),
                                })
                                .collect();
                            rows.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
                            g.set_path(parent.clone().into());
                            g.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
                        }
                        Err(e) => {
                            toast_ui(&ui, 2, format!("返回上级失败: {e}"));
                        }
                    }
                    g.set_sel(-1);
                },
            );
            false
        }
        Cmd::FilesDownload { name, local } => {
            let (remote, path) = ui_files_pos(ui).await;
            let client = ctx.mgr.client.read().await.clone();
            let fs = local_fs(local.parent().unwrap_or(Path::new(".")));
            let res = client
                .copyfile(
                    &format!("{remote}:"),
                    &join_path(&path, &name),
                    &fs,
                    &name,
                )
                .await;
            match res {
                Ok(_) => toast(ui, 1, format!("已下载 {name}")),
                Err(e) => toast(ui, 2, format!("下载失败: {e}")),
            }
            false
        }
        Cmd::FilesMkdir => {
            let (remote, path) = ui_files_pos(ui).await;
            let client = ctx.mgr.client.read().await.clone();
            let fs = format!("{remote}:");
            let res = client.mkdir(&fs, &join_path(&path, "新建文件夹")).await;
            match res {
                Ok(_) => {
                    let _ = ui.upgrade_in_event_loop(
                        |ui| {
                            ui.global::<FilesG>().set_rename_name("新建文件夹".into());
                        },
                    );
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
                    toast(ui, 1, "文件夹已创建（选中后可重命名）");
                }
                Err(e) => toast(ui, 2, format!("创建失败: {e}")),
            }
            false
        }
        Cmd::FilesDel { name, is_dir } => {
            let (remote, path) = ui_files_pos(ui).await;
            let client = ctx.mgr.client.read().await.clone();
            let fs = format!("{remote}:");
            let full = join_path(&path, &name);
            let res = if is_dir {
                client.purge(&fs, &full).await
            } else {
                client.deletefile(&fs, &full).await
            };
            match res {
                Ok(_) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
                    toast(ui, 1, format!("已删除 {name}"));
                }
                Err(e) => toast(ui, 2, format!("删除失败: {e}")),
            }
            false
        }
        Cmd::FilesRename { from, to } => {
            if to.trim().is_empty() || to == from {
                return false;
            }
            let (remote, path) = ui_files_pos(ui).await;
            let client = ctx.mgr.client.read().await.clone();
            let fs = format!("{remote}:");
            let res = client
                .movefile(&fs, &join_path(&path, &from), &fs, &join_path(&path, &to))
                .await;
            match res {
                Ok(_) => {
                    let _ = ui.upgrade_in_event_loop(
                        |ui| {
                            ui.global::<FilesG>().set_sel(-1);
                        },
                    );
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
                    toast(ui, 1, "重命名完成");
                }
                Err(e) => toast(ui, 2, format!("重命名失败: {e}")),
            }
            false
        }
        Cmd::FilesCopy { name } => {
            let (remote, path) = ui_files_pos(ui).await;
            let client = ctx.mgr.client.read().await.clone();
            let fs = format!("{remote}:");
            let (base, ext) = split_name(&name);
            let entries = client.list(&fs, &path, None).await.unwrap_or_default();
            let mut cand = format!("{base}-1{ext}");
            for i in 2..100 {
                if entries.iter().all(|e| e.name != cand) {
                    break;
                }
                cand = format!("{base}-{i}{ext}");
            }
            let res = client
                .copyfile(&fs, &join_path(&path, &name), &fs, &join_path(&path, &cand))
                .await;
            match res {
                Ok(_) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
                    toast(ui, 1, format!("已复制为 {cand}"));
                }
                Err(e) => toast(ui, 2, format!("复制失败: {e}")),
            }
            false
        }
        Cmd::FilesCopyLink { name } => {
            let (remote, path) = ui_files_pos(ui).await;
            let cfg_raw = ctx.store.get_all("imagehost");
            let cfg = cfg_raw.get("config").cloned().unwrap_or(json!({}));
            let roots = cfg
                .get("roots")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();
            let root_id = roots
                .iter()
                .find(|r| {
                    r.get("remote")
                        .and_then(|x| x.as_str())
                        .map(|x| x.trim_end_matches(':'))
                        == Some(remote.as_str())
                })
                .and_then(|r| r.get("id").and_then(|x| x.as_str()).map(|s| s.to_string()));
            let http_port = ctx.cfg.lock().unwrap().http.port;
            match root_id {
                Some(rid) => {
                    let stored = join_path(&path, &name);
                    let url = format!("http://127.0.0.1:{http_port}/i/{rid}/{stored}");
                    clipboard_copy(&url);
                    toast(ui, 1, format!("直链已复制: {url}"));
                }
                None => toast(ui, 2, "该远程未配置为图床存储根"),
            }
            false
        }
        Cmd::MountsRefresh => {
            let client = ctx.mgr.client.read().await.clone();
            let mounts = client.mounts().await.unwrap_or_default();
            let mut rows: Vec<MountRow> = mounts
                .iter()
                .map(|m| MountRow {
                    fs: m.fs.clone().into(),
                    mount_point: m.mount_point.clone().into(),
                    pending: false,
                })
                .collect();
            // 合并“正在挂载中”的条目；已被真实列表收录的则自动摘掉
            {
                let mut pending = ctx.pending_mounts.lock().unwrap();
                pending.retain(|(_, pt)| !rows.iter().any(|r| r.mount_point.eq_ignore_ascii_case(pt)));
                for (fs, pt) in pending.iter() {
                    if !rows.iter().any(|r| r.mount_point.eq_ignore_ascii_case(pt)) {
                        rows.push(MountRow {
                            fs: fs.clone().into(),
                            mount_point: pt.clone().into(),
                            pending: true,
                        });
                    }
                }
            }
            *ctx.mounts_cache.lock().unwrap() = rows.clone();
            apply_mounts_filter(ui, &rows).await;
            false
        }
        Cmd::MountAdd => {
            let (fs_disp, point_raw, ro, cache, auto) = read_ui(ui, |ui| {
                let m = ui.global::<MountsG>();
                let fs = m.get_new_fs().to_string();
                let point = m.get_new_point().to_string();
                let ro = m.get_ro();
                let cache = m
                    .get_caches()
                    .row_data(m.get_cache_idx().max(0) as usize)
                    .unwrap_or("minimal".into())
                    .to_string();
                let auto = m.get_auto_mount();
                (fs, point, ro, cache, auto)
            })
            .await
            .unwrap_or_default();
            let fs = display_to_fs(&fs_disp);
            if fs.is_empty() {
                toast(ui, 2, "请选择或输入远程路径（如 teldrive:）");
                return false;
            }
            let point = normalize_mount_point(&point_raw);
            // 盘符 / 挂载点重复检测（含正在挂载中的条目）
            if point != "*" {
                let dup = ctx
                    .mounts_cache
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.mount_point.eq_ignore_ascii_case(&point));
                if dup {
                    toast(ui, 2, format!("挂载点 {point} 已被占用，请修改盘符"));
                    return false;
                }
            }
            let client = ctx.mgr.client.read().await.clone();
            let opts = json!({
                "vfs_cache_mode": cache,
                "read_only": ro,
            });
            // 立即在“活动挂载”里插入一条 pending（spinner + 高亮）
            ctx.pending_mounts.lock().unwrap().push((fs.clone(), point.clone()));
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<MountsG>().set_busy(true));
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
            let res = client.mount(&fs, &point, opts).await;
            // 无论成败都先摘掉 pending
            ctx.pending_mounts.lock().unwrap().retain(|(_, p)| p != &point);
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<MountsG>().set_busy(false));
            match res {
                Ok(actual) => {
                    if auto {
                        // 保存自动挂载（按挂载点去重）
                        let item = config::AutoMount {
                            fs: fs.clone(),
                            point: actual.clone(),
                            ro,
                            cache: cache.clone(),
                        };
                        let mut save_err = None;
                        {
                            let mut cfg = ctx.cfg.lock().unwrap();
                            cfg.auto_mounts.retain(|m| m.point != item.point);
                            cfg.auto_mounts.push(item);
                            if let Err(e) = config::save(&cfg) {
                                save_err = Some(e.to_string());
                            }
                        }
                        if let Some(e) = save_err {
                            toast(ui, 2, format!("自动挂载配置保存失败: {e}"));
                        }
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    push_history(ctx, ui, &fs_disp).await;
                    toast(ui, 1, format!("挂载成功：{actual}"));
                }
                Err(e) => {
                    // 失败：刷新列表（pending 已摘除，条目自动消失）
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    toast(ui, 2, format!("挂载失败：{e}"));
                }
            }
            false
        }
        Cmd::MountRemove { point } => {
            let client = ctx.mgr.client.read().await.clone();
            let _ = ui.upgrade_in_event_loop(
                |ui| {
                    ui.global::<MountsG>().set_busy(true);
                },
            );
            let res = client.unmount(&point).await;
            let _ = ui.upgrade_in_event_loop(
                |ui| {
                    ui.global::<MountsG>().set_busy(false);
                },
            );
            match res {
                Ok(_) => {
                    // 同步移除自动挂载项
                    {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        if cfg.auto_mounts.iter().any(|m| m.point == point) {
                            cfg.auto_mounts.retain(|m| m.point != point);
                            let _ = config::save(&cfg);
                        }
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    toast(ui, 1, format!("已卸载 {point}"));
                }
                Err(e) => toast(ui, 2, format!("卸载失败: {e}")),
            }
            false
        }
        Cmd::HistPick { scope, path } => {
            let _ = ui.upgrade_in_event_loop(move |ui| {
                if scope == 0 {
                    ui.global::<MountsG>().set_new_fs(path.into());
                } else {
                    ui.global::<ServesG>().set_new_fs(path.into());
                }
            });
            false
        }
        Cmd::BrowseOpen { scope } => {
            let cur = read_ui(ui, move |ui| {
                if scope == 0 {
                    ui.global::<MountsG>().get_new_fs().to_string()
                } else {
                    ui.global::<ServesG>().get_new_fs().to_string()
                }
            })
            .await
            .unwrap_or_default();
            let start = if cur.trim().is_empty() { "root://".to_string() } else { cur };
            let (items, disp) = list_display(ctx, &start, false).await;
            let rows: Vec<PathItem> = items
                .into_iter()
                .map(|(n, d)| PathItem { name: n.into(), is_dir: d })
                .collect();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<BrowseG>();
                g.set_scope(scope);
                g.set_path(disp.into());
                g.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
                g.set_sel(-1);
                g.set_show(true);
            });
            false
        }
        Cmd::BrowseGoto { name } => {
            let path = read_ui(ui, |ui| ui.global::<BrowseG>().get_path().to_string())
                .await
                .unwrap_or_default();
            let base = path.trim().trim_end_matches('/');
            let target = if name == ".." {
                match display_parent(&path) {
                    Some(p) => p,
                    None => return false,
                }
            } else if base == "root:" || base == "root://" || base == "root" || base.is_empty() {
                format!("root://{name}")
            } else {
                format!("{base}/{name}")
            };
            let (items, disp) = list_display(ctx, &target, false).await;
            let rows: Vec<PathItem> = items
                .into_iter()
                .map(|(n, d)| PathItem { name: n.into(), is_dir: d })
                .collect();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<BrowseG>();
                g.set_path(disp.into());
                g.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
                g.set_sel(-1);
            });
            false
        }
        Cmd::BrowseOk => {
            let (scope, path, sel, entries) = read_ui(ui, |ui| {
                let g = ui.global::<BrowseG>();
                let mut entries = vec![];
                let m = g.get_entries();
                for i in 0..m.row_count() {
                    if let Some(e) = m.row_data(i) {
                        entries.push((e.name.to_string(), e.is_dir));
                    }
                }
                (g.get_scope(), g.get_path().to_string(), g.get_sel(), entries)
            })
            .await
            .unwrap_or_default();
            let base = path.trim().trim_end_matches('/');
            let is_root = base == "root:" || base == "root://" || base == "root" || base.is_empty();
            let picked = if sel >= 0 { entries.get(sel as usize) } else { None };
            let final_disp = match picked {
                Some((n, _)) => {
                    if is_root {
                        format!("root://{n}")
                    } else {
                        format!("{base}/{n}")
                    }
                }
                None => {
                    if is_root {
                        "root://".to_string()
                    } else {
                        base.to_string()
                    }
                }
            };
            let _ = ui.upgrade_in_event_loop(move |ui| {
                if scope == 0 {
                    ui.global::<MountsG>().set_new_fs(final_disp.clone().into());
                } else {
                    ui.global::<ServesG>().set_new_fs(final_disp.clone().into());
                }
                ui.global::<BrowseG>().set_show(false);
            });
            false
        }
        Cmd::RemotesRefresh => {
            let client = ctx.mgr.client.read().await.clone();
            let names = client.list_remotes().await.unwrap_or_default();
            let dump = client.config_dump().await.unwrap_or(json!({}));
            let mut full: Vec<(String, String, String)> = Vec::with_capacity(names.len());
            {
                let cache = ctx.remotes_cache.lock().unwrap();
                for n in &names {
                    let ty = dump
                        .get(n)
                        .and_then(|e| e.get("type"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let usage = cache
                        .iter()
                        .find(|(cn, _, _)| cn == n)
                        .map(|(_, _, u)| u.clone())
                        .unwrap_or_else(|| "…".into());
                    full.push((n.clone(), ty, usage));
                }
            }
            *ctx.remotes_cache.lock().unwrap() = full.clone();
            apply_remotes_filter(ui, &full).await;
            // 异步加载用量
            for n in names {
                let client = client.clone();
                let ctx2 = ctx.clone();
                let ui2 = ui.clone();
                tokio::spawn(async move {
                    let usage = match client.about(&format!("{n}:")).await {
                        Ok(v) => {
                            let total = v.get("total").and_then(|x| x.as_f64());
                            let used = v.get("used").and_then(|x| x.as_f64());
                            match (total, used) {
                                (Some(t), Some(u)) => format!("{} / {}", fmt_bytes(u), fmt_bytes(t)),
                                (None, Some(u)) => format!("已用 {}", fmt_bytes(u)),
                                _ => "—".into(),
                            }
                        }
                        Err(_) => "—".into(),
                    };
                    {
                        let mut cache = ctx2.remotes_cache.lock().unwrap();
                        if let Some(e) = cache.iter_mut().find(|(cn, _, _)| cn == &n) {
                            e.2 = usage.clone();
                        }
                    }
                    let _ = ui2.upgrade_in_event_loop(move |ui| {
                        let g = ui.global::<RemotesG>();
                        let rows = g.get_rows();
                        for i in 0..rows.row_count() {
                            if let Some(mut r) = rows.row_data(i) {
                                if r.name.as_str() == n.as_str() {
                                    r.usage = usage.clone().into();
                                    rows.set_row_data(i, r);
                                }
                            }
                        }
                    });
                    let full2 = ctx2.remotes_cache.lock().unwrap().clone();
                    apply_remotes_filter(&ui2, &full2).await;
                });
            }
            false
        }
        Cmd::SearchApply { scope } => {
            match scope {
                0 => {
                    let full = ctx.remotes_cache.lock().unwrap().clone();
                    apply_remotes_filter(ui, &full).await;
                }
                1 => {
                    let full = ctx.mounts_cache.lock().unwrap().clone();
                    apply_mounts_filter(ui, &full).await;
                }
                2 => {
                    let full = ctx.serves_cache.lock().unwrap().clone();
                    apply_serves_filter(ui, &full).await;
                }
                3 => {
                    build_tasks(ctx, ui).await;
                }
                _ => {}
            }
            false
        }
        Cmd::RemoteAdd => {
            // 先显示编辑器，再异步加载 providers
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<RemotesG>();
                g.set_edit_is_new(true);
                g.set_edit_name("".into());
                g.set_edit_params("".into());
                g.set_type_idx(0);
                g.set_show_advanced(false);
                g.set_show_raw(false);
                g.set_show_editor(true);
            });
            // 异步获取 providers 并更新字段
            let ui2 = ui.clone();
            let ctx2 = ctx.clone();
            tokio::spawn(async move {
                let providers = get_providers(&ctx2).await;
                // 更新类型列表
                let list = providers.get("providers").and_then(|p| p.as_array()).cloned().unwrap_or_default();
                let mut names: Vec<String> = list.iter()
                    .filter_map(|p| p.get("Name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                    .collect();
                names.sort();
                let first_type = names.first().cloned().unwrap_or_default();
                let _ = ui2.upgrade_in_event_loop(move |ui| {
                    let g = ui.global::<RemotesG>();
                    g.set_types(ModelRc::from(Rc::new(VecModel::from(
                        names.iter().map(|s| s.as_str().into()).collect::<Vec<slint::SharedString>>()
                    ))));
                    g.set_type_idx(0);
                });
                update_editor_fields(&ui2, &providers, &first_type);
            });
            false
        }
        Cmd::RemoteTypeChanged => {
            let type_name = read_ui(ui, |ui| {
                let g = ui.global::<RemotesG>();
                let ti = g.get_type_idx().max(0) as usize;
                g.get_types().row_data(ti).unwrap_or_default().to_string()
            })
            .await
            .unwrap_or_default();
            let providers = get_providers(&ctx).await;
            update_editor_fields(ui, &providers, &type_name);
            false
        }
        Cmd::RemoteEditLoad { name } => {
            let client = ctx.mgr.client.read().await.clone();
            let dump = client.config_dump().await.unwrap_or(json!({}));
            let entry = dump.get(&name).cloned().unwrap_or(json!({}));
            let ty = entry
                .get("type")
                .and_then(|x| x.as_str())
                .unwrap_or("local")
                .to_string();
            let params_text = entry
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter(|(k, _)| *k != "type")
                        .map(|(k, v)| format!("{}={}", k, v.as_str().unwrap_or(&v.to_string())))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let existing_params = entry.as_object().cloned().unwrap_or_default();
            let providers = get_providers(&ctx).await;
            let (desc, mut fields) = build_fields_from_provider(&providers, &ty);
            fill_fields_with_existing(&mut fields, &existing_params);
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<RemotesG>();
                // 确保类型列表包含当前类型
                let mut types: Vec<slint::SharedString> =
                    (0..g.get_types().row_count()).filter_map(|i| g.get_types().row_data(i)).collect();
                let idx = types.iter().position(|t| t.as_str() == ty);
                if idx.is_none() {
                    types.push(ty.clone().into());
                }
                g.set_types(ModelRc::from(Rc::new(VecModel::from(types.clone()))));
                let new_idx = idx.unwrap_or(types.len() - 1) as i32;
                g.set_type_idx(new_idx);
                g.set_edit_is_new(false);
                g.set_edit_name(name.into());
                g.set_edit_params(params_text.into());
                g.set_provider_desc(desc.into());
                g.set_edit_fields(ModelRc::from(Rc::new(VecModel::from(fields))));
                g.set_show_advanced(false);
                g.set_show_raw(false);
                g.set_show_editor(true);
            });
            false
        }
        Cmd::RemoteSave => {
            let (name, type_str, params_text, is_new, show_raw) = read_ui(ui, |ui| {
                let g = ui.global::<RemotesG>();
                let ti = g.get_type_idx().max(0) as usize;
                let ty = g.get_types().row_data(ti).unwrap_or_default().to_string();
                (
                    g.get_edit_name().to_string(),
                    ty,
                    g.get_edit_params().to_string(),
                    g.get_edit_is_new(),
                    g.get_show_raw(),
                )
            })
            .await
            .unwrap_or_default();
            if name.trim().is_empty() || name.contains(':') || name.contains(' ') {
                toast(ui, 2, "名称不能为空，且不能包含空格或冒号");
                return false;
            }
            // 优先使用模板字段，原始模式或字段为空时用 key=value
            let params = if !show_raw {
                let fields = read_fields_from_ui(ui).await;
                if !fields.is_empty() {
                    fields_to_params(&fields)
                } else {
                    serde_json::Value::Object(parse_kv_lines(&params_text))
                }
            } else {
                serde_json::Value::Object(parse_kv_lines(&params_text))
            };
            let client = ctx.mgr.client.read().await.clone();
            let res = if is_new {
                client.config_create(&name, &type_str, params).await
            } else {
                client.config_update(&name, params).await
            };
            match res {
                Ok(()) => {
                    let _ = ui.upgrade_in_event_loop(|ui| {
                        ui.global::<RemotesG>().set_show_editor(false);
                    });
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::RemotesRefresh)).await;
                    toast(ui, 1, format!("远程存储 {} 已保存", name));
                }
                Err(e) => toast(ui, 2, format!("保存失败: {e}")),
            }
            false
        }
        Cmd::RemoteDelete { name } => {
            // 确认已在 UI 层（ConfirmPop）完成
            let client = ctx.mgr.client.read().await.clone();
            match client.config_delete(&name).await {
                Ok(()) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::RemotesRefresh)).await;
                    toast(ui, 1, format!("已删除 {name}"));
                }
                Err(e) => toast(ui, 2, format!("删除失败: {e}")),
            }
            false
        }
        Cmd::RemoteCancel => {
            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.global::<RemotesG>().set_show_editor(false);
            });
            false
        }
        Cmd::ServesRefresh => {
            let client = ctx.mgr.client.read().await.clone();
            let v = client.serve_list().await.unwrap_or(json!({}));
            let items = v
                .get("list")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            let rows: Vec<ServeRow> = items
                .iter()
                .filter(|it| {
                    // 只显示运行中的服务（已停止的不留在列表里）
                    it.get("running").and_then(|x| x.as_bool()).unwrap_or(true)
                })
                .map(|it| {
                    let id = it
                        .get("id")
                        .and_then(|x| {
                            x.as_str()
                                .map(|s| s.to_string())
                                .or_else(|| x.as_i64().map(|n| n.to_string()))
                        })
                        .unwrap_or_default();
                    let params = it.get("params").cloned().unwrap_or(json!({}));
                    let getp = |keys: &[&str]| -> String {
                        for k in keys {
                            if let Some(v) = params.get(*k).and_then(|x| x.as_str()) {
                                return v.to_string();
                            }
                            if let Some(v) = it.get(*k).and_then(|x| x.as_str()) {
                                return v.to_string();
                            }
                        }
                        String::new()
                    };
                    // 协议位于 params.type（顶层没有 type 字段），修复协议列空白
                    let ty = getp(&["type", "Type"]);
                    let fs = getp(&["fs", "Fs"]);
                    let addr = getp(&["addr", "Addr", "listen", "Listen"]);
                    let name = ensure_serve_name(ctx, &ty, &fs, &addr);
                    ServeRow {
                        id: id.into(),
                        name: name.into(),
                        stype: ty.into(),
                        fs: fs.into(),
                        addr: addr.into(),
                    }
                })
                .collect();
            *ctx.serves_cache.lock().unwrap() = rows.clone();
            apply_serves_filter(ui, &rows).await;
            false
        }
        Cmd::ServeStart => {
            let (stype, fs_disp, addr, user, pass, auto, svc_name) = read_ui(ui, |ui| {
                let g = ui.global::<ServesG>();
                let ty = g
                    .get_types()
                    .row_data(g.get_type_idx().max(0) as usize)
                    .unwrap_or_default()
                    .to_string();
                (
                    ty,
                    g.get_new_fs().to_string(),
                    g.get_addr().to_string(),
                    g.get_user().to_string(),
                    g.get_pass().to_string(),
                    g.get_auto_run(),
                    g.get_svc_name().to_string(),
                )
            })
            .await
            .unwrap_or_default();
            let fs = display_to_fs(&fs_disp);
            if fs.is_empty() {
                toast(ui, 2, "请选择或输入远程路径");
                return false;
            }
            if addr.trim().is_empty() {
                toast(ui, 2, "请填写监听地址");
                return false;
            }
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(true));
            let client = ctx.mgr.client.read().await.clone();
            let res = client
                .serve_start(&stype, &fs, &addr, &user, &pass)
                .await;
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(false));
            match res {
                Ok(id) => {
                    let addr_t = addr.trim().to_string();
                    {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        // 服务名称：优先用用户输入；否则复用已有映射；再否则生成默认 Serve_NN
                        let svc_name = svc_name.trim().to_string();
                        let name = if !svc_name.is_empty() {
                            svc_name
                        } else if let Some(s) = cfg
                            .serve_names
                            .iter()
                            .find(|s| s.stype == stype && s.fs == fs && s.addr == addr_t)
                        {
                            s.name.clone()
                        } else {
                            next_serve_name(&cfg)
                        };
                        cfg.serve_names.retain(|s| !(s.stype == stype && s.fs == fs && s.addr == addr_t));
                        cfg.serve_names.push(config::ServeName {
                            stype: stype.clone(),
                            fs: fs.clone(),
                            addr: addr_t.clone(),
                            name: name.clone(),
                        });
                        if auto {
                            let item = config::AutoServe {
                                stype: stype.clone(),
                                fs: fs.clone(),
                                addr: addr_t.clone(),
                                user: user.clone(),
                                pass: pass.clone(),
                            };
                            cfg.auto_serves
                                .retain(|s| !(s.fs == item.fs && s.addr == item.addr && s.stype == item.stype));
                            cfg.auto_serves.push(item);
                        }
                        let _ = config::save(&cfg);
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ServesRefresh)).await;
                    push_history(ctx, ui, &fs_disp).await;
                    toast(ui, 1, format!("服务已启动: {id}"));
                }
                Err(e) => toast(ui, 2, format!("启动失败: {e}")),
            }
            false
        }
        Cmd::ServeStop { id } => {
            let client = ctx.mgr.client.read().await.clone();
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(true));
            let res = client.serve_stop(&id).await;
            // 取该服务的 fs/addr，用于移除自动启动项
            let mut match_key: Option<(String, String, String)> = None;
            if res.is_ok() {
                if let Ok(v) = client.serve_list().await {
                    if let Some(items) = v.get("list").and_then(|x| x.as_array()) {
                        for it in items {
                            let iid = it
                                .get("id")
                                .and_then(|x| {
                                    x.as_str().map(|s| s.to_string())
                                        .or_else(|| x.as_i64().map(|n| n.to_string()))
                                })
                                .unwrap_or_default();
                            if iid == id {
                                let params = it.get("params").cloned().unwrap_or(json!({}));
                                let getp = |keys: &[&str]| -> String {
                                    for k in keys {
                                        if let Some(s) = params.get(*k).and_then(|x| x.as_str()) {
                                            return s.to_string();
                                        }
                                        if let Some(s) = it.get(*k).and_then(|x| x.as_str()) {
                                            return s.to_string();
                                        }
                                    }
                                    String::new()
                                };
                                match_key = Some((
                                    it.get("type").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                    getp(&["fs", "Fs"]),
                                    getp(&["addr", "Addr", "listen", "Listen"]),
                                ));
                                break;
                            }
                        }
                    }
                }
            }
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(false));
            match res {
                Ok(()) => {
                    if let Some((stype, fs, addr)) = match_key {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        let before = cfg.auto_serves.len();
                        cfg.auto_serves
                            .retain(|s| !(s.stype == stype && s.fs == fs && s.addr == addr));
                        if cfg.auto_serves.len() != before {
                            let _ = config::save(&cfg);
                        }
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ServesRefresh)).await;
                    toast(ui, 1, format!("已停止 {id}"));
                }
                Err(e) => toast(ui, 2, format!("停止失败: {e}")),
            }
            false
        }
        Cmd::TransfersEnter => {
            // 把已保存的全局限速回填到 UI（仅在进入页面时，不覆盖正在输入的草稿）
            {
                let cfg = ctx.cfg.lock().unwrap().clone();
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    let g = ui.global::<TransfersG>();
                    g.set_up_limit(cfg.bw_up.clone().into());
                    g.set_up_unit(cfg.bw_up_unit);
                    g.set_down_limit(cfg.bw_down.clone().into());
                    g.set_down_unit(cfg.bw_down_unit);
                });
            }
            build_tasks(ctx, ui).await;
            apply_global_limit(ctx, ui).await;
            false
        }
        Cmd::HideDoneChanged => {
            build_tasks(ctx, ui).await;
            false
        }
        Cmd::LimitApply => {
            // 从 UI 读取草稿 → 写配置 → 下发 rclone
            let draft = read_ui(ui, |u| {
                let g = u.global::<TransfersG>();
                (
                    g.get_up_limit().to_string(),
                    g.get_up_unit(),
                    g.get_down_limit().to_string(),
                    g.get_down_unit(),
                )
            })
            .await
            .unwrap_or_default();
            {
                let mut cfg = ctx.cfg.lock().unwrap();
                cfg.bw_up = draft.0;
                cfg.bw_up_unit = draft.1;
                cfg.bw_down = draft.2;
                cfg.bw_down_unit = draft.3;
                let _ = config::save(&cfg);
            }
            apply_global_limit(ctx, ui).await;
            toast(ui, 1, "限速已应用");
            false
        }
        Cmd::JobStop { id } => {
            let client = ctx.mgr.client.read().await.clone();
            match client.job_stop(id).await {
                Ok(()) => {
                    build_tasks(ctx, ui).await;
                    toast(ui, 1, format!("任务 #{} 已停止", id));
                }
                Err(e) => toast(ui, 2, format!("停止失败: {e}")),
            }
            false
        }
        Cmd::OptionsLoad => {
            let client = ctx.mgr.client.read().await.clone();
            match client.options_get().await {
                Ok(v) => {
                    let main = v.get("main").cloned().unwrap_or(json!({}));
                    let transfers = main.get("Transfers").and_then(|x| x.as_i64()).map(|n| n.to_string()).unwrap_or_default();
                    let checkers = main.get("Checkers").and_then(|x| x.as_i64()).map(|n| n.to_string()).unwrap_or_default();
                    let tps = main.get("TPSLimit").and_then(|x| x.as_f64()).map(|n| format!("{:.1}", n)).unwrap_or_default();
                    let log = main
                        .get("LogLevel")
                        .and_then(|x| x.as_str())
                        .unwrap_or("INFO")
                        .to_string();
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        let g = ui.global::<SettingsG>();
                        g.set_opt_transfers(transfers.into());
                        g.set_opt_checkers(checkers.into());
                        g.set_opt_tpslimit(tps.into());
                        let lv = g.get_log_levels();
                        let mut idx: i32 = 1;
                        for i in 0..lv.row_count() {
                            if lv.row_data(i).as_deref() == Some(log.as_str()) {
                                idx = i as i32;
                            }
                        }
                        g.set_log_level_idx(idx);
                    });
                }
                Err(e) => toast(ui, 2, format!("读取参数失败: {e}")),
            }
            false
        }
        Cmd::OptionsSave => {
            let (transfers, checkers, tps, log_level) = read_ui(ui, |ui| {
                let g = ui.global::<SettingsG>();
                let li = g.get_log_level_idx() as usize;
                let lv = g.get_log_levels().row_data(li).unwrap_or_default().to_string();
                (
                    g.get_opt_transfers().to_string(),
                    g.get_opt_checkers().to_string(),
                    g.get_opt_tpslimit().to_string(),
                    lv,
                )
            })
            .await
            .unwrap_or_default();
            let t: i64 = transfers.parse().unwrap_or(4);
            let c: i64 = checkers.parse().unwrap_or(8);
            let p: f64 = tps.parse().unwrap_or(0.0);
            let opts = json!({
                "main": {
                    "Transfers": t,
                    "Checkers": c,
                    "TPSLimit": p,
                    "LogLevel": log_level,
                }
            });
            let client = ctx.mgr.client.read().await.clone();
            match client.options_set(opts).await {
                Ok(()) => toast(ui, 1, "运行参数已保存"),
                Err(e) => toast(ui, 2, format!("保存失败: {e}")),
            }
            false
        }
        Cmd::PluginsRescan => {
            rescan_plugins(ctx, ui).await;
            false
        }
        Cmd::PluginToggle { idx } => {
            let row = read_ui(ui, move |ui| {
                ui.global::<PluginsG>()
                    .get_rows()
                    .row_data(idx as usize)
                    .map(|r| (r.id.to_string(), r.enabled))
            })
            .await
            .flatten();
            if let Some((id, old_enabled)) = row {
                let enabled = !old_enabled;
                {
                    let mut cfg = ctx.cfg.lock().unwrap();
                    cfg.plugin_enabled.insert(id.clone(), enabled);
                    let _ = config::save(&cfg);
                }
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    let g = ui.global::<PluginsG>();
                    let model = g.get_rows();
                    if let Some(mut row) = model.row_data(idx as usize) {
                        row.enabled = enabled;
                        model.set_row_data(idx as usize, row);
                    }
                    toast_ui(
                        &ui,
                        1,
                        format!("插件 {} 已{}", id, if enabled { "启用" } else { "停用" }),
                    );
                });
            }
            false
        }
        Cmd::IhSaveConfig => {
            ih_save_config(ctx, ui).await;
            false
        }
        Cmd::IhAddRoot => {
            let client = ctx.mgr.client.read().await.clone();
            let remotes = client.list_remotes().await.unwrap_or_default();
            let first = remotes.first().cloned().unwrap_or_else(|| "remote".into());
            let _ = ui.upgrade_in_event_loop(
                move |ui| {
                    let g = ui.global::<PluginsG>();
                    let n = g.get_roots().row_count();
                    let mut rows: Vec<RootRow> = (0..n).filter_map(|i| g.get_roots().row_data(i)).collect();
                    rows.push(RootRow {
                        id: format!("root{}", n + 1).into(),
                        remote: format!("{first}:").into(),
                        is_default: n == 0,
                        public_template: "".into(),
                    });
                    g.set_roots(ModelRc::from(Rc::new(VecModel::from(rows))));
                },
            );
            false
        }
        Cmd::IhDelRoot { id } => {
            let _ = ui.upgrade_in_event_loop(
                move |ui| {
                    let g = ui.global::<PluginsG>();
                    let n = g.get_roots().row_count();
                    let mut rows: Vec<RootRow> = (0..n).filter_map(|i| g.get_roots().row_data(i)).collect();
                    rows.retain(|r| r.id != id);
                    g.set_roots(ModelRc::from(Rc::new(VecModel::from(rows))));
                },
            );
            false
        }
        Cmd::IhRootDefault { id } => {
            let _ = ui.upgrade_in_event_loop(
                move |ui| {
                    let g = ui.global::<PluginsG>();
                    let n = g.get_roots().row_count();
                    let mut rows: Vec<RootRow> = (0..n).filter_map(|i| g.get_roots().row_data(i)).collect();
                    for r in rows.iter_mut() {
                        r.is_default = r.id == id;
                    }
                    g.set_roots(ModelRc::from(Rc::new(VecModel::from(rows))));
                },
            );
            false
        }
        Cmd::IhAddToken => {
            let params = plugin_http_req(
                "POST",
                "/token",
                Some(&json!({"name": "token"})),
                json!({}),
            );
            match ctx
                .host
                .invoke(&find_wasm("imagehost"), "request", params, Some(ctx.caps()), "imagehost")
                .await
            {
                Ok(_) => {
                    load_ih_config(ctx, ui).await;
                    toast(ui, 1, "令牌已创建");
                }
                Err(e) => toast(ui, 2, format!("创建令牌失败: {e}")),
            }
            false
        }
        Cmd::IhRevokeToken { id } => {
            let params = plugin_http_req("DELETE", &format!("/token/{id}"), None, json!({}));
            match ctx
                .host
                .invoke(&find_wasm("imagehost"), "request", params, Some(ctx.caps()), "imagehost")
                .await
            {
                Ok(_) => {
                    load_ih_config(ctx, ui).await;
                    toast(ui, 1, "令牌已撤销");
                }
                Err(e) => toast(ui, 2, format!("撤销失败: {e}")),
            }
            false
        }
        Cmd::IhImagesRefresh => {
            let params = plugin_http_req("GET", "/files", None, json!({"pageSize": "200"}));
            match ctx
                .host
                .invoke(&find_wasm("imagehost"), "request", params, Some(ctx.caps()), "imagehost")
                .await
            {
                Ok(v) => {
                    let items = v
                        .get("items")
                        .and_then(|x| x.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let _ = ui.upgrade_in_event_loop(
                        move |ui| {
                            let rows: Vec<ImageRow> = items
                                .iter()
                                .map(|it| ImageRow {
                                    name: it.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                                    path: it
                                        .get("storedPath")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .into(),
                                    url: it.get("url").and_then(|x| x.as_str()).unwrap_or("").into(),
                                    size: fmt_bytes(
                                        it.get("size").and_then(|x| x.as_i64()).unwrap_or(0) as f64,
                                    )
                                    .into(),
                                    sha1: it.get("sha1").and_then(|x| x.as_str()).unwrap_or("").into(),
                                    uploaded: it
                                        .get("uploadedAt")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .into(),
                                })
                                .collect();
                            ui.global::<PluginsG>()
                                .set_images(ModelRc::from(Rc::new(VecModel::from(rows))));
                        },
                    );
                }
                Err(e) => toast(ui, 2, format!("读取索引失败: {e}")),
            }
            false
        }
        Cmd::IhImageDel { path } => {
            let root_id = read_ui(ui, |ui| {
                ui.global::<PluginsG>()
                    .get_roots()
                    .row_data(0)
                    .map(|r| r.id.to_string())
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let body = json!({"rootId": root_id, "paths": [path]});
            let params = plugin_http_req("POST", "/files/delete", Some(&body), json!({}));
            match ctx
                .host
                .invoke(&find_wasm("imagehost"), "request", params, Some(ctx.caps()), "imagehost")
                .await
            {
                Ok(_) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::IhImagesRefresh)).await;
                    toast(ui, 1, "已删除（回收站）");
                }
                Err(e) => toast(ui, 2, format!("删除失败: {e}")),
            }
            false
        }
        Cmd::CopyText { text } => {
            clipboard_copy(&text);
            toast(ui, 1, "已复制到剪贴板");
            false
        }
        Cmd::SettingsSave => {
            // 路径与内嵌不同则视为用户自定义（IO 放后台线程）
            let embedded_path = extract_embedded_rclone().unwrap_or_default();
            let save_result = read_ui(ui, {
                let ctx = ctx.clone();
                move |ui| -> Result<(), String> {
                    let s = ui.global::<SettingsG>();
                    let mut cfg = ctx.cfg.lock().unwrap().clone();
                    cfg.rclone.mode = s.get_mode_idx() as u8;
                    cfg.rclone.exe = s.get_exe().to_string();
                    cfg.rclone.exe_custom = !cfg.rclone.exe.is_empty() && cfg.rclone.exe != embedded_path;
                    cfg.rclone.port = s.get_rc_port().parse().unwrap_or(5574);
                    cfg.rclone.user = s.get_rc_user().to_string();
                    cfg.rclone.pass = s.get_rc_pass().to_string();
                    cfg.rclone.url = s.get_rc_url().to_string();
                    cfg.theme_mode = s.get_theme_idx() as u8;
                    cfg.accent_idx = s.get_accent_idx() as u8;
                    cfg.autostart = s.get_autostart();
                    cfg.silent_start = s.get_silent();
                    cfg.http.enabled = s.get_http_enabled();
                    cfg.http.port = s.get_http_port().parse().unwrap_or(5273);
                    set_autostart(cfg.autostart, cfg.silent_start);
                    apply_theme(&ui, cfg.theme_mode);
                    ui.global::<ThemeG>().set_accent_idx(cfg.accent_idx as i32);
                    *ctx.cfg.lock().unwrap() = cfg.clone();
                    config::save(&cfg).map_err(|e| e.to_string())
                }
            })
            .await;
            match save_result {
                Some(Ok(())) => toast(ui, 1, "设置已保存（连接相关改动重启后生效）"),
                Some(Err(e)) => toast(ui, 2, format!("配置保存失败: {e}")),
                None => {}
            }
            ctx.http_restart.notify_one();
            false
        }
        Cmd::SettingsTest => {
            let client = ctx.mgr.client.read().await.clone();
            match client.version().await {
                Ok(v) => toast(ui, 1, format!("连接正常：rclone {}", v.version)),
                Err(e) => toast(ui, 2, format!("连接失败: {e}")),
            }
            false
        }
    }
}

// ---------------- 图床配置 ----------------

async fn rescan_plugins(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let mut rows: Vec<PluginRow> = Vec::new();
    let mut dirs = vec![config::data_dir().join("plugins")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.join("assets/plugins"));
        }
    }
    let enabled_map = ctx.cfg.lock().unwrap().plugin_enabled.clone();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("wasm") {
                if let Ok(m) = ctx.host.load_manifest(&p).await {
                    let enabled = enabled_map.get(&m.id).copied().unwrap_or(true);
                    rows.push(PluginRow {
                        id: m.id.clone().into(),
                        name: m.name.clone().into(),
                        version: m.version.into(),
                        desc: m.description.into(),
                        loaded: true,
                        enabled,
                    });
                }
            }
        }
    }
    let imagehost = rows.iter().any(|r| r.id == "imagehost");
    let _ = ui.upgrade_in_event_loop(
        move |ui| {
            let g = ui.global::<PluginsG>();
            g.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            g.set_loaded(imagehost);
        },
    );
    if imagehost {
        load_ih_config(ctx, ui).await;
        let _ = Box::pin(handle_cmd(ctx, ui, Cmd::IhImagesRefresh)).await;
    }
}

async fn load_ih_config(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let cfg_raw = ctx.store.get_all("imagehost");
    let cfg = cfg_raw.get("config").cloned().unwrap_or(json!({}));
    let tokens = cfg
        .get("tokens")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let _ = ui.upgrade_in_event_loop(
        move |ui| {
            let g = ui.global::<PluginsG>();
            let roots: Vec<RootRow> = cfg
                .get("roots")
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .map(|r| RootRow {
                            id: r.get("id").and_then(|x| x.as_str()).unwrap_or("").into(),
                            remote: r.get("remote").and_then(|x| x.as_str()).unwrap_or("").into(),
                            is_default: r.get("isDefault").and_then(|x| x.as_bool()).unwrap_or(false),
                            public_template: r
                                .get("publicUrlTemplate")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .into(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            g.set_roots(ModelRc::from(Rc::new(VecModel::from(roots.clone()))));
            if let Some(n) = cfg.get("naming") {
                let mode = n.get("mode").and_then(|x| x.as_str()).unwrap_or("timestamp");
                let modes = g.get_naming();
                for i in 0..modes.row_count() {
                    if modes.row_data(i).as_deref() == Some(mode) {
                        g.set_naming_idx(i as i32);
                    }
                }
                g.set_seq_pad(
                    n.get("seq_pad")
                        .and_then(|x| x.as_i64())
                        .unwrap_or(3)
                        .to_string()
                        .into(),
                );
            }
            g.set_dedupe(cfg.get("dedupe").and_then(|x| x.as_bool()).unwrap_or(true));
            g.set_public_template(
                roots
                    .iter()
                    .find(|r| r.is_default)
                    .map(|r| r.public_template.clone())
                    .unwrap_or_default(),
            );
            if let Some(o) = cfg.get("obsidian") {
                let layout = o.get("dir_layout").and_then(|x| x.as_str()).unwrap_or("note");
                let layouts = g.get_obs();
                for i in 0..layouts.row_count() {
                    if layouts.row_data(i).as_deref() == Some(layout) {
                        g.set_obs_idx(i as i32);
                    }
                }
            }
            g.set_trash_dir(
                cfg.get("trash")
                    .and_then(|t| t.get("dir"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .into(),
            );
            let toks: Vec<TokenRow> = tokens
                .iter()
                .map(|t| TokenRow {
                    id: t.get("id").and_then(|x| x.as_str()).unwrap_or("").into(),
                    name: t.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                    token: t.get("token").and_then(|x| x.as_str()).unwrap_or("").into(),
                    created: t.get("created").and_then(|x| x.as_str()).unwrap_or("").into(),
                })
                .collect();
            g.set_tokens(ModelRc::from(Rc::new(VecModel::from(toks))));
        },
    );
}

async fn ih_save_config(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let (roots, naming_idx, seq_pad, dedupe, public_template, obs_idx, trash_dir) = read_ui(ui, |ui| {
            let g = ui.global::<PluginsG>();
            let n = g.get_roots().row_count();
            let roots: Vec<Value> = (0..n)
                .filter_map(|i| g.get_roots().row_data(i))
                .map(|r| {
                    json!({
                        "id": r.id.to_string(),
                        "remote": r.remote.to_string(),
                        "isDefault": r.is_default,
                        "publicUrlTemplate": r.public_template.to_string(),
                    })
                })
                .collect();
            (
                roots,
                g.get_naming_idx(),
                g.get_seq_pad().to_string(),
                g.get_dedupe(),
                g.get_public_template().to_string(),
                g.get_obs_idx(),
                g.get_trash_dir().to_string(),
            )
        })
        .await
        .unwrap_or_default();

    let naming_mode = ["timestamp", "seq", "keep", "hash"]
        .get(naming_idx.max(0) as usize)
        .copied()
        .unwrap_or("timestamp");
    let obs_layout = ["flat", "date", "vault", "note"]
        .get(obs_idx.max(0) as usize)
        .copied()
        .unwrap_or("note");

    let body = json!({
        "roots": roots,
        "naming": { "mode": naming_mode, "seq_pad": seq_pad.parse::<i64>().unwrap_or(3) },
        "dedupe": dedupe,
        "obsidian": { "dir_layout": obs_layout },
        "trash": { "dir": trash_dir },
    });
    let params = plugin_http_req("PUT", "/config", Some(&body), json!({}));
    match ctx
        .host
        .invoke(&find_wasm("imagehost"), "request", params, Some(ctx.caps()), "imagehost")
        .await
    {
        Ok(_) => {
            load_ih_config(ctx, ui).await;
            toast(ui, 1, "图床配置已保存");
        }
        Err(e) => toast(ui, 2, format!("保存失败: {e}")),
    }
}

/// 构造插件协议的 http 请求参数
fn plugin_http_req(method: &str, path: &str, body: Option<&Value>, query: Value) -> Value {
    json!({
        "http": {
            "method": method,
            "path": path,
            "query": query,
            "authorization": "",
            "bodyB64": body
                .map(|b| b64(&serde_json::to_vec(b).unwrap_or_default()))
                .unwrap_or_default(),
            "multipart": null,
            "origin": "",
        },
        "now": crate::http::now_json(),
        "req_id": uuid::Uuid::new_v4().simple().to_string(),
    })
}

fn find_wasm(id: &str) -> PathBuf {
    let p = config::data_dir().join("plugins").join(format!("{id}.wasm"));
    if p.exists() {
        return p;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let p = d.join("assets/plugins").join(format!("{id}.wasm"));
            if p.exists() {
                return p;
            }
        }
    }
    p
}

// ---------------- UI 读取辅助 ----------------

async fn ui_files_pos(ui: &Weak<MainWindow>) -> (String, String) {
    read_ui(ui, |ui| {
        let g = ui.global::<FilesG>();
        let remotes = g.get_remotes();
        let idx = g.get_remote_idx();
        let remote = remotes
            .row_data(idx.max(0) as usize)
            .unwrap_or_default()
            .to_string();
        (remote, g.get_path().to_string())
    })
    .await
    .unwrap_or_default()
}

// ---------------- 回调接线 ----------------

fn wire_callbacks(
    ui: &MainWindow,
    _ctx: Arc<Ctx>,
    tx: mpsc::UnboundedSender<Cmd>,
    debounce: SearchDebounce,
) {
    ui.on_refresh_dashboard({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::RefreshDashboard);
        }
    });
    ui.on_files_refresh({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move || {
            // 抢占式刷新：递增 seq 使旧的在途请求结果失效
            let Some(ui) = ui.upgrade() else { return };
            let g = ui.global::<FilesG>();
            g.set_loading(true);
            g.set_seq(g.get_seq() + 1);
            let _ = tx.send(Cmd::FilesRefresh { seq: g.get_seq() });
        }
    });
    ui.on_files_open({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let row = ui
                .upgrade()
                .and_then(|ui| ui.global::<FilesG>().get_entries().row_data(i as usize));
            let Some(row) = row else { return };
            if row.is_dir {
                // 进入目录：请求进行中禁点；成功后才更新路径（防拼出不存在的路径）
                if let Some(ui) = ui.upgrade() {
                    let g = ui.global::<FilesG>();
                    if g.get_loading() {
                        return;
                    }
                    g.set_loading(true);
                    g.set_seq(g.get_seq() + 1);
                    let _ = tx.send(Cmd::FilesEnter {
                        name: row.name.to_string(),
                        seq: g.get_seq(),
                    });
                }
            } else if let Some(local) = rfd::FileDialog::new()
                .set_file_name(&row.name)
                .set_title("保存文件到…")
                .save_file()
            {
                let _ = tx.send(Cmd::FilesDownload {
                    name: row.name.to_string(),
                    local,
                });
            }
        }
    });
    ui.on_files_up({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move || {
            // 抢占式返回上级：同样走 seq 失效机制
            let Some(ui) = ui.upgrade() else { return };
            let g = ui.global::<FilesG>();
            g.set_loading(true);
            g.set_seq(g.get_seq() + 1);
            let _ = tx.send(Cmd::FilesUp { seq: g.get_seq() });
        }
    });
    ui.on_files_mkdir({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::FilesMkdir);
        }
    });
    ui.on_files_del({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            // 确认已在 UI 层（ConfirmPop）完成
            let row = ui
                .upgrade()
                .and_then(|ui| ui.global::<FilesG>().get_entries().row_data(i as usize));
            let Some(row) = row else { return };
            let _ = tx.send(Cmd::FilesDel {
                name: row.name.to_string(),
                is_dir: row.is_dir,
            });
        }
    });
    ui.on_files_rename({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let (name, new_name) = ui
                .upgrade()
                .map(|ui| {
                    let g = ui.global::<FilesG>();
                    let name = g
                        .get_entries()
                        .row_data(i as usize)
                        .map(|r| r.name.to_string())
                        .unwrap_or_default();
                    (name, g.get_rename_name().to_string())
                })
                .unwrap_or_default();
            let _ = tx.send(Cmd::FilesRename { from: name, to: new_name });
        }
    });
    ui.on_files_copy({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let name = ui
                .upgrade()
                .and_then(|ui| ui.global::<FilesG>().get_entries().row_data(i as usize))
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let _ = tx.send(Cmd::FilesCopy { name });
        }
    });
    ui.on_files_copy_link({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let name = ui
                .upgrade()
                .and_then(|ui| ui.global::<FilesG>().get_entries().row_data(i as usize))
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let _ = tx.send(Cmd::FilesCopyLink { name });
        }
    });
    ui.on_mounts_refresh({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::MountsRefresh);
        }
    });
    ui.on_mount_add({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::MountAdd);
        }
    });
    ui.on_mount_remove({
        let tx = tx.clone();
        move |point| {
            let _ = tx.send(Cmd::MountRemove { point: point.to_string() });
        }
    });
    ui.on_mount_path_nav({
        let tx = tx.clone();
        move |name| {
            let _ = tx.send(Cmd::HistPick { scope: 0, path: name.to_string() });
        }
    });
    ui.on_serve_path_nav({
        let tx = tx.clone();
        move |name| {
            let _ = tx.send(Cmd::HistPick { scope: 1, path: name.to_string() });
        }
    });
    ui.on_mounts_search({
        let db = debounce.clone();
        move |_| {
            db.bump(1);
        }
    });
    ui.on_serves_search({
        let db = debounce.clone();
        move |_| {
            db.bump(2);
        }
    });
    ui.on_browse_open({
        let tx = tx.clone();
        move |scope| {
            let _ = tx.send(Cmd::BrowseOpen { scope });
        }
    });
    ui.on_browse_goto({
        let tx = tx.clone();
        move |name| {
            let _ = tx.send(Cmd::BrowseGoto { name: name.to_string() });
        }
    });
    ui.on_browse_ok({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::BrowseOk);
        }
    });
    ui.on_nav_select({
        let tx = tx.clone();
        move |i| {
            match i {
                0 => { let _ = tx.send(Cmd::RefreshDashboard); }
                1 => { let _ = tx.send(Cmd::FilesInit); }
                2 => { let _ = tx.send(Cmd::RemotesRefresh); }
                3 => { let _ = tx.send(Cmd::MountsRefresh); }
                4 => { let _ = tx.send(Cmd::ServesRefresh); }
                5 => { let _ = tx.send(Cmd::TransfersEnter); }
                7 => { let _ = tx.send(Cmd::OptionsLoad); }
                _ => {}
            }
        }
    });
    ui.on_remotes_refresh({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::RemotesRefresh);
        }
    });
    ui.on_remotes_filter({
        let db = debounce.clone();
        move |_| {
            db.bump(0);
        }
    });
    ui.on_remote_add({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::RemoteAdd);
        }
    });
    ui.on_remote_edit({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let name = ui
                .upgrade()
                .and_then(|ui| ui.global::<RemotesG>().get_rows().row_data(i as usize))
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let _ = tx.send(Cmd::RemoteEditLoad { name });
        }
    });
    ui.on_remote_del({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let name = ui
                .upgrade()
                .and_then(|ui| ui.global::<RemotesG>().get_rows().row_data(i as usize))
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let _ = tx.send(Cmd::RemoteDelete { name });
        }
    });
    ui.on_remote_editor_save({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::RemoteSave);
        }
    });
    ui.on_remote_editor_cancel({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::RemoteCancel);
        }
    });
    ui.on_remote_type_changed({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::RemoteTypeChanged);
        }
    });
    ui.on_serves_refresh({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::ServesRefresh);
        }
    });
    ui.on_serve_start({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::ServeStart);
        }
    });
    ui.on_serve_stop({
        let tx = tx.clone();
        move |id| {
            let _ = tx.send(Cmd::ServeStop { id: id.to_string() });
        }
    });
    ui.on_transfers_refresh({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::TransfersEnter);
        }
    });
    ui.on_hide_done_changed({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::HideDoneChanged);
        }
    });
    ui.on_transfers_task_search({
        let db = debounce.clone();
        move |_| {
            db.bump(3);
        }
    });
    ui.on_limit_apply({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::LimitApply);
        }
    });
    ui.on_job_stop({
        let tx = tx.clone();
        move |id| {
            let _ = tx.send(Cmd::JobStop { id: id as i64 });
        }
    });
    ui.on_options_save({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::OptionsSave);
        }
    });
    ui.on_plugins_rescan({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::PluginsRescan);
        }
    });
    ui.on_ih_save_config({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::IhSaveConfig);
        }
    });
    ui.on_ih_add_root({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::IhAddRoot);
        }
    });
    ui.on_ih_del_root({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let id = ui
                .upgrade()
                .and_then(|ui| ui.global::<PluginsG>().get_roots().row_data(i as usize))
                .map(|r| r.id.to_string())
                .unwrap_or_default();
            let _ = tx.send(Cmd::IhDelRoot { id });
        }
    });
    ui.on_ih_root_default({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let id = ui
                .upgrade()
                .and_then(|ui| ui.global::<PluginsG>().get_roots().row_data(i as usize))
                .map(|r| r.id.to_string())
                .unwrap_or_default();
            let _ = tx.send(Cmd::IhRootDefault { id });
        }
    });
    ui.on_ih_add_token({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::IhAddToken);
        }
    });
    ui.on_ih_revoke_token({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            // 确认已在 UI 层（ConfirmPop）完成
            let t = ui
                .upgrade()
                .and_then(|ui| ui.global::<PluginsG>().get_tokens().row_data(i as usize));
            let Some(t) = t else { return };
            let _ = tx.send(Cmd::IhRevokeToken { id: t.id.to_string() });
        }
    });
    ui.on_ih_images_refresh({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::IhImagesRefresh);
        }
    });
    ui.on_ih_image_del({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            // 确认已在 UI 层（ConfirmPop）完成
            let path = ui
                .upgrade()
                .and_then(|ui| ui.global::<PluginsG>().get_images().row_data(i as usize))
                .map(|r| r.path.to_string())
                .unwrap_or_default();
            let _ = tx.send(Cmd::IhImageDel { path });
        }
    });
    ui.on_ih_copy_url({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let url = ui
                .upgrade()
                .and_then(|ui| ui.global::<PluginsG>().get_images().row_data(i as usize))
                .map(|r| r.url.to_string())
                .unwrap_or_default();
            if !url.is_empty() {
                clipboard_copy(&url);
                let _ = tx.send(Cmd::CopyText { text: url });
            }
        }
    });
    ui.on_copy_text(move |text| {
        clipboard_copy(&text);
    });
    ui.on_plugin_toggle({
        let tx = tx.clone();
        move |i| {
            let _ = tx.send(Cmd::PluginToggle { idx: i });
        }
    });
    ui.on_settings_save({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::SettingsSave);
        }
    });
    ui.on_settings_test({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::SettingsTest);
        }
    });
    ui.on_pick_exe({
        let ui = ui.as_weak();
        let _tx2 = tx.clone();
        move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("rclone", &["exe"])
                .set_title("选择 rclone.exe")
                .pick_file()
            {
                if let Some(ui) = ui.upgrade() {
                    ui.global::<SettingsG>()
                        .set_exe(path.display().to_string().into());
                }
            }
        }
    });

    // 初始加载
    let _ = tx.send(Cmd::RefreshDashboard);
    let _ = tx.send(Cmd::FilesInit);
    let _ = tx.send(Cmd::RemotesRefresh);
    let _ = tx.send(Cmd::MountsRefresh);
    let _ = tx.send(Cmd::PluginsRescan);
}

// ---------------- 托盘 ----------------

fn spawn_tray(ui: Weak<MainWindow>, tx: mpsc::UnboundedSender<Cmd>) {
    std::thread::spawn(move || {
        use tray_icon::menu::{Menu, MenuEvent, MenuItem};
        use tray_icon::{TrayIconBuilder, TrayIconEvent};

        let icon_rgba = make_icon_rgba(32);
        let Ok(icon) = tray_icon::Icon::from_rgba(icon_rgba, 32, 32) else {
            return;
        };
        let menu = Menu::new();
        let open_item = MenuItem::with_id("open", "打开主窗口", true, None);
        let quit_item = MenuItem::with_id("quit", "退出", true, None);
        let _ = menu.append_items(&[&open_item, &quit_item]);

        let _tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Rclone GUI")
            .with_icon(icon)
            .build();

        loop {
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                match ev.id.0.as_str() {
                    "open" => {
                        let _ = ui.upgrade_in_event_loop(
                            |ui| {
                                ui.show().ok();
                                ui.window().set_minimized(false);
                            },
                        );
                    }
                    "quit" => {
                        let _ = tx.send(Cmd::Quit);
                    }
                    _ => {}
                }
            }
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                // 左键点击释放时显示主窗口（0.14 无 DoubleClick 变体）
                if matches!(
                    ev,
                    TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    }
                ) {
                    let _ = ui.upgrade_in_event_loop(|ui| {
                        ui.show().ok();
                        ui.window().set_minimized(false);
                    });
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });
}

/// 程序化生成图标：蓝底圆形 + 白色闪电
fn make_icon_rgba(size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; size * size * 4];
    let center = (size - 1) as f32 / 2.0;
    let r = size as f32 / 2.0 - 1.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let i = (y * size + x) * 4;
            if dist <= r {
                let t = y as f32 / size as f32;
                buf[i] = (37.0 + t * 30.0) as u8;
                buf[i + 1] = (99.0 + t * 40.0) as u8;
                buf[i + 2] = (235.0 + t * 20.0) as u8;
                let nx = (x as f32 - center) / r;
                let ny = (y as f32 - center) / r;
                let bolt = (ny > -0.9 && ny < 0.1 && nx > -0.08 - ny * 0.22 && nx < 0.08 - ny * 0.22)
                    || (ny >= 0.1 && ny < 0.8 && nx > 0.02 - ny * 0.45 && nx < 0.2 - ny * 0.45);
                if bolt {
                    buf[i] = 255;
                    buf[i + 1] = 255;
                    buf[i + 2] = 255;
                }
                buf[i + 3] = 255;
            }
        }
    }
    buf
}
