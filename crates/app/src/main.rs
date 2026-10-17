// release 版不弹出控制台窗口（debug 保留便于调试）
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Rclone GUI（Windows 桌面版）：Rust + Slint + rclone RC + WASM 插件
//!
//! 架构：主线程跑 Slint UI；后台 std 线程内 tokio runtime 负责
//! rclone 生命周期 / 内嵌 HTTP / 统计轮询 / UI 命令处理；托盘独立线程。

mod config;
mod conf_io;
mod http;
mod kv;
mod persist;
mod profiles;
mod script;

use anyhow::anyhow;
use rcgui_rc::{ConnStatus, Handshake, RcState};
use rclone_client::{local_fs, RcloneManager};
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
    /// 从文件（zip）或文件夹添加插件包
    PluginAddFromFile,
    PluginAddFromDir,
    /// 打开插件目录
    PluginOpenDir,
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
    /// 连接：按设置页当前字段建立连接并握手（可能拉起托管 rcd）
    Connect,
    /// 断开：只停止使用该端点，不终止 rclone 进程
    Disconnect,
    /// 重新握手：不动进程，仅重跑版本/配置路径/能力探测
    Reconnect,
    /// 导出 rcgui.conf 到指定路径
    ConfExport { path: PathBuf, include_secrets: bool },
    /// 解析指定 rcgui.conf 并生成预览（不写盘）
    ConfPreview { path: PathBuf },
    /// 应用上一次预览过的导入（先备份当前档案）
    ConfApply,
    /// 生成恢复脚本并预览（只写文件，不执行、不注册启动项）
    ScriptGenerate,
    /// 复制当前预览的脚本到剪贴板
    ScriptCopy,
    /// 复选框切换权限后：判断是否需要重启，并弹出确认弹层
    AdminToggle,
    /// 用户确认「以管理员身份运行」：按 UI 里的目标状态重启自身
    AdminRestart,
    /// 用户取消重启：把复选框恢复成当前真实权限状态
    AdminCancel,
    Quit,
}

struct Ctx {
    /// 连接状态（RC 调用入口）。UI 层只从这里取客户端，
    /// 不再经由进程管理器，从而与「rclone 进程是谁拉起的」解耦。
    rc: Arc<RcState>,
    /// legacy：rclone 进程生命周期。阶段 7 移除。
    mgr: Arc<RcloneManager>,
    host: Arc<plugin_host::PluginHost>,
    store: Arc<kv::KvStore>,
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
    /// 周期任务防重入：上一次未完成则跳过，避免挂起任务无限堆积（内存泄漏）
    tick_busy: Arc<std::sync::atomic::AtomicBool>,
    /// 瞬时速度采样（上次累计字节, 时刻）。rclone 的 speed 是平均值，看起来不会动。
    speed_meter: Arc<std::sync::Mutex<Option<(f64, std::time::Instant)>>>,
    /// 窗口是否可见且未最小化（由 UI 线程每 500ms 采样写入）：门控周期轮询
    visible: Arc<std::sync::atomic::AtomicBool>,
    /// 连接/握手进行中（防止连点重复发起）
    conn_busy: Arc<std::sync::atomic::AtomicBool>,
    /// 已通过「导入预览」校验、等待用户点「应用导入」的文档。
    /// 两步流程本身就是一次确认：预览不写盘，应用才写盘并先备份。
    pending_import: Arc<std::sync::Mutex<Option<rcgui_profile::bundle::RcguiConf>>>,
    /// 同类刷新单飞闸门：防止重叠请求，保证「旧响应不覆盖新状态」
    gate_mounts: Arc<std::sync::atomic::AtomicBool>,
    gate_serves: Arc<std::sync::atomic::AtomicBool>,
    gate_remotes: Arc<std::sync::atomic::AtomicBool>,
    /// 内嵌 HTTP 服务当前实例：重启前必须优雅关闭旧实例，否则端口仍被占用（os error 10048）
    http_server: Arc<tokio::sync::Mutex<Option<HttpServer>>>,
}

