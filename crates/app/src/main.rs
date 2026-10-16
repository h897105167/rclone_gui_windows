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
    FilesRefresh,
    FilesDownload { name: String, local: PathBuf },
    FilesMkdir,
    FilesDel { name: String, is_dir: bool },
    FilesRename { from: String, to: String },
    FilesCopy { name: String },
    FilesCopyLink { name: String },
    MountsRefresh,
    MountAdd,
    MountRemove { point: String },
    RemotesRefresh,
    RemotesFilter,
    RemoteAdd,
    RemoteEditLoad { name: String },
    RemoteDelete { name: String },
    RemoteSave,
    RemoteCancel,
    RemoteTypeChanged,
    ServesRefresh,
    ServeStart,
    ServeStop { id: String },
    JobsRefresh,
    JobStop { id: i64 },
    BwLimitApply,
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

fn confirm_dialog(msg: String) -> bool {
    rfd::MessageDialog::new()
        .set_title("确认")
        .set_description(&msg)
        .set_buttons(rfd::MessageButtons::OkCancel)
        .show()
        == rfd::MessageDialogResult::Ok
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

fn apply_remotes_filter(ui: &Weak<MainWindow>, full: &[(String, String, String)]) {
    let filter = ui
        .upgrade()
        .map(|u| u.global::<RemotesG>().get_filter().to_string())
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

/// 从 UI 读取当前 FieldRow 列表
fn read_fields_from_ui(ui: &Weak<MainWindow>) -> Vec<FieldRow> {
    ui.upgrade()
        .map(|u| {
            let m = u.global::<RemotesG>().get_edit_fields();
            (0..m.row_count())
                .filter_map(|i| m.row_data(i))
                .collect()
        })
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
    if cfg.rclone.exe.is_empty() {
        // 优先释放并试用内嵌 rclone.exe
        let extracted = extract_embedded_rclone();
        if let Ok(exe) = discover_exe(extracted.as_deref()) {
            cfg.rclone.exe = exe.display().to_string();
            let _ = config::save(&cfg);
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

    wire_callbacks(&ui, ctx, tx.clone());

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
            let model = ModelRc::from(Rc::new(VecModel::from(rows.clone())));
            g.set_xfers(model);
            ui.global::<TransfersG>()
                .set_active(ModelRc::from(Rc::new(VecModel::from(rows))));
        },
    );
}

/// 返回 true 表示退出
async fn handle_cmd(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, cmd: Cmd) -> bool {
    match cmd {
        Cmd::Quit => {
            let _ = ctx.mgr.stop().await;
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
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh)).await;
            false
        }
        Cmd::FilesRefresh => {
            let (remote, path) = ui_files_pos(ui);
            // remotes 尚未加载完成时静默跳过，避免 fs=":" 报错
            if remote.is_empty() {
                return false;
            }
            let client = ctx.mgr.client.read().await.clone();
            let fs = format!("{remote}:");
            let res = client.list(&fs, &path, None).await;
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
                },
            );
            false
        }
        Cmd::FilesDownload { name, local } => {
            let (remote, path) = ui_files_pos(ui);
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
            let (remote, path) = ui_files_pos(ui);
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
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh)).await;
                    toast(ui, 1, "文件夹已创建（选中后可重命名）");
                }
                Err(e) => toast(ui, 2, format!("创建失败: {e}")),
            }
            false
        }
        Cmd::FilesDel { name, is_dir } => {
            let (remote, path) = ui_files_pos(ui);
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
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh)).await;
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
            let (remote, path) = ui_files_pos(ui);
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
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh)).await;
                    toast(ui, 1, "重命名完成");
                }
                Err(e) => toast(ui, 2, format!("重命名失败: {e}")),
            }
            false
        }
        Cmd::FilesCopy { name } => {
            let (remote, path) = ui_files_pos(ui);
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
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh)).await;
                    toast(ui, 1, format!("已复制为 {cand}"));
                }
                Err(e) => toast(ui, 2, format!("复制失败: {e}")),
            }
            false
        }
        Cmd::FilesCopyLink { name } => {
            let (remote, path) = ui_files_pos(ui);
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
            let _ = ui.upgrade_in_event_loop(
                move |ui| {
                    let rows: Vec<MountRow> = mounts
                        .iter()
                        .map(|m| MountRow {
                            fs: m.fs.clone().into(),
                            mount_point: m.mount_point.clone().into(),
                        })
                        .collect();
                    ui.global::<MountsG>()
                        .set_mounts(ModelRc::from(Rc::new(VecModel::from(rows))));
                },
            );
            false
        }
        Cmd::MountAdd => {
            let (fs, point, ro, cache) = ui
                .upgrade()
                .map(|ui| {
                    let m = ui.global::<MountsG>();
                    let fs = m.get_new_fs().to_string();
                    let point = m.get_new_point().to_string();
                    let ro = m.get_ro();
                    let cache = m
                        .get_caches()
                        .row_data(m.get_cache_idx().max(0) as usize)
                        .unwrap_or("minimal".into())
                        .to_string();
                    (fs, point, ro, cache)
                })
                .unwrap_or_default();
            if fs.trim().is_empty() {
                toast(ui, 2, "请填写远程路径（如 teldrive:）");
                return false;
            }
            let client = ctx.mgr.client.read().await.clone();
            let opts = json!({
                "vfs_cache_mode": cache,
                "read_only": ro,
            });
            let _ = ui.upgrade_in_event_loop(
                |ui| {
                    ui.global::<MountsG>().set_busy(true);
                },
            );
            let res = client.mount(&fs, &point, opts).await;
            let _ = ui.upgrade_in_event_loop(
                |ui| {
                    ui.global::<MountsG>().set_busy(false);
                },
            );
            match res {
                Ok(actual) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    toast(ui, 1, format!("已挂载到 {actual}"));
                }
                Err(e) => toast(ui, 2, format!("挂载失败: {e}")),
            }
            false
        }
        Cmd::MountRemove { point } => {
            let client = ctx.mgr.client.read().await.clone();
            match client.unmount(&point).await {
                Ok(_) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    toast(ui, 1, format!("已卸载 {point}"));
                }
                Err(e) => toast(ui, 2, format!("卸载失败: {e}")),
            }
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
            apply_remotes_filter(ui, &full);
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
                    apply_remotes_filter(&ui2, &full2);
                });
            }
            false
        }
        Cmd::RemotesFilter => {
            let full = ctx.remotes_cache.lock().unwrap().clone();
            apply_remotes_filter(ui, &full);
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
            let type_name = ui.upgrade()
                .map(|ui| {
                    let g = ui.global::<RemotesG>();
                    let ti = g.get_type_idx().max(0) as usize;
                    g.get_types().row_data(ti).unwrap_or_default().to_string()
                })
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
            let (name, type_str, params_text, is_new, show_raw) = ui
                .upgrade()
                .map(|ui| {
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
                .unwrap_or_default();
            if name.trim().is_empty() || name.contains(':') || name.contains(' ') {
                toast(ui, 2, "名称不能为空，且不能包含空格或冒号");
                return false;
            }
            // 优先使用模板字段，原始模式或字段为空时用 key=value
            let params = if !show_raw {
                let fields = read_fields_from_ui(ui);
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
            if !confirm_dialog(format!("确认删除远程存储 {}？", name)) {
                return false;
            }
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
            let remotes = client.list_remotes().await.unwrap_or_default();
            let v = client.serve_list().await.unwrap_or(json!({}));
            let items = v
                .get("list")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            let rows: Vec<ServeRow> = items
                .iter()
                .map(|it| {
                    let id = it
                        .get("id")
                        .and_then(|x| {
                            x.as_str()
                                .map(|s| s.to_string())
                                .or_else(|| x.as_i64().map(|n| n.to_string()))
                        })
                        .unwrap_or_default();
                    let ty = it
                        .get("type")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
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
                    ServeRow {
                        id: id.into(),
                        stype: ty.into(),
                        fs: getp(&["fs", "Fs"]).into(),
                        addr: getp(&["addr", "Addr", "listen", "Listen"]).into(),
                    }
                })
                .collect();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                ui.global::<ServesG>().set_remotes(ModelRc::from(Rc::new(VecModel::from(
                    remotes.iter().map(|r| r.clone().into()).collect::<Vec<slint::SharedString>>(),
                ))));
                ui.global::<ServesG>()
                    .set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            });
            false
        }
        Cmd::ServeStart => {
            let (stype, fs, addr, user, pass) = ui
                .upgrade()
                .map(|ui| {
                    let g = ui.global::<ServesG>();
                    let ty = g
                        .get_types()
                        .row_data(g.get_type_idx().max(0) as usize)
                        .unwrap_or_default()
                        .to_string();
                    let fs = g
                        .get_remotes()
                        .row_data(g.get_remote_idx().max(0) as usize)
                        .unwrap_or_default()
                        .to_string();
                    (
                        ty,
                        fs,
                        g.get_addr().to_string(),
                        g.get_user().to_string(),
                        g.get_pass().to_string(),
                    )
                })
                .unwrap_or_default();
            if fs.is_empty() {
                toast(ui, 2, "请先在「远程」页创建远程存储");
                return false;
            }
            if addr.trim().is_empty() {
                toast(ui, 2, "请填写监听地址");
                return false;
            }
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(true));
            let client = ctx.mgr.client.read().await.clone();
            let res = client
                .serve_start(&stype, &format!("{fs}:"), &addr, &user, &pass)
                .await;
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<ServesG>().set_busy(false));
            match res {
                Ok(id) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ServesRefresh)).await;
                    toast(ui, 1, format!("服务已启动: {id}"));
                }
                Err(e) => toast(ui, 2, format!("启动失败: {e}")),
            }
            false
        }
        Cmd::ServeStop { id } => {
            let client = ctx.mgr.client.read().await.clone();
            match client.serve_stop(&id).await {
                Ok(()) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ServesRefresh)).await;
                    toast(ui, 1, format!("已停止 {id}"));
                }
                Err(e) => toast(ui, 2, format!("停止失败: {e}")),
            }
            false
        }
        Cmd::JobsRefresh => {
            let client = ctx.mgr.client.read().await.clone();
            let ids = client.job_list().await.unwrap_or_default();
            let mut rows: Vec<JobRow> = Vec::new();
            for id in &ids {
                if let Ok(st) = client.job_status(*id).await {
                    let finished = st
                        .get("finished")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    let success = st
                        .get("success")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    let error = st
                        .get("error")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let duration = st.get("duration").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let status = if !finished {
                        "运行中"
                    } else if success {
                        "已完成"
                    } else {
                        "失败"
                    }
                    .to_string();
                    let progress = st
                        .get("progress")
                        .and_then(|p| {
                            p.as_f64()
                                .or_else(|| p.get("percent").and_then(|x| x.as_f64()))
                        })
                        .unwrap_or(0.0) as i32;
                    let desc = if finished && !success && !error.is_empty() {
                        format!("#{} · {}", id, error)
                    } else {
                        format!("任务 #{}", id)
                    };
                    rows.push(JobRow {
                        id: *id as i32,
                        desc: desc.into(),
                        status: status.into(),
                        progress,
                        duration: fmt_duration(duration).into(),
                    });
                }
            }
            let _ = ui.upgrade_in_event_loop(move |ui| {
                ui.global::<TransfersG>()
                    .set_jobs(ModelRc::from(Rc::new(VecModel::from(rows))));
            });
            false
        }
        Cmd::JobStop { id } => {
            let client = ctx.mgr.client.read().await.clone();
            match client.job_stop(id).await {
                Ok(()) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::JobsRefresh)).await;
                    toast(ui, 1, format!("任务 #{} 已停止", id));
                }
                Err(e) => toast(ui, 2, format!("停止失败: {e}")),
            }
            false
        }
        Cmd::BwLimitApply => {
            let rate = ui
                .upgrade()
                .map(|ui| ui.global::<TransfersG>().get_bwlimit().to_string())
                .unwrap_or_default();
            let rate = if rate.trim().is_empty() { "off".into() } else { rate.trim().to_string() };
            let client = ctx.mgr.client.read().await.clone();
            match client.bwlimit(&rate).await {
                Ok(applied) => toast(ui, 1, format!("限速已设为 {}", applied)),
                Err(e) => toast(ui, 2, format!("限速设置失败: {e}")),
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
            let (transfers, checkers, tps, log_level) = ui
                .upgrade()
                .map(|ui| {
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
            let row = ui
                .upgrade()
                .and_then(|ui| ui.global::<PluginsG>().get_rows().row_data(idx as usize));
            if let Some(r) = row {
                let id = r.id.to_string();
                let enabled = !r.enabled;
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
            let root_id = ui
                .upgrade()
                .map(|ui| {
                    ui.global::<PluginsG>()
                        .get_roots()
                        .row_data(0)
                        .map(|r| r.id.to_string())
                        .unwrap_or_default()
                })
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
            if let Some(ui) = ui.upgrade() {
                let s = ui.global::<SettingsG>();
                let mut cfg = ctx.cfg.lock().unwrap().clone();
                cfg.rclone.mode = s.get_mode_idx() as u8;
                cfg.rclone.exe = s.get_exe().to_string();
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
                if let Err(e) = config::save(&cfg) {
                    toast_ui(&ui, 2, format!("配置保存失败: {e}"));
                } else {
                    toast_ui(&ui, 1, "设置已保存（连接相关改动重启后生效）");
                }
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
    let (roots, naming_idx, seq_pad, dedupe, public_template, obs_idx, trash_dir) = ui
        .upgrade()
        .map(|ui| {
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

fn ui_files_pos(ui: &Weak<MainWindow>) -> (String, String) {
    ui.upgrade()
        .map(|ui| {
            let g = ui.global::<FilesG>();
            let remotes = g.get_remotes();
            let idx = g.get_remote_idx();
            let remote = remotes
                .row_data(idx.max(0) as usize)
                .unwrap_or_default()
                .to_string();
            (remote, g.get_path().to_string())
        })
        .unwrap_or_default()
}

// ---------------- 回调接线 ----------------

fn wire_callbacks(ui: &MainWindow, ctx: Arc<Ctx>, tx: mpsc::UnboundedSender<Cmd>) {
    ui.on_refresh_dashboard({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::RefreshDashboard);
        }
    });
    ui.on_files_refresh({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::FilesRefresh);
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
                // 进入目录
                if let Some(ui) = ui.upgrade() {
                    let g = ui.global::<FilesG>();
                    let cur = g.get_path().to_string();
                    g.set_path(join_path(&cur, &row.name).into());
                }
                let _ = tx.send(Cmd::FilesRefresh);
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
            if let Some(ui) = ui.upgrade() {
                let g = ui.global::<FilesG>();
                let cur = g.get_path().to_string();
                let parent = match cur.rfind('/') {
                    Some(idx) => cur[..idx].to_string(),
                    None => String::new(),
                };
                g.set_path(parent.into());
            }
            let _ = tx.send(Cmd::FilesRefresh);
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
            let row = ui
                .upgrade()
                .and_then(|ui| ui.global::<FilesG>().get_entries().row_data(i as usize));
            let Some(row) = row else { return };
            let hint = if row.is_dir { "（文件夹将整体删除）" } else { "" };
            if confirm_dialog(format!("确认删除 {}？{}", row.name, hint)) {
                let _ = tx.send(Cmd::FilesDel {
                    name: row.name.to_string(),
                    is_dir: row.is_dir,
                });
            }
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
    ui.on_nav_select({
        let tx = tx.clone();
        move |i| {
            match i {
                0 => { let _ = tx.send(Cmd::RefreshDashboard); }
                1 => { let _ = tx.send(Cmd::FilesInit); }
                2 => { let _ = tx.send(Cmd::RemotesRefresh); }
                3 => { let _ = tx.send(Cmd::MountsRefresh); }
                4 => { let _ = tx.send(Cmd::ServesRefresh); }
                5 => { let _ = tx.send(Cmd::JobsRefresh); }
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
        let tx = tx.clone();
        move |_| {
            let _ = tx.send(Cmd::RemotesFilter);
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
            let _ = tx.send(Cmd::JobsRefresh);
        }
    });
    ui.on_bwlimit_apply({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::BwLimitApply);
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
            let t = ui
                .upgrade()
                .and_then(|ui| ui.global::<PluginsG>().get_tokens().row_data(i as usize));
            let Some(t) = t else { return };
            if confirm_dialog(format!("确认撤销令牌 {}？", t.name)) {
                let _ = tx.send(Cmd::IhRevokeToken { id: t.id.to_string() });
            }
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
            let path = ui
                .upgrade()
                .and_then(|ui| ui.global::<PluginsG>().get_images().row_data(i as usize))
                .map(|r| r.path.to_string())
                .unwrap_or_default();
            if confirm_dialog(format!("确认删除 {path}？")) {
                let _ = tx.send(Cmd::IhImageDel { path });
            }
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