/// 运行中的内嵌 HTTP 服务句柄
struct HttpServer {
    shutdown: tokio::sync::watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Ctx {
    fn caps(&self) -> Arc<kv::RealCaps> {
        Arc::new(kv::RealCaps {
            rc: self.rc.clone(),
            store: self.store.clone(),
        })
    }
}

/// 单飞闸门：持有期间表示「同类请求正在执行」，作用域结束时自动复位。
///
/// 用 Drop 而不是手动复位，是为了让提前 `return`、`?`、panic 都不会把闸门卡死。
struct GateGuard(Arc<std::sync::atomic::AtomicBool>);

/// 周期任务守卫：即使任务 panic 也能复位，避免轮询永久停摆。
struct TickGuard(Arc<std::sync::atomic::AtomicBool>);

impl Drop for TickGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
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

/// 按 (协议, 地址) 查找或分配服务名称，并持久化映射
/// （地址含端口，可唯一标识一个监听服务；remote/sub 仅作记录）。
fn ensure_serve_name(ctx: &Arc<Ctx>, stype: &str, fs: &str, addr: &str) -> String {
    let mut cfg = ctx.cfg.lock().unwrap();
    if let Some(s) = cfg
        .serve_names
        .iter()
        .find(|s| s.stype == stype && s.addr == addr)
    {
        return s.name.clone();
    }
    let (remote, sub_path) = split_fs_str(fs);
    let name = next_serve_name(&cfg);
    cfg.serve_names.push(config::ServeName {
        stype: stype.to_string(),
        remote,
        sub_path,
        addr: addr.to_string(),
        name: name.clone(),
    });
    let _ = config::save(&cfg);
    name
}

/// 记录远程路径历史（去重、最新在前、最多 10 条），并同步到两个界面
#[allow(dead_code)]
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

async fn apply_remotes_filter(ui: &Weak<MainWindow>, full: &[(String, String, String)]) {
    let filter = read_ui(ui, |u| u.global::<RemotesG>().get_filter().to_string())
        .await
        .unwrap_or_default()
        .to_lowercase();
    // 徽标数据：档案里「凭据未随包导出」的远程（精确名匹配）。
    let needs = profiles::needs_auth_index();
    let rows: Vec<RemoteRow> = full
        .iter()
        .filter(|(n, _, _)| filter.is_empty() || n.to_lowercase().contains(&filter))
        .map(|(n, t, u)| RemoteRow {
            name: n.clone().into(),
            rtype: t.clone().into(),
            usage: u.clone().into(),
            needs_auth: *needs.get(n).unwrap_or(&false),
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
    let client = ctx.rc.client().await;
    let (hide, filter) = read_ui(ui, |u| {
        let g = u.global::<TransfersG>();
        (
            g.get_hide_done(),
            g.get_task_search().to_string().to_lowercase(),
        )
    })
    .await
    .unwrap_or((false, String::new()));
    // rclone 的 job 系统永久累积所有历史 job（包括 about/list 等内部操作），可达数万条。
    // job_list 返回全部 id 的 JSON 响应本身就很大，job_status 还会嵌套历史 job 输出，导致内存暴涨。
    // 改为完全不用 job_list/job_status：core/transferred 返回实际传输文件（group=job/xxx），
    // core/stats.transferring 提供运行中速度，两者结合即可构建任务列表。
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
    // 从 core/transferred 构建任务列表（只包含实际传输的文件，不含内部操作 job）
    for (group, (fname, src, dst, bytes)) in &tmap {
        // group 格式: "job/12345"，提取数字 id
        let id = group
            .strip_prefix("job/")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0) as i32;

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
            if !sv.fs.is_empty() && (src.starts_with(sv.fs.as_str()) || dst.starts_with(sv.fs.as_str())) {
                svc = sv.name.as_str().to_string();
                break;
            }
        }

        let running = rmap.contains_key(group);
        if hide && !running {
            continue;
        }
        let status = if running { "运行中" } else { "已完成" }.to_string();
        let speed = if running {
            rmap.get(group).map(|v| fmt_speed(*v)).unwrap_or_else(|| "—".into())
        } else if *bytes > 0.0 {
            // 无 duration 信息，无法计算历史速度
            "—".into()
        } else {
            "—".into()
        };
        rows.push(TaskRow {
            id,
            op: op.into(),
            remote: remote.into(),
            path: fname.clone().into(),
            svc: svc.into(),
            status: status.into(),
            duration: "—".into(),
            speed: speed.into(),
            done: !running,
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
    let client = ctx.rc.client().await;
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
        let val = val.trim();
        match f.kind.as_str() {
            // 布尔必须显式写出 true/false：只在 true 时插入会导致
            // 用户取消勾选后旧值仍保留在 rclone 配置里，永远无法改回 false。
            "bool" => {
                map.insert(f.name.to_string(), Value::Bool(val == "true"));
            }
            "number" => {
                if val.is_empty() {
                    // 空值：非必填则跳过（保持 rclone 默认），必填则传空串让 rclone 报错提示
                    if f.required {
                        map.insert(f.name.to_string(), Value::String(String::new()));
                    }
                } else if let Ok(n) = val.parse::<i64>() {
                    map.insert(f.name.to_string(), Value::Number(n.into()));
                } else if let Ok(x) = val.parse::<f64>() {
                    // Duration / SizeSuffix / float 等：能解析为数字就用数字
                    if let Some(n) = serde_json::Number::from_f64(x) {
                        map.insert(f.name.to_string(), Value::Number(n));
                    } else {
                        map.insert(f.name.to_string(), Value::String(val.to_string()));
                    }
                } else {
                    // "10M"、"1h30m" 这类 rclone 特有写法按字符串传
                    map.insert(f.name.to_string(), Value::String(val.to_string()));
                }
            }
            _ => {
                if val.is_empty() {
                    // 非必填空值跳过（用 rclone 默认）；必填空值照传，交由 rclone 校验报错
                    if f.required {
                        map.insert(f.name.to_string(), Value::String(String::new()));
                    }
                } else {
                    map.insert(f.name.to_string(), Value::String(val.to_string()));
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
#[allow(dead_code)]
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

/// 配置中的（远程名, 子路径）→ rclone fs（""teldrive", "a/b") → "teldrive:a/b"
fn compose_fs(remote: &str, sub_path: &str) -> String {
    let remote = remote.trim();
    let sub = sub_path.trim().trim_matches('/');
    if sub.is_empty() {
        format!("{remote}:")
    } else {
        format!("{remote}:{sub}")
    }
}

/// rclone fs（"name:sub"）→ （远程名, 子路径）
fn split_fs_str(fs: &str) -> (String, String) {
    match fs.find(':') {
        Some(i) => (fs[..i].to_string(), fs[i + 1..].trim_matches('/').to_string()),
        None => (fs.trim_end_matches(':').to_string(), String::new()),
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
        let client = ctx.rc.client().await;
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
    let client = ctx.rc.client().await;
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
    let client = ctx.rc.client().await;
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

/// build.rs 生成：真正嵌入可执行文件的内置 rclone 字节（未内置时为空切片）


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
    let cfg = config::load();

    // Slint UI
    let ui = MainWindow::new().expect("UI 初始化失败");
    apply_theme(&ui, cfg.theme_mode);
    ui.global::<SettingsG>().set_rc_user(cfg.rclone.user.clone().into());
    ui.global::<SettingsG>().set_rc_pass(cfg.rclone.pass.clone().into());
    ui.global::<SettingsG>().set_rc_url(cfg.rclone.url.clone().into());
    ui.global::<SettingsG>()
        .set_connect_on_start(cfg.rclone.connect_on_start);
    ui.global::<SettingsG>().set_theme_idx(cfg.theme_mode as i32);
    ui.global::<ThemeG>().set_accent_idx(cfg.accent_idx as i32);
    ui.global::<SettingsG>().set_accent_idx(cfg.accent_idx as i32);
    ui.global::<SettingsG>().set_autostart(get_autostart());
    ui.global::<SettingsG>().set_silent(cfg.silent_start);
    ui.global::<SettingsG>().set_http_enabled(cfg.http.enabled);
    ui.global::<SettingsG>().set_http_port(cfg.http.port.to_string().into());
    ui.global::<SettingsG>()
        .set_cfg_path(config::config_path().display().to_string().into());
    // 复选框反映**当前进程真实权限**，而不是记忆的偏好：重启后状态自然对齐
    ui.global::<SettingsG>().set_run_as_admin(is_elevated());
    ui.global::<MountsG>().set_winfsp_ok(winfsp_installed().0);
    ui.global::<MountsG>().set_winfsp_msg(winfsp_installed().1.into());
    // 远程路径历史
    set_history(&ui.as_weak(), &cfg.path_history);

    // 数据目录迁移 / 档案初始化的一次性提示（在 UI 起来之后再吐，避免丢失）
    for msg in profiles::take_notices() {
        toast(&ui.as_weak(), 1, msg);
    }

    // 内置插件释放
    embed_plugins();

    // 插件宿主
    let host = Arc::new(plugin_host::PluginHost::new().expect("wasm 引擎初始化失败"));
    let store = Arc::new(kv::KvStore::new(config::data_dir().join("plugins")));

    // 命令通道
    let (tx, mut rx) = mpsc::unbounded_channel::<Cmd>();

    // 后台 runtime
    let cfg_arc = Arc::new(std::sync::Mutex::new(cfg.clone()));
    // 连接状态与进程管理分开持有：`rc` 是 RC 调用入口，`mgr` 只负责进程生命周期。
    let rc = Arc::new(RcState::new());
    let ctx = Arc::new(Ctx {
        rc: rc.clone(),
        mgr: Arc::new(RcloneManager::new(rc)),
        host,
        store,
        cfg: cfg_arc.clone(),
        http_restart: Arc::new(tokio::sync::Notify::new()),
        remotes_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
        providers_cache: Arc::new(std::sync::Mutex::new(None)),
        mounts_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
        pending_mounts: Arc::new(std::sync::Mutex::new(Vec::new())),
        serves_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
        last_bwlimit: Arc::new(std::sync::Mutex::new(String::new())),
        tick_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        speed_meter: Arc::new(std::sync::Mutex::new(None)),
        visible: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        conn_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pending_import: Arc::new(std::sync::Mutex::new(None)),
        gate_mounts: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        gate_serves: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        gate_remotes: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        http_server: Arc::new(tokio::sync::Mutex::new(None)),
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
    // 可见性标志要在 ctx 被移交给 wire_callbacks 之前取出来
    let visible_flag = ctx.visible.clone();
    wire_callbacks(&ui, ctx, tx.clone(), search_debounce.clone());

    // 窗口可见性采样（UI 线程）：后台轮询据此暂停/恢复。
    //
    // 只采样、不做任何 RC 操作，因此可以在 UI 线程上每 500ms 跑一次；
    // 后台线程只读 AtomicBool，不需要跨线程调用 winit。
    {
        let vis = visible_flag;
        let ui_weak = ui.as_weak();
        let vis_timer = Box::leak(Box::new(slint::Timer::default()));
        vis_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(500),
            move || {
                use slint::winit_030::WinitWindowAccessor;
                let shown = ui_weak
                    .upgrade()
                    .and_then(|u| {
                        u.window()
                            .with_winit_window(|w| {
                                w.is_visible().unwrap_or(true) && !w.is_minimized().unwrap_or(false)
                            })
                    })
                    // 取不到窗口状态时按「可见」处理：宁可多轮询，也不要让界面停更
                    .unwrap_or(true);
                vis.store(shown, std::sync::atomic::Ordering::Relaxed);
            },
        );
    }

    // 窗口关闭 → 优雅退出（卸载挂载 / 停止服务 / 退出 rclone 进程）
    {
        let tx_c = tx.clone();
        ui.window().on_close_requested(move || {
            let _ = tx_c.send(Cmd::Quit);
            slint::CloseRequestResponse::HideWindow
        });
    }

    // 搜索防抖派发（Spinner 动画由 Slint 内建 animation-tick() 驱动，无需此处推进）
    {
        let tx_db = tx.clone();
        let db = search_debounce.clone();
        let debounce_timer = Box::leak(Box::new(slint::Timer::default()));
        debounce_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
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

    // 托盘（取主窗可 HWND，使左键托盘也能弹出启 退出 的菜单）
    let main_hwnd: isize = {
        use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use slint::winit_030::WinitWindowAccessor;
        ui.window()
            .with_winit_window(|w| {
                w.window_handle().ok().and_then(|h| match h.as_raw() {
                    RawWindowHandle::Win32(h) => Some(h.hwnd.get() as isize),
                    _ => None,
                })
            })
            .flatten()
            .unwrap_or(0)
    };
    spawn_tray(ui.as_weak(), tx.clone(), main_hwnd);

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

/// 单实例互斥量句柄。
///
/// 必须保存下来：用户切换「以管理员身份运行」时本进程要先释放互斥量，
/// 否则新拉起的进程会看到 ERROR_ALREADY_EXISTS 而直接退出。
static SINGLETON: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

fn acquire_single_instance() -> bool {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::HSTRING;
    unsafe {
        match CreateMutexW(None, false, &HSTRING::from("rclone-gui-singleton")) {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = windows::Win32::Foundation::CloseHandle(h);
                    false
                } else {
                    SINGLETON.store(h.0 as isize, std::sync::atomic::Ordering::SeqCst);
                    true
                }
            }
            // 创建互斥量失败时不应阻止程序启动（最坏情况是允许多开）
            Err(_) => true,
        }
    }
}

/// 释放单实例互斥量（重启自身前调用）。
fn release_single_instance() {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    let v = SINGLETON.swap(0, std::sync::atomic::Ordering::SeqCst);
    if v != 0 {
        unsafe {
            let _ = CloseHandle(HANDLE(v as *mut core::ffi::c_void));
        }
    }
}

/// 当前进程是否以提升（管理员）权限运行。
fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 按 Windows 命令行规则给参数加引号（用于 ShellExecuteW 的参数串）。
fn quote_win_arg(a: &str) -> String {
    if a.is_empty() {
        return "\"\"".into();
    }
    if !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut s = String::from("\"");
    let mut bs = 0usize;
    for ch in a.chars() {
        match ch {
            '\\' => bs += 1,
            '"' => {
                s.push_str(&"\\".repeat(bs * 2 + 1));
                s.push('"');
                bs = 0;
            }
            c => {
                s.push_str(&"\\".repeat(bs));
                bs = 0;
                s.push(c);
            }
        }
    }
    s.push_str(&"\\".repeat(bs * 2));
    s.push('"');
    s
}

/// 以目标权限重新启动自身。
///
/// - `elevate = true`：ShellExecuteW("runas") 触发 UAC，用户取消时返回错误（不会重启）。
/// - `elevate = false`：交给 explorer.exe 代启动 —— 高权限进程直接 CreateProcess 得到的
///   仍是高权限子进程，必须让运行在中等完整性级别的 explorer 来拉起才能降权。
///
/// 调用前会释放单实例互斥量；成功后调用方应立即退出本进程。
fn relaunch_self(elevate: bool) -> anyhow::Result<()> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    release_single_instance();
    if elevate {
        let file = HSTRING::from(exe.as_os_str());
        let params = HSTRING::from(
            args.iter()
                .map(|a| quote_win_arg(a))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let verb = HSTRING::from("runas");
        // 返回值 > 32 才算成功；<= 32 是错误码（常见 5 = 用户取消了 UAC）
        let code = unsafe {
            ShellExecuteW(None, &verb, &file, &params, PCWSTR::null(), SW_SHOWNORMAL).0 as isize
        };
        if code <= 32 {
            anyhow::bail!("以管理员身份重启失败（错误码 {code}，可能是取消了 UAC 提示）");
        }
    } else {
        std::process::Command::new("explorer.exe")
            .arg(&exe)
            .args(&args)
            .spawn()
            .map_err(|e| anyhow!("以普通权限重启失败: {e}"))?;
    }
    Ok(())
}

fn notify_show() {
    if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", IPC_PORT)) {
        use std::io::Write as _;
        let _ = s.write_all(b"show");
    }
}

// ---------------- 连接与握手（阶段 3） ----------------

/// 连接状态在 UI 上的投影。所有字段都可安全展示（不含密码）。
#[derive(Clone, Default)]
struct ConnView {
    status: String,
    live: bool,
    /// 曾连上、现已断开：界面展示的是上次快照
    stale: bool,
    busy: bool,
    /// 含用户名的安全展示串
    endpoint: String,
    /// 裸地址（仪表盘用）
    endpoint_url: String,
    version: String,
    config_path: String,
    last_refresh: String,
    /// 失败原因，或「已连接但有告警」的说明
    error: String,
}

struct ConnOutcome {
    view: ConnView,
    version: Option<rcgui_rc::VersionInfo>,
}

fn now_hms() -> String {
    use chrono::Timelike as _;
    let n = chrono::Local::now();
    format!("{:02}:{:02}:{:02}", n.hour(), n.minute(), n.second())
}

fn apply_conn_view(ui: &Weak<MainWindow>, v: ConnView) {
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let s = ui.global::<SettingsG>();
        s.set_conn_status(v.status.as_str().into());
        s.set_conn_live(v.live);
        s.set_conn_stale(v.stale);
        s.set_conn_busy(v.busy);
        s.set_conn_endpoint(v.endpoint.as_str().into());
        s.set_conn_version(v.version.as_str().into());
        s.set_conn_config_path(v.config_path.as_str().into());
        s.set_conn_last_refresh(v.last_refresh.as_str().into());
        s.set_conn_error(v.error.as_str().into());
        // 仪表盘复用同一份状态：running/stale 必须与设置页一致
        let d = ui.global::<DashboardG>();
        d.set_running(v.live);
        d.set_stale(v.stale);
    });
}

/// 从共享连接状态生成投影（版本/配置路径由握手结果补充）。
async fn snapshot_view(ctx: &Arc<Ctx>) -> ConnView {
    let status = ctx.rc.status().await;
    let ep = ctx.rc.endpoint().await;
    ConnView {
        status: status.label().to_string(),
        live: status.is_live(),
        stale: status == ConnStatus::Stale,
        endpoint: ep.display(),
        endpoint_url: ep.url.clone(),
        error: ctx.rc.last_error().await.unwrap_or_default(),
        last_refresh: now_hms(),
        ..Default::default()
    }
}

/// 只做握手：不启停任何进程，只验证端点并抓取版本 / 实际配置路径 / 能力表。
async fn handshake_only(ctx: &Arc<Ctx>) -> ConnOutcome {
    let client = ctx.rc.client().await;
    match Handshake::perform(&client).await {
        Ok(hs) => {
            // 握手成功才算真正「连上」（此前可能是占位客户端）
            ctx.rc.set_connected(client).await;
            // 重连后必须重新下发限速：对端可能是新实例，或状态已被重置
            ctx.last_bwlimit.lock().unwrap().clear();
            let mut view = snapshot_view(ctx).await;
            view.version = if hs.version.version.is_empty() {
                "—".to_string()
            } else {
                hs.version.version.clone()
            };
            let p = hs.config_path();
            view.config_path = if p.is_empty() { "—".to_string() } else { p };
            // 能力告警与缺失能力分开描述（不要把「能力缺失」说成「连接失败」）
            let mut notes = Vec::new();
            if !hs.is_compatible() {
                view.status = "已连接（能力受限）".to_string();
                notes.push(format!("缺少必需能力：{}", hs.missing_text()));
            }
            notes.extend(hs.warnings.iter().cloned());
            view.error = notes.join("；");
            ConnOutcome {
                view,
                version: Some(hs.version),
            }
        }
        Err(e) => {
            ctx.rc.set_disconnected(Some(e.describe())).await;
            let mut view = snapshot_view(ctx).await;
            view.error = format!("{}：{}（{}）", e.kind.label(), e.kind.hint(), e.detail);
            view.last_refresh = now_hms();
            ConnOutcome {
                view,
                version: None,
            }
        }
    }
}

/// 建立连接：**只连用户保存的 endpoint**，绝不启动 rclone 进程。
///
/// 连不上就是离线模式（保留本地档案与上次快照），不回退、不兜底启动进程。
async fn connect_report(ctx: &Arc<Ctx>) -> ConnOutcome {
    let cfg = ctx.cfg.lock().unwrap().clone();
    let attempt = ctx
        .mgr
        .attach(
            &cfg.rclone.url,
            (!cfg.rclone.user.is_empty()).then(|| cfg.rclone.user.clone()),
            (!cfg.rclone.pass.is_empty()).then(|| cfg.rclone.pass.clone()),
        )
        .await
        .map(|_| None::<String>);
    match attempt {
        Ok(_) => handshake_only(ctx).await,
        Err(e) => {
            let mut view = snapshot_view(ctx).await;
            view.error = e.to_string();
            view.last_refresh = now_hms();
            ConnOutcome {
                view,
                version: None,
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ConnAction {
    /// 建立连接（可能拉起托管进程）+ 握手
    Connect,
    /// 只重新握手，不动进程
    Handshake,
}

/// 用户触发的连接动作：置忙 → 执行 → 复位 → 应用到 UI → 分类提示。
async fn conn_action(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, which: ConnAction) {
    if ctx
        .conn_busy
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        toast(ui, 0, "正在连接，请稍候");
        return;
    }
    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<SettingsG>().set_conn_busy(true));
    let out = match which {
        ConnAction::Connect => connect_report(ctx).await,
        ConnAction::Handshake => handshake_only(ctx).await,
    };
    ctx.conn_busy
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let live = out.view.live;
    let err = out.view.error.clone();
    apply_conn_view(ui, out.view);
    if live {
        if err.is_empty() {
            toast(ui, 1, "连接成功");
        } else {
            toast(ui, 2, err);
        }
        // 连上后自动恢复上次的挂载/服务（rclone 自己不持久化 mount/serve）。
        // 放到独立线程 + 独立 runtime：conn_action 会被主循环的 Send future await，
        // 而 replay 内部要拿 std Mutex 与 Slint Weak，直接 tokio::spawn 会破坏 Send 约束。
        let ctx2 = ctx.clone();
        let ui2 = ui.clone();
        std::thread::spawn(move || {
            match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt.block_on(async move {
                    persist::replay(&ctx2, &ui2).await;
                }),
                Err(_) => {}
            }
        });
    } else {
        toast(ui, 2, format!("连接失败：{err}"));
    }
}

/// 断开连接：**不终止 rclone 进程**，只停止使用该端点并保留上次快照。
async fn disconnect_now(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    ctx.rc.set_disconnected(None).await;
    let v = snapshot_view(ctx).await;
    apply_conn_view(ui, v);
    toast(ui, 0, "已断开连接（界面保留上次快照）");
}

/// 真正的导出动作（唯一实现）。`include_secrets` 由调用方给出。
/// 直接导出时传 false；确认后导出时传入 *点击时捕获* 的值，绝不重新读取 UI。
async fn export_conf(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, path: PathBuf, include_secrets: bool) {
    // 实时状态：预设已移除，挂载/服务只能从运行中的 rclone 读出来
    let (dump, live_serves, live_mounts) = if ctx.rc.is_live() {
        let client = ctx.rc.client().await;
        let dump = client.config_dump().await.ok();
        let serves = client
            .serve_list()
            .await
            .ok()
            .map(|v| conf_io::live_serves_from(&v))
            .unwrap_or_default();
        let mounts = client
            .mounts_raw()
            .await
            .ok()
            .map(|v| conf_io::live_mounts_from(&v))
            .unwrap_or_default();
        (dump, serves, mounts)
    } else {
        (None, Vec::new(), Vec::new())
    };
    let cfg = ctx.cfg.lock().unwrap().clone();
    let providers = get_providers(ctx).await;
    let conf = conf_io::build_bundle(
        &cfg,
        dump.as_ref(),
        include_secrets,
        Some(&providers),
        &live_serves,
        &live_mounts,
    );
    let summary = conf.summary();
    match conf_io::write_bundle(&path, &conf) {
        Ok(()) => {
            let mut msg = format!(
                "已导出 {}（远程 {} · 挂载 {} · 服务 {}）",
                path.display(),
                summary.remote_count,
                summary.mount_count,
                summary.serve_count
            );
            if summary.sensitive {
                msg.push_str(&format!("；⚠ 含 {} 个远程的凭据，请妥善保管", summary.sensitives));
            }
            if summary.needs_auth > 0 {
                msg.push_str(&format!(
                    "；{} 个远程的凭据未随包导出（可能需要重新认证）",
                    summary.needs_auth
                ));
            }
            toast(ui, if summary.sensitive { 2 } else { 1 }, msg);
        }
        Err(e) => toast(ui, 2, format!("导出失败: {e}")),
    }
}

// ---------------- 后台主循环 ----------------

async fn backend_main(ctx: Arc<Ctx>, ui: Weak<MainWindow>, rx: &mut mpsc::UnboundedReceiver<Cmd>) {
    // 1) 启动时连接已保存的 endpoint（仅当用户开启「启动时连接」）。
    //    连接失败 → 离线模式；**不会**回退成启动 rclone 进程。
    let conn = if ctx.cfg.lock().unwrap().rclone.connect_on_start {
        connect_report(&ctx).await
    } else {
        let mut v = snapshot_view(&ctx).await;
        v.error = "启动时未自动连接（可在设置里点「连接 / 握手」）".to_string();
        ConnOutcome {
            view: v,
            version: None,
        }
    };
    let ready = conn.view.live;
    apply_conn_view(&ui, conn.view.clone());
    if ready {
        if !conn.view.error.is_empty() {
            // 连上了但版本/能力有告警：单独提示，不与「连接失败」混为一谈
            toast(&ui, 2, conn.view.error.clone());
        }
    } else {
        toast(&ui, 2, format!("未连接：{}", conn.view.error));
    }
    {
        let managed = false; // RCGUI 已无「托管模式」：永远连接外部实例
        let vi = conn.version.unwrap_or_default();
        let version = vi.version;
        let os_info = format!("{} / {}", vi.os, vi.arch);
        let url = conn.view.endpoint_url;
        let _ = ui.upgrade_in_event_loop(
            move |ui| {
                let g = ui.global::<DashboardG>();
                g.set_version(version.into());
                g.set_os_info(os_info.into());
                g.set_managed(managed);
                g.set_url(url.into());
            },
        );
    }

    // 1.5) 先刷新界面列表（远程 / 挂载 / 服务），保证“已连接的远程”先显示出来
    if ready {
        let _ = Box::pin(handle_cmd(&ctx, &ui, Cmd::RemotesRefresh)).await;
        let _ = Box::pin(handle_cmd(&ctx, &ui, Cmd::MountsRefresh)).await;
        let _ = Box::pin(handle_cmd(&ctx, &ui, Cmd::ServesRefresh)).await;
    }

    // 1.6) 启动流程：并发校验每个远程（≤20 并发），预热连接与目录列表；
    //      随后恢复上次记住的挂载/服务（rclone 自己不持久化 mount/serve）。
    //
    //      这里用「独立线程 + 独立 current_thread runtime」而不是 tokio::spawn：
    //      replay 需要 std::sync::Mutex 与 Slint Weak，直接 spawn 会破坏 future 的 Send 约束。
    if ready {
        let ctx2 = ctx.clone();
        let ui2 = ui.clone();
        std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async move {
                    startup_flow(&ctx2, &ui2).await;
                    persist::replay(&ctx2, &ui2).await;
                });
            }
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
    // 关键：所有耗时操作（RC 请求/网络 IO）全部 spawn 出去执行，
    // select 循环只负责收命令和派发，保证 UI 命令永远即时响应，
    // 不会因某个慢请求（如失效远程的 list 挂起）导致全部功能无响应。
    let mut tick: u64 = 0;
    let mut last_dark: Option<bool> = None;
    // 可见性状态迁移检测：不可见 → 可见时立即补一次刷新
    let mut was_visible = true;
    // 看门狗：万一周期任务异常退出未复位，30s 后强制解锁，避免轮询永久停摆
    let tick_watchdog: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(1000));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                tick += 1;
                // 轮询纪律（指南 §6）：窗口最小化/隐藏到托盘时暂停周期任务。
                // 暂停期间不产生任何 RC 请求，避免后台空转。
                if !ctx.visible.load(std::sync::atomic::Ordering::Relaxed) {
                    was_visible = false;
                    continue;
                }
                if !was_visible {
                    was_visible = true;
                    // 恢复可见：立即刷新一次，并更新时间戳
                    let ctx2 = ctx.clone();
                    let ui2 = ui.clone();
                    tokio::spawn(async move {
                        let _ = Box::pin(handle_cmd(&ctx2, &ui2, Cmd::RefreshDashboard)).await;
                        let _ = Box::pin(handle_cmd(&ctx2, &ui2, Cmd::MountsRefresh)).await;
                        let _ = Box::pin(handle_cmd(&ctx2, &ui2, Cmd::ServesRefresh)).await;
                    });
                }
                // 防重入：上一轮周期任务未完成则跳过本轮，避免 rclone 慢/挂时挂起任务无限堆积。
                {
                    let mut wd = tick_watchdog.lock().unwrap();
                    if ctx.tick_busy.load(std::sync::atomic::Ordering::SeqCst) {
                        match *wd {
                            Some(t) if t.elapsed().as_secs() >= 60 => {
                                ctx.tick_busy.store(false, std::sync::atomic::Ordering::SeqCst);
                                *wd = None;
                                eprintln!("[tick] 周期任务卡死超过 60s，已强制复位");
                            }
                            None => *wd = Some(std::time::Instant::now()),
                            Some(_) => {}
                        }
                    } else {
                        *wd = None;
                    }
                }
                // 未连接时不轮询（离线模式零 RC 请求），但主题跟随等本地逻辑继续执行。
                if ctx.rc.is_live()
                    && !ctx.tick_busy.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let ctx2 = ctx.clone();
                    let ui2 = ui.clone();
                    let t = tick;
                    tokio::spawn(async move {
                        // 守卫：panic 也能复位（否则轮询会永久停摆）。
                        let _tick_guard = TickGuard(ctx2.tick_busy.clone());
                        // 整体 30s 超时：rclone 挂起时快速放弃（drop future 即取消底层 reqwest 请求），
                        // 避免 tick_busy 被 reqwest 默认 600s 超时长期持有导致仪表盘停更
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                            // 挂载计数是较重的调用，降到 5s 一次；统计本身仍 1s 一次
                            poll_stats(&ctx2, &ui2, t % 5 == 0).await;
                            // 传输页：后台任务列表 + 全局限速（每 10s，core_transferred 响应大，不宜频繁）
                            if t % 10 == 0 {
                                build_tasks(&ctx2, &ui2).await;
                            }
                            // 限速只在配置值变化时下发（apply_global_limit 内部已判重）
                            if t % 2 == 0 {
                                apply_global_limit(&ctx2, &ui2).await;
                            }
                            // 已连接时更新「上次刷新」时间戳
                            if t % 5 == 0 && ctx2.rc.status().await.is_live() {
                                let ts = now_hms();
                                let _ = ui2.upgrade_in_event_loop(move |ui| {
                                    ui.global::<SettingsG>().set_conn_last_refresh(ts.into());
                                });
                            }
                        })
                        .await;
                        ctx2.tick_busy.store(false, std::sync::atomic::Ordering::SeqCst);
                    });
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
                // Quit 需要顺序执行清理并结束主循环，直接在循环内 await
                if matches!(cmd, Cmd::Quit) {
                    if handle_cmd(&ctx, &ui, cmd).await {
                        break;
                    }
                } else {
                    let ctx2 = ctx.clone();
                    let ui2 = ui.clone();
                    tokio::spawn(async move { handle_cmd(&ctx2, &ui2, cmd).await; });
                }
            }
            _ = ctx.http_restart.notified() => {
                let (enabled, port) = {
                    let c = ctx.cfg.lock().unwrap();
                    (c.http.enabled, c.http.port)
                };
                if enabled {
                    let ctx2 = ctx.clone();
                    let ui2 = ui.clone();
                    tokio::spawn(async move { spawn_http(&ctx2, port, ui2).await; });
                } else {
                    // 用户关闭了内嵌 HTTP：立即停止并释放端口
                    let ctx2 = ctx.clone();
                    tokio::spawn(async move { stop_http(&ctx2).await; });
                }
            }
        }
    }
    // 退出主循环：只释放连接，不触碰任何 rclone 进程
    ctx.rc.set_disconnected(None).await;
}

/// 启动流程：对每个远程预设并发执行（最大 20 并发，避免预设过多时网络拥堵）：
/// 连接(about) → 列根目录（校验+预热连接）→ 刷新文件列表 → 执行该远程的挂载/服务预设
/// 任一步骤失败的预设立即终止自身流程，不影响其他预设；全程不阻塞命令循环。
async fn startup_flow(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let client = ctx.rc.client().await;
    let remotes = client.list_remotes().await.unwrap_or_default();
    if remotes.is_empty() {
        return;
    }
    // 侧边栏"远程"图标 spinner（预热期间）
    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<NavG>().set_starting(true));

    let sem = Arc::new(tokio::sync::Semaphore::new(20));
    let mut handles = Vec::new();
    for name in remotes.iter().cloned() {
        let ctx2 = ctx.clone();
        let ui2 = ui.clone();
        let client = client.clone();
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await;
            // 1) 连接校验（失败 → 该预设流程终止）。8s 超时，防止慢后端把整个启动流程拖死
            let fs = format!("{name}:");
            let ok = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                client.about(&fs),
            )
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
            if !ok {
                return; // 预热失败静默跳过（不打扰用户）
            }
            // 2) 列根目录校验（同时预热远程连接，避免文件页首次访问被连接建立阻塞）
            let ok = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                client.list(&fs, "", None),
            )
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
            if !ok {
                return;
            }
            // 3) 文件页当前显示的正是该远程 → 刷新列表
            let (cur_remote, _) = ui_files_pos(&ui2).await;
            if cur_remote == name {
                let _ = Box::pin(handle_cmd(&ctx2, &ui2, Cmd::FilesRefresh { seq: -1 })).await;
            }

        }));
    }
    // 兜底：只等「连接校验」阶段
    let _ = tokio::time::timeout(std::time::Duration::from_secs(25), async {
        for h in handles {
            let _ = h.await;
        }
    })
    .await;
    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<NavG>().set_starting(false));
}

/// 挂载点是否已经真正出现（Windows 盘符 / 目录）。
fn mount_point_ready(point: &str) -> bool {
    let p = normalize_mount_point(point);
    Path::new(&p).exists()
}

/// 规范化挂载点用于比较：统一大写、去掉结尾反斜杠。
/// rclone 在 listmounts 里回报 "T:"，而配置里可能是 "T:\"，必须能对上。
fn mount_key(point: &str) -> String {
    normalize_mount_point(point)
        .trim_end_matches(['\\', '/'])
        .to_ascii_uppercase()
}

#[allow(dead_code)]
/// 该挂载点是否已被占用（本进程的 rclone、其他残留 rclone、或任何已存在的盘符）。
///
/// 关键：只看 `mount/listmounts` 是不够的 —— 残留的 rclone（可能在别的端口）
/// 或用户手动挂载的盘符不会出现在当前 rclone 的列表里，但盘符确实已被占用。
/// 此时再挂同一个盘符必然失败（mountpoint path already exists）。
fn mount_point_taken(point: &str, known: &[(String, String)]) -> bool {
    let key = mount_key(point);
    if known.iter().any(|(_, pt)| mount_key(pt) == key) {
        return true;
    }
    mount_point_ready(point)
}

/// RC 挂载调用的超时：慢后端（网盘挂载要先建缓存/拉元数据）可能很久。
/// 实测 teldrive 单次 `mount/mount` 阻塞约 15~30s。
const MOUNT_RC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
/// 盘符出现的最长等待时间（兜底）。
/// 实测网盘挂载耗时波动很大（15s / 24s / 27s+），因此这里给得比较宽裕，
/// 并且只要 rclone 仍认为该挂载存在，就继续等（见 `wait_mount_ready_rc`）。
const MOUNT_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 等待挂载点出现。WinFsp 的挂载是异步的：RC 返回成功时盘符可能尚未就绪，
/// 直接判定「成功」会让用户在资源管理器里找不到盘。
///
/// 返回是否在超时前出现。
async fn wait_mount_ready(point: &str, timeout: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if mount_point_ready(point) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

/// 等待挂载点出现；**只要 rclone 仍把该挂载点列为已挂载**，就把等待时间延长。
///
/// 为什么需要这个：网盘挂载耗时波动很大（实测 15~30s，慢时更久）。
/// 固定的超时必然会在某些机器/网络下把「仍在正常挂载」误报成失败。
/// rclone 的 `mount/listmounts` 是权威信号：它说还挂着，就继续等。
async fn wait_mount_ready_rc(
    client: &rclone_client::RcClient,
    point: &str,
    timeout: std::time::Duration,
) -> bool {
    let key = mount_key(point);
    let mut waited = std::time::Duration::ZERO;
    let step = std::time::Duration::from_secs(5);
    loop {
        // 每轮先做一次短等待，避免频繁轮请 RC
        if wait_mount_ready(point, step.min(timeout.saturating_sub(waited))).await {
            return true;
        }
        waited += step;
        if waited >= timeout {
            return false;
        }
        // rclone 仍报告该挂载点存在 → 挂载仍在进行，把总预算再放宽一载
        if let Ok(ms) = client.mounts().await {
            if ms.iter().any(|m| mount_key(&m.mount_point) == key) {
                waited = waited.saturating_sub(step);
            }
        }
    }
}

/// 挂载点迟迟不出现时给出可操作的诊断，而不是笼统地「请检查 WinFsp」。
///
/// 关键区分：
/// - WinFsp **未安装 / 未就绪** → 明确提示去装 WinFsp（这才是需要用户处理的）；
/// - WinFsp 正常但盘符没出现 → 说明 rclone 受理了却没挂上，通常是盘符被占用，
///   或挂载仍在后台进行（网盘首次挂载较慢）。
async fn report_mount_not_visible(ui: &Weak<MainWindow>, fs: &str, point: &str) {
    let (ok, msg) = winfsp_installed();
    if !ok {
        toast(ui, 2, format!("挂载 {point} 失败：{msg}"));
        return;
    }
    let hint = if mount_point_ready(point) {
        "该盘符已被占用，请换一个盘符"
    } else {
        "WinFsp 正常，但盘符未出现（网盘首次挂载可能较慢；也可能是盘符被占用或隐藏）"
    };
    toast(ui, 2, format!("挂载 {fs} 到 {point} 未生效：{hint}"));
}

/// 启动（或重启）内嵌 HTTP 服务。
///
/// 端口冲突（os error 10048）的根因是：旧实例的 listener 在重启时尚未释放。
/// 因此这里先向旧实例发送关闭信号并等待其任务真正结束，再绑定新端口。
async fn spawn_http(ctx: &Arc<Ctx>, port: u16, ui: Weak<MainWindow>) {
    // 1) 优雅关闭并等待旧实例退出（释放监听套接字）
    stop_http(ctx).await;

    let caps = Arc::new(kv::RealCaps {
        rc: ctx.rc.clone(),
        store: ctx.store.clone(),
    });
    let st = http::HttpState {
        host: ctx.host.clone(),
        caps,
        cfg: ctx.cfg.clone(),
    };

    // 2) 先绑定：失败时给出明确原因，且不会留下"半启动"的服务
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            let hint = if e.kind() == std::io::ErrorKind::AddrInUse {
                "端口已被其他程序占用，请在设置中改用其他端口"
            } else {
                "请检查端口设置"
            };
            toast(&ui, 2, format!("HTTP 服务启动失败（端口 {port}）：{e}；{hint}"));
            return;
        }
    };

    // 3) 启动并在 Ctx 中登记句柄，供后续重启时关闭
    let (tx_shutdown, mut rx_shutdown) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        let shutdown = async move {
            // 已收到关闭信号则立即返回；否则等待信号
            let _ = rx_shutdown.changed().await;
        };
        if let Err(e) = http::serve(st, listener, shutdown).await {
            eprintln!("[http] 服务异常退出（端口 {port}）: {e}");
        }
    });
    *ctx.http_server.lock().await = Some(HttpServer { shutdown: tx_shutdown, handle });
}

/// 停止内嵌 HTTP 服务并等待端口释放。未运行时为空操作。
async fn stop_http(ctx: &Arc<Ctx>) {
    let server = ctx.http_server.lock().await.take();
    let Some(server) = server else { return };
    let _ = server.shutdown.send(true);
    // 等待任务结束：axum 优雅关闭完成后 listener 即被释放。
    // 加超时兜底，避免异常情况下卡住重启流程。
    if tokio::time::timeout(std::time::Duration::from_secs(5), server.handle)
        .await
        .is_err()
    {
        eprintln!("[http] 旧实例关闭超时，端口可能仍被占用");
    }
}

async fn poll_stats(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, count_mounts: bool) {
    let client = ctx.rc.client().await;
    let Ok(Ok(stats)) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.core_stats(),
    )
    .await
    else {
        return;
    };
    // rclone 的 core/stats.speed 是「本次会话平均值」，空闲时几乎不动，
    // 因此用累计字节差 / 时间差算瞬时速度；真正空闲时归零。
    let idle = stats.transferring.is_empty();
    let now = std::time::Instant::now();
    let inst = {
        let mut g = ctx.speed_meter.lock().unwrap();
        let inst = match *g {
            Some((last_bytes, last_at)) => {
                let dt = now.duration_since(last_at).as_secs_f64();
                let db = (stats.bytes - last_bytes).max(0.0);
                if dt >= 0.2 { db / dt } else { 0.0 }
            }
            None => 0.0,
        };
        *g = Some((stats.bytes, now));
        inst
    };
    let speed = if idle && inst < 1.0 { 0.0 } else if inst > 0.0 { inst } else { stats.speed };
    let bytes = stats.bytes;
    let transferring = stats.transferring;
    // 挂载计数较贵（mount/listmounts 会遍历全部挂载），按调用方给的节奏抓取
    let mounts = if count_mounts {
        tokio::time::timeout(std::time::Duration::from_secs(10), client.mounts())
            .await
            .map(|r| r.map(|m| m.len()).unwrap_or(0))
            .unwrap_or(0)
    } else {
        0
    };
    let _ = ui.upgrade_in_event_loop(
        move |ui| {
            let g = ui.global::<DashboardG>();
            g.set_speed(format!("{}/s", fmt_bytes(speed)).into());
            g.set_total_bytes(fmt_bytes(bytes).into());
            g.set_transfers(transferring.len() as i32);
            if count_mounts {
                g.set_mounts(mounts as i32);
            }
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


/// 把导入的配置真正应用到**当前连接的本地 rclone**：
/// - 远程：`config/create`（不存在）或 `config/update`（已存在）；
/// - 挂载：`mount/mount`，跳过已存在的挂载点；
/// - 服务：`serve/start`。
///
/// 这是用户点「应用导入」后的显式动作，不属于静默后台行为；失败项逐条返回，
/// 由调用方汇总提示，不影响已经成功的条目。
async fn apply_bundle_to_rclone(
    ctx: &Arc<Ctx>,
    conf: &rcgui_profile::bundle::RcguiConf,
) -> (usize, usize, usize, Vec<String>) {
    let client = ctx.rc.client().await;
    let mut ok_remotes = 0usize;
    let mut ok_mounts = 0usize;
    let mut ok_serves = 0usize;
    let mut errs: Vec<String> = Vec::new();

    // 1) 远程：只改给出的参数键，未通过凭据策略导出的字段保持原值
    let existing = client.list_remotes().await.unwrap_or_default();
    for r in &conf.remotes {
        let params = serde_json::Value::Object(r.params.clone());
        let exists = existing.iter().any(|n| n == &r.name);
        let res = if exists {
            client.config_update(&r.name, params).await
        } else {
            client.config_create(&r.name, &r.rtype, params).await
        };
        match res {
            Ok(()) => ok_remotes += 1,
            Err(e) => errs.push(format!("远程 {}: {e}", r.name)),
        }
    }

    // 2) 挂载：已经挂着的盘符直接跳过，避免重复挂载报错
    let live = client.mounts().await.unwrap_or_default();
    for m in &conf.profile.mounts {
        if m.remote.trim().is_empty() {
            continue;
        }
        let point = normalize_mount_point(&m.point);
        // 无论是否已挂上，都记进档案：导入的服务/挂载本身就是用户想要的期望状态
        {
            let mut cfg = ctx.cfg.lock().unwrap();
            if let Err(e) =
                persist::remember_mount(&mut cfg, &m.remote, &m.sub_path, &point, m.ro, &m.cache)
            {
                errs.push(format!("记录挂载 {point}: {e}"));
            }
        }
        if live
            .iter()
            .any(|x| mount_key(&x.mount_point) == mount_key(&point))
        {
            ok_mounts += 1;
            continue;
        }
        let fs = compose_fs(&m.remote, &m.sub_path);
        let opts = json!({ "vfs_cache_mode": m.cache, "read_only": m.ro });
        match tokio::time::timeout(MOUNT_RC_TIMEOUT, client.mount(&fs, &point, opts)).await {
            Ok(Ok(_)) => ok_mounts += 1,
            Ok(Err(e)) => errs.push(format!("挂载 {fs} → {point}: {e}")),
            Err(_) => errs.push(format!("挂载 {fs} → {point}: 超时")),
        }
    }

    // 3) 服务：serve/start 返回 id 即视为成功
    for s in &conf.profile.serves {
        if s.remote.trim().is_empty() {
            continue;
        }
        // 同样记进档案，供下次连接自动恢复
        {
            let mut cfg = ctx.cfg.lock().unwrap();
            if let Err(e) = persist::remember_serve(
                &mut cfg, &s.stype, &s.remote, &s.sub_path, &s.addr, &s.user, &s.pass,
            ) {
                errs.push(format!("记录服务 {}: {e}", s.addr));
            }
        }
        let fs = compose_fs(&s.remote, &s.sub_path);
        match client
            .serve_start(&s.stype, &fs, &s.addr, &s.user, &s.pass)
            .await
        {
            Ok(_) => ok_serves += 1,
            Err(e) => errs.push(format!("服务 {} {}: {e}", s.stype, s.addr)),
        }
    }

    (ok_remotes, ok_mounts, ok_serves, errs)
}

/// 返回 true 表示退出
async fn handle_cmd(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, cmd: Cmd) -> bool {
    match cmd {
        Cmd::Quit => {
            // 关闭 GUI 只做三件事：停止轮询、释放连接、退出事件循环。
            // **不**调用 core/quit、**不**杀 rclone、**不**卸载挂载、**不**停止 serve：
            // rclone 的生命周期与运行中的挂载/服务都由用户自己掌握。
            stop_http(ctx).await;
            ctx.rc.set_disconnected(None).await;
            let _ = slint::quit_event_loop();
            true
        }
        Cmd::RefreshDashboard => {
            // 离线/已断开：保留上次快照，不用失败结果覆盖界面
            if !ctx.rc.is_live() {
                return false;
            }
            let client = ctx.rc.client().await;
            let ver_info = client.version().await.unwrap_or_default();
            let version = ver_info.version;
            let os_info = format!("{} / {}", ver_info.os, ver_info.arch);
            let remotes = client.list_remotes().await.map(|r| r.len()).unwrap_or(0);
            let errs = client.core_stats().await.map(|s| s.errors).unwrap_or(0);
            let managed = false; // RCGUI 已无「托管模式」：永远连接外部实例
            let url = ctx.cfg.lock().unwrap().rclone.url.clone();
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
            let client = ctx.rc.client().await;
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
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                client.list(&fs, &path, None),
            )
            .await
            .map_err(|_| anyhow!("请求超时"))
            .and_then(|r| r.map_err(anyhow::Error::from));
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
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                client.list(&fs, &target, None),
            )
            .await
            .map_err(|_| anyhow!("请求超时"))
            .and_then(|r| r.map_err(anyhow::Error::from));
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
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                client.list(&fs, &parent, None),
            )
            .await
            .map_err(|_| anyhow!("请求超时"))
            .and_then(|r| r.map_err(anyhow::Error::from));
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
            let client = ctx.rc.client().await;
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
            let client = ctx.rc.client().await;
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
            let client = ctx.rc.client().await;
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
            let client = ctx.rc.client().await;
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
            let client = ctx.rc.client().await;
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
            // 离线/已断开：保留上次快照（指南 §3），只标记陈旧，不清空列表
            if !ctx.rc.is_live() {
                return false;
            }
            // 单飞：已有同类刷新在跑就丢弃本次，避免重叠请求与「旧响应覆盖新状态」
            if ctx.gate_mounts.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            let _gate = GateGuard(ctx.gate_mounts.clone());
            let client = ctx.rc.client().await;
            let mounts = client.mounts().await.unwrap_or_default();
            let mut rows: Vec<MountRow> = mounts
                .iter()
                .map(|m| MountRow {
                    fs: m.fs.clone().into(),
                    mount_point: m.mount_point.clone().into(),
                    pending: false,
                })
                .collect();
            // 合并“正在挂载中”的条目；已被真实列表收录的则自动摘掉。
            // 必须用 mount_key 归一化比较：pending 里存的是 "T:\"（规范化后的配置值），
            // 而 rclone 的 listmounts 回报的是 "T:"。直接字符串比较永远不相等，
            // 结果就是同一个盘符在界面上出现两条（一条 pending + 一条真实）。
            {
                let mut pending = ctx.pending_mounts.lock().unwrap();
                pending.retain(|(_, pt)| !rows.iter().any(|r| mount_key(&r.mount_point) == mount_key(pt)));
                for (fs, pt) in pending.iter() {
                    if !rows.iter().any(|r| mount_key(&r.mount_point) == mount_key(pt)) {
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
            let (remote, sub_path, point_raw, ro, cache) = read_ui(ui, |ui| {
                let m = ui.global::<MountsG>();
                let remote = m
                    .get_remotes()
                    .row_data(m.get_remote_idx().max(0) as usize)
                    .unwrap_or_default()
                    .to_string();
                let point = m.get_new_point().to_string();
                let ro = m.get_ro();
                let cache = m
                    .get_caches()
                    .row_data(m.get_cache_idx().max(0) as usize)
                    .unwrap_or("minimal".into())
                    .to_string();
                (
                    remote,
                    m.get_sub_path().to_string(),
                    point,
                    ro,
                    cache,
                )
            })
            .await
            .unwrap_or_default();
            if remote.is_empty() {
                toast(ui, 2, "请先在「远程」页添加仓库，再选择仓库");
                return false;
            }
            let fs = compose_fs(&remote, &sub_path);
            let point = normalize_mount_point(&point_raw);
            // 盘符 / 挂载点重复检测（含正在挂载中的条目）。
            // 必须用 mount_key 归一化比较：rclone 回报 "T:"，而配置/输入可能是 "T:\"，
            // 直接字符串比较会漏判，导致对已占用的盘符再次发起挂载。
            if point != "*" {
                let dup = ctx
                    .mounts_cache
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| mount_key(&r.mount_point) == mount_key(&point));
                if dup {
                    toast(ui, 2, format!("挂载点 {point} 已被占用，请修改盘符"));
                    return false;
                }
            }
            let client = ctx.rc.client().await;
            let opts = json!({
                "vfs_cache_mode": cache,
                "read_only": ro,
            });
            // 立即在“活动挂载”里插入一条 pending（spinner + 高亮）
            ctx.pending_mounts.lock().unwrap().push((fs.clone(), point.clone()));
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<MountsG>().set_busy(true));
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
            // 给足时间：RC 调用会阻塞到挂载完成（网盘首次挂载可能 15~30s 甚至更久）
            let res = tokio::time::timeout(MOUNT_RC_TIMEOUT, client.mount(&fs, &point, opts))
                .await
                .map_err(|_| anyhow!("挂载超时（{}s）", MOUNT_RC_TIMEOUT.as_secs()))
                .and_then(|r| r.map_err(anyhow::Error::from));
            // 无论成败都先摘掉 pending
            ctx.pending_mounts.lock().unwrap().retain(|(_, p)| p != &point);
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<MountsG>().set_busy(false));
            // rclone 的 mount/mount 返回成功只代表「已受理」，WinFsp 真正挂上盘符是异步的。
            // 必须确认挂载点真的出现，否则会出现「提示挂载成功但资源管理器里没有盘」。
            let res = match res {
                Ok(actual) => {
                    if wait_mount_ready_rc(&client, &actual, MOUNT_READY_TIMEOUT).await {
                        Ok(actual)
                    } else {
                        // 交给统一的诊断，区分「没装 WinFsp」与「盘符被占/仍在挂载」
                        report_mount_not_visible(ui, &fs, &actual).await;
                        return false;
                    }
                }
                Err(e) => {
                    // 报错但盘符其实已经挂上：按成功处理，避免误导
                    if wait_mount_ready_rc(&client, &point, MOUNT_READY_TIMEOUT).await {
                        Ok(point.clone())
                    } else {
                        Err(e)
                    }
                }
            };
            match res {
                Ok(actual) => {
                    // 记住这次挂载：下次连接成功会自动重建（rclone 自己不持久化 mount）
                    {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        if let Err(e) = persist::remember_mount(
                            &mut cfg,
                            &remote,
                            &sub_path,
                            &actual,
                            ro,
                            &cache,
                        ) {
                            toast(ui, 2, format!("{e}"));
                        }
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
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
            let client = ctx.rc.client().await;
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
                    // 用户主动卸载 → 忘掉这条记忆，避免下次连接又挂回来
                    {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        persist::forget_mount(&mut cfg, &point);
                        let _ = config::save(&cfg);
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    toast(ui, 1, format!("已卸载 {point}"));
                }
                Err(e) => toast(ui, 2, format!("卸载失败: {e}")),
            }
            false
        }
        Cmd::HistPick { scope, path } => {
            let (remote, sub) = split_display(&path)
                .map(|(fs, sub)| (fs.trim_end_matches(':').to_string(), sub))
                .unwrap_or_default();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                if scope == 0 {
                    let g = ui.global::<MountsG>();
                    let model = g.get_remotes();
                    for i in 0..model.row_count() {
                        if model.row_data(i).as_deref() == Some(remote.as_str()) {
                            g.set_remote_idx(i as i32);
                            break;
                        }
                    }
                    g.set_sub_path(sub.into());
                } else {
                    let g = ui.global::<ServesG>();
                    let model = g.get_remotes();
                    for i in 0..model.row_count() {
                        if model.row_data(i).as_deref() == Some(remote.as_str()) {
                            g.set_remote_idx(i as i32);
                            break;
                        }
                    }
                    g.set_sub_path(sub.into());
                }
            });
            false
        }
        Cmd::BrowseOpen { scope } => {
            let cur = read_ui(ui, move |ui| {
                // 由「仓应 + 子路径」拼出当前显示路径 root://remote/sub
                if scope == 0 {
                    let g = ui.global::<MountsG>();
                    let r = g
                        .get_remotes()
                        .row_data(g.get_remote_idx().max(0) as usize)
                        .unwrap_or_default()
                        .to_string();
                    let sub = g.get_sub_path().to_string();
                    if r.is_empty() {
                        String::new()
                    } else {
                        format!("root://{}/{}", r, sub.trim_matches('/'))
                    }
                } else {
                    let g = ui.global::<ServesG>();
                    let r = g
                        .get_remotes()
                        .row_data(g.get_remote_idx().max(0) as usize)
                        .unwrap_or_default()
                        .to_string();
                    let sub = g.get_sub_path().to_string();
                    if r.is_empty() {
                        String::new()
                    } else {
                        format!("root://{}/{}", r, sub.trim_matches('/'))
                    }
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
                // root://name/sub → 定位仓库下拉须 + 填子路径
                let (remote, sub) = split_display(&final_disp)
                    .map(|(fs, sub)| (fs.trim_end_matches(':').to_string(), sub))
                    .unwrap_or_default();
                if scope == 0 {
                    let g = ui.global::<MountsG>();
                    let model = g.get_remotes();
                    let mut idx = 0;
                    for i in 0..model.row_count() {
                        if model.row_data(i).as_deref() == Some(remote.as_str()) {
                            idx = i as i32;
                            break;
                        }
                    }
                    g.set_remote_idx(idx);
                    g.set_sub_path(sub.into());
                } else {
                    let g = ui.global::<ServesG>();
                    let model = g.get_remotes();
                    let mut idx = 0;
                    for i in 0..model.row_count() {
                        if model.row_data(i).as_deref() == Some(remote.as_str()) {
                            idx = i as i32;
                            break;
                        }
                    }
                    g.set_remote_idx(idx);
                    g.set_sub_path(sub.into());
                }
                ui.global::<BrowseG>().set_show(false);
            });
            false
        }
        Cmd::RemotesRefresh => {
            if !ctx.rc.is_live() {
                return false;
            }
            if ctx.gate_remotes.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            let _gate = GateGuard(ctx.gate_remotes.clone());
            let client = ctx.rc.client().await;
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
                        .unwrap_or_else(|| "—".into());
                    full.push((n.clone(), ty, usage));
                }
            }
            *ctx.remotes_cache.lock().unwrap() = full.clone();
            // 本地副本（仅名字/类型/用量，不含凭据）：离线时也能看到「有哪些远程」
            profiles::set_remotes(&full);
            apply_remotes_filter(ui, &full).await;
            // 同步仓库下拉框（挂载须 服务页）与挂载、服务预设列行
            let names_ui: Vec<slint::SharedString> =
                full.iter().map(|(n, _, _)| n.as_str().into()).collect();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let m = ModelRc::from(Rc::new(VecModel::from(names_ui)));
                ui.global::<MountsG>().set_remotes(m.clone());
                ui.global::<ServesG>().set_remotes(m);
            });
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
            let client = ctx.rc.client().await;
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
            let client = ctx.rc.client().await;
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
            let client = ctx.rc.client().await;
            match client.config_delete(&name).await {
                Ok(()) => {
                    // 级联清理：该远程下记住的挂载/服务一并忘记（否则重放会一直失败）
                    let (drop_points, drop_addrs) = {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        let out = persist::forget_remote(&mut cfg, &name);
                        let _ = config::save(&cfg);
                        out
                    };
                    // 停止该远程正在活动的挂载/服务（失败静默，rclone 远程已删）
                    for p in &drop_points {
                        let _ = client.unmount(p).await;
                    }
                    {
                        let running = ctx.serves_cache.lock().unwrap().clone();
                        for row in running.iter() {
                            let (rname, _) = split_fs_str(&row.fs);
                            if rname == name || drop_addrs.iter().any(|a| a == &row.addr.as_str()) {
                                let _ = client.serve_stop(&row.id).await;
                            }
                        }
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::RemotesRefresh)).await;
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ServesRefresh)).await;
                    toast(ui, 1, format!("已删除 {name} 及其挂载/服务配置"));
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
            if !ctx.rc.is_live() {
                return false;
            }
            if ctx.gate_serves.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            let _gate = GateGuard(ctx.gate_serves.clone());
            let client = ctx.rc.client().await;
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
            let (stype, remote, sub_path, addr, user, pass, svc_name) =
                read_ui(ui, |ui| {
                    let g = ui.global::<ServesG>();
                    let ty = g
                        .get_types()
                        .row_data(g.get_type_idx().max(0) as usize)
                        .unwrap_or_default()
                        .to_string();
                    let remote = g
                        .get_remotes()
                        .row_data(g.get_remote_idx().max(0) as usize)
                        .unwrap_or_default()
                        .to_string();
                    (
                        ty,
                        remote,
                        g.get_sub_path().to_string(),
                        g.get_addr().to_string(),
                        g.get_user().to_string(),
                        g.get_pass().to_string(),
                        g.get_svc_name().to_string(),
                    )
                })
                .await
                .unwrap_or_default();
            if remote.is_empty() {
                toast(ui, 2, "请先在「远程」页添加仓库，再选择仓库");
                return false;
            }
            let sub_path = sub_path.trim().trim_matches('/').to_string();
            let fs = compose_fs(&remote, &sub_path);
            let addr_t = addr.trim().to_string();
            if addr_t.is_empty() {
                toast(ui, 2, "请填写监听地址");
                return false;
            }
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(true));
            let client = ctx.rc.client().await;
            let res = client
                .serve_start(&stype, &fs, &addr_t, &user, &pass)
                .await;
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(false));
            match res {
                Ok(id) => {
                    let mut saved_err = None;
                    {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        // 服务名称：优先用用户输入；否则复用已有映射；再否则生成默认 Serve_NN
                        let svc_name = svc_name.trim().to_string();
                        let name = if !svc_name.is_empty() {
                            svc_name
                        } else if let Some(s) = cfg
                            .serve_names
                            .iter()
                            .find(|s| s.stype == stype && s.addr == addr_t)
                        {
                            s.name.clone()
                        } else {
                            next_serve_name(&cfg)
                        };
                        persist::remember_serve_name(
                            &mut cfg,
                            &stype,
                            &remote,
                            &sub_path,
                            &addr_t,
                            &name,
                        );
                        // 记住这次服务：下次连接成功会自动重建（rclone 自己不持久化 serve）
                        if let Err(e) = persist::remember_serve(
                            &mut cfg, &stype, &remote, &sub_path, &addr_t, &user, &pass,
                        ) {
                            saved_err = Some(e.to_string());
                        }
                    }
                    if let Some(e) = saved_err {
                        toast(ui, 2, e);
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ServesRefresh)).await;
                    toast(ui, 1, format!("服务已启动: {id}"));
                }
                Err(e) => toast(ui, 2, format!("启动失败: {e}")),
            }
            false
        }
        Cmd::ServeStop { id } => {
            // 停止前先从缓存拿到地址（停止后 serve_list 里就查不到了），
            // 用于「停止即忘记」：不记下来下次连接会把服务又自动起回来。
            let addr_of_id = ctx
                .serves_cache
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id.as_str() == id.as_str())
                .map(|r| r.addr.to_string());
            let client = ctx.rc.client().await;
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(true));
            let res = client.serve_stop(&id).await;
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(false));
            match res {
                Ok(()) => {
                    if let Some(addr) = addr_of_id {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        persist::forget_serve(&mut cfg, &addr);
                        let _ = config::save(&cfg);
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
            let client = ctx.rc.client().await;
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
            let client = ctx.rc.client().await;
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
            let client = ctx.rc.client().await;
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
        Cmd::PluginAddFromFile => {
            plugin_add_from_file(ctx, ui).await;
            false
        }
        Cmd::PluginAddFromDir => {
            plugin_add_from_dir(ctx, ui).await;
            false
        }
        Cmd::PluginOpenDir => {
            let d = plugins_dir();
            let _ = std::fs::create_dir_all(&d);
            // explorer.exe 打不开时退 cmd start；两者都失败才报错，并总是回显路径便于排查
            let mut opened = std::process::Command::new("explorer.exe").arg(&d).spawn().is_ok();
            if !opened {
                opened = std::process::Command::new("cmd.exe")
                    .args(["/c", "start", "", d.to_string_lossy().as_ref()])
                    .spawn()
                    .is_ok();
            }
            toast(
                ui,
                if opened { 1 } else { 2 },
                format!(
                    "{}插件目录：{}",
                    if opened { "已请求打开" } else { "无法打开" },
                    d.display()
                ),
            );
            false
        }
        Cmd::PluginToggle { idx } => {
            let row = read_ui(ui, move |ui| {
                ui.global::<PluginsG>()
                    .get_rows()
                    .row_data(idx as usize)
                    .map(|r| {
                        (
                            r.id.to_string(),
                            r.enabled,
                            r.kind.to_string(),
                            r.verified,
                            r.path.to_string(),
                        )
                    })
            })
            .await
            .flatten();
            if let Some((id, old_enabled, kind, verified, path)) = row {
                // 未通过完整性校验的插件包不允许启用
                if !old_enabled && kind == "package" && !verified {
                    toast(ui, 2, format!("插件 {id} 未通过完整性校验，无法启用"));
                    return false;
                }
                let enabled = !old_enabled;

                // 插件包：启用 = 执行 entry_file（例如安装 WinFsp 的 msi）
                if enabled && kind == "package" {
                    let dir = PathBuf::from(&path);
                    let meta = match read_plugin_json_lenient(&dir) {
                        Some(m) => m,
                        None => {
                            toast(ui, 2, format!("插件 {id} 的 plugin.json 无法读取"));
                            return false;
                        }
                    };
                    // 再次校验，避免文件在校验之后被替换（TOCTOU）
                    if let Err(e) =
                        plugin_host::package::load_from_dir(&dir, &gui_version())
                    {
                        toast(ui, 2, format!("插件 {id} 校验失败: {e:#}"));
                        return false;
                    }
                    toast(ui, 1, format!("正在执行插件 {id} 的入口文件…"));
                    let r = tokio::task::spawn_blocking(move || {
                        plugin_host::package::run_entry_file(&dir, &meta)
                    })
                    .await;
                    match r {
                        Ok(Ok(0)) => toast(ui, 1, format!("插件 {id} 已启用（入口文件执行成功）")),
                        Ok(Ok(code)) => toast(
                            ui,
                            2,
                            format!("插件 {id} 入口文件退出码 {code}（可能未成功，请检查）"),
                        ),
                        Ok(Err(e)) => {
                            toast(ui, 2, format!("插件 {id} 启动失败: {e:#}"));
                            return false; // 执行失败不记录为已启用
                        }
                        Err(e) => {
                            toast(ui, 2, format!("插件 {id} 执行线程异常: {e}"));
                            return false;
                        }
                    }
                }

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
            let client = ctx.rc.client().await;
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
            match invoke_plugin(ctx, "imagehost", params).await
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
            match invoke_plugin(ctx, "imagehost", params).await
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
            match invoke_plugin(ctx, "imagehost", params).await
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
            // 读取默认根 + 回收站设置：插件仅在 toTrash=true 且已配置 trash_dir 时才移入回收站，
            // 否则直接删除。这里按实际配置决定提示文案，避免"已删除（回收站）"与实际不符。
            let (root_id, has_trash) = read_ui(ui, |ui| {
                let g = ui.global::<PluginsG>();
                (
                    g.get_roots()
                        .row_data(0)
                        .map(|r| r.id.to_string())
                        .unwrap_or_default(),
                    !g.get_trash_dir().trim().is_empty(),
                )
            })
            .await
            .unwrap_or_default();
            let body = json!({"rootId": root_id, "paths": [path], "toTrash": has_trash});
            let params = plugin_http_req("POST", "/files/delete", Some(&body), json!({}));
            match invoke_plugin(ctx, "imagehost", params).await
            {
                Ok(_) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::IhImagesRefresh)).await;
                    if has_trash {
                        toast(ui, 1, "已移入回收站");
                    } else {
                        toast(ui, 1, "已永久删除（未配置回收站目录）");
                    }
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
            // 记录旧 HTTP 配置：只有它变化时才重启内嵌服务，
            // 否则每次保存（含切换主题/强调色）都会重启服务并可能撞上端口未释放（10048）
            let old_http = {
                let c = ctx.cfg.lock().unwrap();
                (c.http.enabled, c.http.port)
            };
            let save_result = read_ui(ui, {
                let ctx = ctx.clone();
                move |ui| -> Result<(), String> {
                    let s = ui.global::<SettingsG>();
                    let mut cfg = ctx.cfg.lock().unwrap().clone();
                    cfg.rclone.user = s.get_rc_user().to_string();
                    cfg.rclone.pass = s.get_rc_pass().to_string();
                    let url = s.get_rc_url().trim().to_string();
                    if !url.is_empty() {
                        cfg.rclone.url = url;
                    }
                    cfg.rclone.connect_on_start = s.get_connect_on_start();
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
            // 仅当内嵌 HTTP 的启用状态或端口真正变化时才重启服务
            let new_http = {
                let c = ctx.cfg.lock().unwrap();
                (c.http.enabled, c.http.port)
            };
            if new_http != old_http {
                ctx.http_restart.notify_one();
            }
            false
        }
        Cmd::SettingsTest => {
            // 测试连接 = 完整握手，并把结果写回连接状态区（不再只报「连接正常」）
            conn_action(ctx, ui, ConnAction::Handshake).await;
            false
        }
        Cmd::Connect => {
            // 先把设置页上的连接字段落盘，否则「改完直接点连接」会用到旧值
            let saved = read_ui(ui, {
                let ctx = ctx.clone();
                move |ui| -> Result<(), String> {
                    let s = ui.global::<SettingsG>();
                    let mut cfg = ctx.cfg.lock().unwrap().clone();
                    cfg.rclone.user = s.get_rc_user().to_string();
                    cfg.rclone.pass = s.get_rc_pass().to_string();
                    let url = s.get_rc_url().trim().to_string();
                    if !url.is_empty() {
                        cfg.rclone.url = url;
                    }
                    cfg.rclone.connect_on_start = s.get_connect_on_start();
                    *ctx.cfg.lock().unwrap() = cfg.clone();
                    config::save(&cfg).map_err(|e| e.to_string())
                }
            })
            .await;
            if let Some(Err(e)) = saved {
                toast(ui, 2, format!("保存连接设置失败: {e}"));
                return false;
            }
            conn_action(ctx, ui, ConnAction::Connect).await;
            false
        }
        Cmd::Disconnect => {
            disconnect_now(ctx, ui).await;
            false
        }
        Cmd::Reconnect => {
            conn_action(ctx, ui, ConnAction::Handshake).await;
            false
        }
        Cmd::ConfExport { path, include_secrets } => {
            // include_secrets 来自点击时捕获的值（true 只可能来自确认后的回调）
            export_conf(ctx, ui, path, include_secrets).await;
            false
        }
        Cmd::ConfPreview { path } => {
            let cfg = ctx.cfg.lock().unwrap().clone();
            let current = profiles::current_doc(&cfg);
            match conf_io::preview_file(&path, &current) {
                Ok((conf, preview)) => {
                    let info = conf_io::describe(&preview);
                    *ctx.pending_import.lock().unwrap() = Some(conf);
                    let info2 = info.clone();
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        let g = ui.global::<SettingsG>();
                        g.set_conf_pending(true);
                        g.set_conf_info(info2.into());
                    });
                    toast(ui, 1, format!("预览完成（尚未写入）：{info}"));
                }
                Err(e) => {
                    *ctx.pending_import.lock().unwrap() = None;
                    let _ = ui.upgrade_in_event_loop(|ui| {
                        let g = ui.global::<SettingsG>();
                        g.set_conf_pending(false);
                        g.set_conf_info("导入预览失败，详见提示".into());
                    });
                    toast(ui, 2, format!("导入预览失败: {e}"));
                }
            }
            false
        }
        Cmd::ScriptGenerate => {
            let cfg = ctx.cfg.lock().unwrap().clone();
            match script::generate_and_save(&cfg) {
                Ok((path, body)) => {
                    let p = path.display().to_string();
                    let b = body.clone();
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        let g = ui.global::<SettingsG>();
                        g.set_script_path(p.into());
                        g.set_script_preview(b.into());
                    });
                    toast(ui, 1, format!("已生成恢复脚本（未执行）：{}", path.display()));
                }
                Err(e) => toast(ui, 2, format!("生成恢复脚本失败: {e}")),
            }
            false
        }
        Cmd::ScriptCopy => {
            let text = read_ui(ui, |u| {
                u.global::<SettingsG>().get_script_preview().to_string()
            })
            .await
            .unwrap_or_default();
            if text.is_empty() {
                toast(ui, 0, "还没有可复制的脚本，请先「生成恢复脚本」");
            } else {
                clipboard_copy(&text);
                toast(ui, 1, "脚本已复制到剪贴板");
            }
            false
        }
        Cmd::AdminToggle => {
            let target = read_ui(ui, |u| u.global::<SettingsG>().get_run_as_admin())
                .await
                .unwrap_or(false);
            let current = is_elevated();
            if target == current {
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    ui.global::<SettingsG>().set_run_as_admin(current)
                });
                toast(ui, 0, "当前已是该权限，无需重启");
                return false;
            }
            let msg = if target {
                "软件将重启，并以管理员身份运行。若弹出 UAC 提示，请选择「是」。"
            } else {
                "软件将重启，并以普通权限运行。"
            };
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<SettingsG>();
                g.set_admin_confirm_msg(msg.into());
                g.set_admin_confirm_show(true);
            });
            false
        }
        Cmd::AdminRestart => {
            // 目标状态来自 UI 复选框（用户点「确定」后仍以它为准）
            let target = read_ui(ui, |u| u.global::<SettingsG>().get_run_as_admin())
                .await
                .unwrap_or(false);
            let current = is_elevated();
            if target == current {
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    ui.global::<SettingsG>().set_run_as_admin(current)
                });
                return false;
            }
            match relaunch_self(target) {
                Ok(()) => {
                    // 新实例已拉起：本进程立即退出，避免两个实例同时轮询同一个 endpoint。
                    // 只停止轮询/释放连接，不碰 rclone 进程。
                    let _ = slint::quit_event_loop();
                    std::process::exit(0);
                }
                Err(e) => {
                    // 失败（典型：用户取消了 UAC）：复选框回到真实状态，不做任何重启
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        let g = ui.global::<SettingsG>();
                        g.set_admin_confirm_show(false);
                        g.set_run_as_admin(current);
                    });
                    toast(ui, 2, format!("{e}"));
                }
            }
            false
        }
        Cmd::AdminCancel => {
            let current = is_elevated();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<SettingsG>();
                g.set_admin_confirm_show(false);
                g.set_run_as_admin(current);
            });
            toast(ui, 0, "已取消，保持当前权限");
            false
        }
        Cmd::ConfApply => {
            let Some(conf) = ctx.pending_import.lock().unwrap().take() else {
                toast(ui, 0, "请先「导入预览」，确认无误后再应用");
                return false;
            };
            let mut cfg = ctx.cfg.lock().unwrap().clone();
            match conf_io::apply(&conf, &mut cfg) {
                Ok(bak) => {
                    *ctx.cfg.lock().unwrap() = cfg.clone();
                    // 写入档案后，再通过 RC 把远程/挂载/服务真正应用到本地 rclone
                    let (ok_r, ok_m, ok_s, errs) = apply_bundle_to_rclone(ctx, &conf).await;
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::RemotesRefresh)).await;
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ServesRefresh)).await;
                    let mut info = format!(
                        "已应用到本地 rclone：远程 {ok_r} / 挂载 {ok_m} / 服务 {ok_s}；原档案备份在 {}",
                        bak.display()
                    );
                    if !errs.is_empty() {
                        info.push_str(&format!("；失败 {} 项：{}", errs.len(), errs.join("；")));
                    }
                    let info2 = info.clone();
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        let g = ui.global::<SettingsG>();
                        g.set_conf_pending(false);
                        g.set_conf_info(info2.into());
                    });
                    toast(ui, if errs.is_empty() { 1 } else { 2 }, info);
                }
                Err(e) => {
                    // 失败时放回待应用状态，允许用户处理后重试
                    *ctx.pending_import.lock().unwrap() = Some(conf);
                    toast(ui, 2, format!("应用导入失败（数据未改动）: {e}"));
                }
            }
            false
        }
    }
}

// ---------------- 图床配置 ----------------

/// 插件根目录（插件包与 WASM 插件共用）
fn plugins_dir() -> PathBuf {
    config::data_dir().join("plugins")
}

/// 插件包解压用的临时目录
fn plugins_tmp_dir() -> PathBuf {
    config::data_dir().join("tmp")
}

/// 应用自身版本（用于插件包 min_gui_version 校验）
fn gui_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

async fn rescan_plugins(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let mut rows: Vec<PluginRow> = Vec::new();
    let mut dirs = vec![plugins_dir()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.join("assets/plugins"));
        }
    }
    let enabled_map = ctx.cfg.lock().unwrap().plugin_enabled.clone();

    // 1) WASM 插件（单文件 .wasm）
    for dir in &dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("wasm") {
                if let Ok(m) = ctx.host.load_manifest(&p).await {
                    let enabled = enabled_map.get(&m.id).copied().unwrap_or(true);
                    let has_config = m.id == "imagehost";
                    rows.push(PluginRow {
                        id: m.id.clone().into(),
                        name: m.name.clone().into(),
                        version: m.version.into(),
                        desc: m.description.into(),
                        loaded: true,
                        enabled,
                        kind: "wasm".into(),
                        author: "".into(),
                        entry: "".into(),
                        verified: true,
                        has_config,
                        status: "".into(),
                        path: p.display().to_string().into(),
                    });
                }
            }
        }
    }

    // 2) 插件包（目录形态，含 plugin.json）
    //    完整性校验失败或版本不兼容的包也会列出，但标记为不可用、不可启用，
    //    这样用户能看到"为什么我的插件没生效"。
    let gv = gui_version();
    let root = plugins_dir();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("plugin.json").is_file() {
                continue;
            }
            let (meta, verified, status) = match plugin_host::package::load_from_dir(&dir, &gv) {
                Ok((m, _)) => (m, true, String::new()),
                Err(e) => {
                    // 校验失败：尽量读出元数据用于展示
                    match read_plugin_json_lenient(&dir) {
                        Some(m) => (m, false, format!("{e:#}")),
                        None => continue, // 连 plugin.json 都读不出，跳过
                    }
                }
            };
            let enabled = enabled_map.get(&meta.id).copied().unwrap_or(false);
            rows.push(PluginRow {
                id: meta.id.clone().into(),
                name: meta.name.clone().into(),
                version: meta.version.clone().into(),
                desc: meta.description.clone().into(),
                loaded: verified,
                // 未通过校验的包强制视为未启用
                enabled: enabled && verified,
                kind: "package".into(),
                author: meta.author.clone().into(),
                entry: meta.entry_file.clone().into(),
                verified,
                has_config: false,
                status: status.into(),
                path: dir.display().to_string().into(),
            });
        }
    }

    let imagehost = rows.iter().any(|r| r.id == "imagehost");
    let dir_text = plugins_dir().display().to_string();
    let _ = ui.upgrade_in_event_loop(
        move |ui| {
            let g = ui.global::<PluginsG>();
            g.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            g.set_loaded(imagehost);
            g.set_dir(dir_text.into());
        },
    );
    if imagehost {
        load_ih_config(ctx, ui).await;
        let _ = Box::pin(handle_cmd(ctx, ui, Cmd::IhImagesRefresh)).await;
    }
}

/// 宽松读取 plugin.json（即使 manifest 校验失败，也能展示插件名/版本）
fn read_plugin_json_lenient(dir: &Path) -> Option<plugin_host::package::PluginJson> {
    let text = std::fs::read_to_string(dir.join("plugin.json")).ok()?;
    serde_json::from_str::<plugin_host::package::PluginJson>(&text).ok()
}

/// 选择插件文件（zip）并安装。
///
/// 流程（按需求）：选择目标 → 判定文件大小 → 确认包内有 plugin.json →
/// 解压到临时目录 → 读 id/version → 新 id 则校验完整性后添加；
/// 已存在则校验完整性，版本更高才替换，否则不替换 → 最后刷新界面。
async fn plugin_add_from_file(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let Some(path) = rfd::FileDialog::new()
        .set_title("选择插件包（.zip）")
        .add_filter("插件包", &["zip"])
        .pick_file()
    else {
        return;
    };

    // 1) 文件大小判定（太小/太大都说明选错了目标）
    let size = match std::fs::metadata(&path) {
        Ok(m) => m.len(),
        Err(e) => {
            toast(ui, 2, format!("无法读取文件信息: {e}"));
            return;
        }
    };
    if let Err(e) = plugin_host::package::check_source_size(size) {
        toast(ui, 2, format!("{e}"));
        return;
    }

    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            toast(ui, 2, format!("读取文件失败: {e}"));
            return;
        }
    };
    let root = plugins_dir();
    let tmp = plugins_tmp_dir();
    let gv = gui_version();
    let res = tokio::task::spawn_blocking(move || {
        plugin_host::package::install_from_zip(&data, &root, &tmp, &gv)
    })
    .await;

    match res {
        Ok(Ok(outcome)) => {
            toast(ui, 1, outcome.message());
            rescan_plugins(ctx, ui).await;
        }
        Ok(Err(e)) => toast(ui, 2, format!("添加插件失败: {e:#}")),
        Err(e) => toast(ui, 2, format!("安装线程异常: {e}")),
    }
}

/// 选择一个**已解压**的插件文件夹并安装。
async fn plugin_add_from_dir(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let Some(dir) = rfd::FileDialog::new()
        .set_title("选择已解压的插件文件夹")
        .pick_folder()
    else {
        return;
    };
    let root = plugins_dir();
    let gv = gui_version();
    let res = tokio::task::spawn_blocking(move || {
        plugin_host::package::install_verified_dir(&dir, &root, &gv)
    })
    .await;

    match res {
        Ok(Ok(outcome)) => {
            toast(ui, 1, outcome.message());
            rescan_plugins(ctx, ui).await;
        }
        Ok(Err(e)) => toast(ui, 2, format!("添加插件失败: {e:#}")),
        Err(e) => toast(ui, 2, format!("安装线程异常: {e}")),
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
    let (roots, naming_idx, seq_pad, dedupe, obs_idx, trash_dir) = read_ui(ui, |ui| {
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
    match invoke_plugin(ctx, "imagehost", params).await
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

/// 插件是否已启用（默认启用）。
///
/// HTTP 层已按此拦截（停用返回 403）；GUI 自身的调用必须走同一判断，
/// 否则"停用插件"后界面里的插件面板仍然可用，与设置不一致。
fn plugin_enabled(ctx: &Arc<Ctx>, id: &str) -> bool {
    ctx.cfg
        .lock()
        .map(|c| c.plugin_enabled.get(id).copied().unwrap_or(true))
        .unwrap_or(true)
}

/// 调用插件：停用时直接返回错误，与 HTTP 层保持一致的语义。
async fn invoke_plugin(
    ctx: &Arc<Ctx>,
    id: &str,
    params: Value,
) -> anyhow::Result<Value> {
    if !plugin_enabled(ctx, id) {
        return Err(anyhow!("插件 {id} 已停用，请在插件页启用后重试"));
    }
    ctx.host
        .invoke(&find_wasm(id), "request", params, Some(ctx.caps()), id)
        .await
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

    // ---- 自绘标题栏的窗口控制 ----
    ui.on_win_minimize({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                ui.window().set_minimized(true);
            }
        }
    });
    ui.on_win_toggle_maximize({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                // 直接取反当前状态（maximized 由 Slint 与 winit 双向同步）
                let now = !ui.window().is_maximized();
                ui.window().set_maximized(now);
            }
        }
    });
    ui.on_win_close({
        let tx = tx.clone();
        move || {
            // 与系统关闭按钮一致：走 Quit（卸载挂载 / 停服务 / 退出 rclone）
            let _ = tx.send(Cmd::Quit);
        }
    });
    ui.on_plugin_toggle({
        let tx = tx.clone();
        move |i| {
            let _ = tx.send(Cmd::PluginToggle { idx: i });
        }
    });
    ui.on_plugin_add_from_file({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::PluginAddFromFile);
        }
    });
    ui.on_plugin_add_from_dir({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::PluginAddFromDir);
        }
    });
    ui.on_plugin_open_dir({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::PluginOpenDir);
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
    // 连接动作（阶段 3）：三个按钮都只影响 RC 连接，不启停 rclone 进程
    ui.on_conn_connect({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::Connect);
        }
    });
    ui.on_conn_disconnect({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::Disconnect);
        }
    });
    ui.on_conn_reconnect({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::Reconnect);
        }
    });
    // 档案导入导出（阶段 5A）：文件选择在 UI 线程做，真正的读写交给后台
    ui.on_conf_export({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move || {
            // 是否携带凭据由用户显式勾选决定，默认不含
            let include_secrets = ui
                .upgrade()
                .map(|u| u.global::<SettingsG>().get_conf_include_secrets())
                .unwrap_or(false);
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("rcgui.conf", &["conf"])
                .set_file_name("rcgui.conf")
                .set_title(if include_secrets {
                    "导出 rcgui.conf（含凭据）"
                } else {
                    "导出 rcgui.conf（不含凭据）"
                })
                .save_file()
            {
                let _ = tx.send(Cmd::ConfExport { path, include_secrets });
            }
        }
    });
    ui.on_conf_import_preview({
        let tx = tx.clone();
        move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("rcgui.conf", &["conf", "json"])
                .set_title("选择要导入的 rcgui.conf")
                .pick_file()
            {
                let _ = tx.send(Cmd::ConfPreview { path });
            }
        }
    });
    ui.on_conf_import_apply({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::ConfApply);
        }
    });
    // 已确认「包含凭据」：此时才选文件并导出（确认前不会生成任何文件）
    ui.on_conf_export_confirmed({
        let tx = tx.clone();
        move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("rcgui.conf", &["conf"])
                .set_file_name("rcgui.conf")
                .set_title("导出 rcgui.conf（含凭据）")
                .save_file()
            {
                let _ = tx.send(Cmd::ConfExport { path, include_secrets: true });
            }
        }
    });
    // 取消：不选文件、不写盘、不留下任何可复用的授权状态
    ui.on_conf_export_cancelled({
        move || {
            // 直接提示即可：这里没有任何状态需要清理（确认项从未登记）
            println!("[conf] 已取消导出（未生成文件）");
        }
    });
    ui.on_script_generate({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::ScriptGenerate);
        }
    });
    ui.on_script_copy({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::ScriptCopy);
        }
    });
    // 「以管理员身份运行」：确定 → 按目标权限重启；取消 → 复选框回到真实状态
    ui.on_admin_toggle({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::AdminToggle);
        }
    });
    ui.on_admin_restart({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::AdminRestart);
        }
    });
    ui.on_admin_cancel({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::AdminCancel);
        }
    });    // 「选择 rclone.exe」已移除：RCGUI 不再启动 rclone，不需要它的路径。

    // 初始加载
    let _ = tx.send(Cmd::RefreshDashboard);
    let _ = tx.send(Cmd::FilesInit);
    let _ = tx.send(Cmd::RemotesRefresh);
    let _ = tx.send(Cmd::MountsRefresh);
    let _ = tx.send(Cmd::PluginsRescan);
}

// ---------------- 托盘 ----------------

fn spawn_tray(ui: Weak<MainWindow>, tx: mpsc::UnboundedSender<Cmd>, main_hwnd: isize) {
    std::thread::spawn(move || {
        use tray_icon::menu::{ContextMenu, Menu, MenuEvent, MenuItem};
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
            // 同一份菜单：右键由系统弹出，左键我们手动在主窗口上弹出
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("Rclone GUI（左键菜单，含退出）")
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
                // 左键点击：弹出与右键相同的菜单（启 打开主窗可 / 退出 ），
                // 记录退出主在托盘区一眼可见；无有数 hwnd 时退化为直接显示主窗可
                if matches!(
                    ev,
                    TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    }
                ) {
                    if main_hwnd != 0 {
                        menu.show_context_menu_for_hwnd(main_hwnd, None);
                    } else {
                        let _ = ui.upgrade_in_event_loop(|ui| {
                            ui.show().ok();
                            ui.window().set_minimized(false);
                        });
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, kind: &str, value: &str, required: bool) -> FieldRow {
        FieldRow {
            name: name.into(),
            label: name.into(),
            kind: kind.into(),
            value: value.into(),
            required,
            help: String::new().into(),
            is_advanced: false,
            options: String::new().into(),
        }
    }

    /// 挂载点比较必须归一化：rclone 回报 "T:"，配置/输入可能是 "T:\" 或 "t"。
    /// 若直接字符串比较，就会漏判「盘符已被占用」，进而对已挂载盘符重复挂载。
    #[test]
    fn mount_key_normalizes_drive_forms() {
        assert_eq!(mount_key("T:"), "T:");
        assert_eq!(mount_key("T:\\"), "T:");
        assert_eq!(mount_key("t:"), "T:");
        assert_eq!(mount_key("t"), "T:");
        assert_eq!(mount_key(" T: "), "T:");
        // 各种写法必须互相相等
        assert_eq!(mount_key("T:"), mount_key("T:\\"));
        assert_eq!(mount_key("t"), mount_key("T:"));
    }

    /// 目录挂载点同样要能归一化比较
    #[test]
    fn mount_key_handles_directory_mounts() {
        assert_eq!(mount_key("C:\\mnt\\data\\"), "C:\\MNT\\DATA");
        assert_eq!(mount_key("C:\\mnt\\data"), "C:\\MNT\\DATA");
    }

    /// 回归：pending 里的 "T:\" 必须能与 rclone 回报的 "T:" 匹配上。
    /// 旧实现用 eq_ignore_ascii_case 直接比较，两者永不相等，
    /// 导致同一盘符在「活动挂载」里出现两条（一条 pending + 一条真实），
    /// 并且第二条会再次发起挂载、失败后弹出「未生效」。
    #[test]
    fn pending_matches_real_mount_across_separator_forms() {
        // rclone 的真实回报
        let real = "T:";
        // pending 里存的是规范化后的配置值
        for pending in ["T:\\", "T:", "t", "t:\\"] {
            assert_eq!(
                mount_key(real),
                mount_key(pending),
                "pending {pending:?} 应与真实挂载 {real:?} 视为同一挂载点"
            );
        }
        // 对照：旧的比较方式在 "T:\" vs "T:" 上会漏判
        assert!(
            !real.eq_ignore_ascii_case("T:\\"),
            "确认旧实现确实无法匹配（这就是重复条目的原因）"
        );
    }

    /// 已在 listmounts 里的盘符应判定为「已占用」
    #[test]
    fn mount_point_taken_detects_known_mount() {
        let known = vec![("teldrive:".to_string(), "T:".to_string())];
        assert!(mount_point_taken("T:\\", &known));
        assert!(mount_point_taken("t:", &known));
        // 既不在 listmounts、也不存在于文件系统的挂载点 → 未占用
        // （用随机不存在的目录，避免依赖机器上恰好没有某个盘符）
        let missing = format!(
            "{}\\no-such-mount-{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        assert!(!mount_point_taken(&missing, &known));
    }

    /// 布尔必须显式写出 false：否则用户取消勾选后旧值仍留在配置里。
    #[test]
    fn bool_false_is_emitted() {
        let out = fields_to_params(&[field("no_check", "bool", "false", false)]);
        assert_eq!(out["no_check"], Value::Bool(false));
    }

    #[test]
    fn bool_true_is_emitted() {
        let out = fields_to_params(&[field("no_check", "bool", "true", false)]);
        assert_eq!(out["no_check"], Value::Bool(true));
    }

    #[test]
    fn integer_number_is_emitted_as_number() {
        let out = fields_to_params(&[field("port", "number", "42", false)]);
        assert_eq!(out["port"], Value::Number(42.into()));
    }

    /// float 不能退化成字符串，否则 rclone 会收到 "1.5" 而非 1.5。
    #[test]
    fn float_number_is_emitted_as_number() {
        let out = fields_to_params(&[field("ratio", "number", "1.5", false)]);
        assert!(out["ratio"].is_number(), "expected number, got {}", out["ratio"]);
    }

    /// rclone 特有的 "10M"/"1h30m" 写法按字符串传递。
    #[test]
    fn size_suffix_stays_string() {
        let out = fields_to_params(&[field("chunk", "number", "10M", false)]);
        assert_eq!(out["chunk"], Value::String("10M".into()));
    }

    /// 非必填空值应被跳过，让 rclone 使用默认值。
    #[test]
    fn empty_optional_is_skipped() {
        let out = fields_to_params(&[field("optional", "text", "", false)]);
        assert!(out.get("optional").is_none());
    }

    /// 必填空值要照传，交由 rclone 校验并报错，而不是静默丢弃。
    #[test]
    fn empty_required_is_kept() {
        let out = fields_to_params(&[field("required", "text", "", true)]);
        assert_eq!(out["required"], Value::String(String::new()));
    }

    /// 值两端空白应被裁剪，避免把误输入的空格写进配置。
    #[test]
    fn value_is_trimmed() {
        let out = fields_to_params(&[field("name", "text", "  hello  ", false)]);
        assert_eq!(out["name"], Value::String("hello".into()));
    }

    /// 直链路径必须先规范化，防止 `..` 穿越到图床根目录之外。
    #[test]
    fn rel_path_strips_traversal() {
        use crate::http::normalize_rel_path;
        assert_eq!(normalize_rel_path("../../windows/win.ini"), "windows/win.ini");
        assert_eq!(normalize_rel_path("a/./b/../c.png"), "a/b/c.png");
        assert_eq!(normalize_rel_path("/abs/path.png"), "abs/path.png");
        assert_eq!(normalize_rel_path("back\\slash.png"), "back/slash.png");
    }

    #[test]
    fn rel_path_keeps_normal_names() {
        use crate::http::normalize_rel_path;
        assert_eq!(normalize_rel_path("wallpaper/2026/a.png"), "wallpaper/2026/a.png");
        assert_eq!(normalize_rel_path("图床/中文名.png"), "图床/中文名.png");
    }

    #[test]
    fn rel_path_traversal_only_becomes_empty() {
        use crate::http::normalize_rel_path;
        assert_eq!(normalize_rel_path(".."), "");
        assert_eq!(normalize_rel_path("../../"), "");
    }
}
