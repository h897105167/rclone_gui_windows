//! 无界面诊断命令行（目标 8）。
//!
//! 入口在 `main()` 里**早于任何窗口/单实例/托盘初始化**被调用：用户在
//! 远程桌面、计划任务、或者干脆没有显示器的机器上也要能问清"装的是哪版、
//! 配置能不能用、内核在哪"。所以这个模块只碰文件与进程，绝不碰 Slint。
//!
//! 三条硬规则：
//! 1. 输出是给人也给人写的程序看的：`--json` 稳定字段名，不加彩色、不加进度。
//! 2. 退出码有含义（见 [`EXIT_*`]），脚本能靠它判断，而不是靠 grep 文案。
//! 3. 输出里不出现任何凭据。路径里的用户名也做遮蔽（见 [`mask_path`]）：
//!    诊断信息最常见的用途就是贴到 issue 里。
//!
//! 关于 `--export-accessibility-tree`：本程序在无窗口模式下运行，拿不到真实的
//! 运行时 UIA 树。这里导出的是**语义结构快照**——界面应该有哪几页、每页有哪些
//! 分组、哪些操作——并在文件里写明它不是 UIA 树。假装拥有运行时树比不导出更糟：
//! 验收的人会以为读屏已经验证过了。

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config;
use crate::core;

/// 正常完成。
pub const EXIT_OK: i32 = 0;
/// 运行中出错（读不到文件、写不进目标）。
pub const EXIT_FAIL: i32 = 1;
/// 命令行参数本身不对（缺值、未知开关）。
pub const EXIT_USAGE: i32 = 2;
/// 输入文件存在、能读，但内容不合法（配置/清单校验失败）。
pub const EXIT_CONFIG: i32 = 3;

/// 所有界面页面名。必须与 `ui/main.slint` 的 `page-names` 一致：读屏念的是
/// 同一批词，快照里换成别的说法只会让人对不上号。
const PAGES: &[&str] = &[
    "仪表盘",
    "文件",
    "远程",
    "挂载",
    "服务",
    "传输",
    "插件",
    "设置",
];

/// 设置对话框的分页（`main.slint` 里设置窗的 tab 顺序）。
const SETTINGS_TABS: &[&str] = &["常规", "内核与连接", "备份与同步", "更新与诊断"];

/// 插件的两个分页。
const PLUGIN_TABS: &[&str] = &["已安装", "商店"];

/// `maintenance-action` 接受的 action（第一参数）。写进快照，让验收时能对照
/// 文档检查"界面上有的动作，命令行也知道"。
const MAINTENANCE_ACTIONS: &[(&str, &str)] = &[
    ("core-detect", "检测本机 rclone 内核"),
    ("core-install", "下载并安装官方内核"),
    ("core-start", "启动应用管理的内核"),
    ("core-stop", "停止应用管理的内核"),
    ("core-save", "保存内核设置"),
    ("check-update", "检查程序更新"),
    ("apply-update", "下载并应用更新"),
    ("store-refresh", "刷新插件商店清单"),
    ("store-install", "安装选中的商店插件"),
    ("store-update", "更新选中的商店插件"),
    ("backup-save", "导出一份配置文件"),
    ("backup-push", "上传配置到 WebDAV"),
    ("backup-pull", "从 WebDAV 拉取配置（先预览）"),
    ("sync-save", "保存同步任务"),
    ("sync-run", "立即运行同步任务"),
    ("sync-toggle", "启用/停用同步任务"),
    ("sync-delete", "删除同步任务"),
    ("diagnostics", "在界面上显示诊断信息"),
];

/// 图形界面自己的开关：出现时由 `main()` 读，不属于命令行契约。
///
/// 这里必须写成**白名单**。早先的实现是「任何 `--xxx` 都放行 → 打开界面」，
/// 于是 `--diagnostics` 拼错成 `--diag` 时会静默弹出窗口：在计划任务、远程
/// 会话里表现为命令既不打印也不退出，脚本只能超时。未知开关一律按参数错误
/// 处理（退出码 2），比"猜用户想开界面"安全。
const GUI_FLAGS: &[&str] = &["--silent"];

/// 解析出来的意图。全部为默认值表示「没有认出的命令行开关」→ 走图形界面。
#[derive(Debug, Default)]
struct Flags {
    json: bool,
    want_version: bool,
    want_diag: bool,
    want_plugins: bool,
    want_help: bool,
    validate: Option<String>,
    tree: Option<String>,
}

/// 解析参数并执行。返回 `Some(退出码)` 表示进程应该就此结束，`None` 表示
/// 继续走图形界面。
pub fn run() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return None;
    }
    match parse_args(&args) {
        Ok(flags) => dispatch(flags),
        Err(code) => Some(code),
    }
}

/// 只做解析，不碰界面、不碰文件。独立的目的是让它可测：
/// `--bad` 必须是参数错误，而不是"开界面"。
fn parse_args(args: &[String]) -> Result<Flags, i32> {
    let mut f = Flags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => f.json = true,
            "--version" | "-V" => f.want_version = true,
            "--diagnostics" => f.want_diag = true,
            "--list-plugins" => f.want_plugins = true,
            "--help" | "-h" => f.want_help = true,
            "--validate-config" => match args.get(i + 1) {
                Some(v) => {
                    f.validate = Some(v.clone());
                    i += 1;
                }
                None => return Err(usage_error("--validate-config 需要一个文件路径")),
            },
            "--export-accessibility-tree" => match args.get(i + 1) {
                Some(v) => {
                    f.tree = Some(v.clone());
                    i += 1;
                }
                None => return Err(usage_error("--export-accessibility-tree 需要一个输出路径")),
            },
            other => {
                if GUI_FLAGS.contains(&other) {
                    // 界面自己的开关：不处理，继续扫后面的参数——
                    // `--silent --diagnostics` 应该走诊断，而不是开界面。
                    i += 1;
                    continue;
                }
                return Err(usage_error(&format!("不认识的参数：{other}")));
            }
        }
        i += 1;
    }
    Ok(f)
}

fn dispatch(f: Flags) -> Option<i32> {
    let Flags {
        json,
        want_version,
        want_diag,
        want_plugins,
        want_help,
        validate,
        tree,
    } = f;

    if want_help {
        print_help();
        return Some(EXIT_OK);
    }
    if want_version {
        println!("rcgui {}", env!("CARGO_PKG_VERSION"));
        return Some(EXIT_OK);
    }
    if want_diag {
        let text = match diagnostics_text(json) {
            Ok(t) => t,
            Err(e) => {
                // 诊断本身失败也要有输出，否则用户只看到"退出码 1"。
                eprintln!(
                    "rcgui: 生成诊断信息失败：{}",
                    crate::logging::redact(&format!("{e:#}"))
                );
                return Some(EXIT_FAIL);
            }
        };
        println!("{text}");
        return Some(EXIT_OK);
    }
    if want_plugins {
        return Some(match list_plugins(json) {
            Ok(t) => {
                println!("{t}");
                EXIT_OK
            }
            Err(e) => {
                eprintln!(
                    "rcgui: 读取插件目录失败：{}",
                    crate::logging::redact(&format!("{e:#}"))
                );
                EXIT_FAIL
            }
        });
    }
    if let Some(path) = validate {
        return Some(validate_config(&path, json));
    }
    if let Some(path) = tree {
        return Some(export_tree(&path));
    }
    None
}

fn usage_error(msg: &str) -> i32 {
    eprintln!("rcgui: {msg}");
    eprintln!("试试 rcgui --help");
    EXIT_USAGE
}

fn print_help() {
    println!(
        "rcgui {} —— rclone 图形界面

用法:
  rcgui                                 打开窗口
  rcgui --version                       只打印版本
  rcgui --diagnostics [--json]          打印环境/配置/内核/插件/同步状态（不启动内核、不开窗口）
  rcgui --validate-config <文件> [--json]  校验一份 rcgui.conf，不写入任何文件
  rcgui --list-plugins [--json]         列出已安装插件
  rcgui --export-accessibility-tree <文件>  导出界面语义结构快照（不是运行时 UIA 树）

退出码: 0 成功 / 1 运行出错 / 2 参数错误 / 3 输入内容不合法",
        env!("CARGO_PKG_VERSION")
    );
}

/// 把用户主目录换成 `%USERPROFILE%`。
///
/// 诊断输出经常被贴进公开 issue，`C:\\Users\\张三\\...` 会连同真实姓名一起出去。
/// 只替换主目录前缀：程序自身的安装位置对排查有用，不该一起遮掉。
pub fn mask_path(p: &Path) -> String {
    let text = p.display().to_string();
    for var in ["USERPROFILE", "HOME"] {
        if let Ok(home) = std::env::var(var) {
            let home = home.trim_end_matches(['\\', '/']);
            if home.is_empty() {
                continue;
            }
            if let Some(rest) = text.strip_prefix(home) {
                let rest = rest.trim_start_matches(['\\', '/']);
                if rest.is_empty() {
                    return "%USERPROFILE%".to_string();
                }
                return format!("%USERPROFILE%\\{rest}");
            }
        }
    }
    text
}

fn mask_str(p: &Path) -> String {
    crate::logging::redact(&mask_path(p))
}

/// 诊断是从哪一侧观察的。
///
/// `--diagnostics` 是另起一个**无界面进程**运行的：它读同一份磁盘配置，但看不到
/// 正在运行的那个图形界面进程里的状态（窗口是显示还是隐藏、有没有进轻量模式、
/// 编译缓存用了多少）。这些字段必须报 null 并说明原因，不能拿本进程的默认值
/// 顶上去——否则用户会看到"界面 = normal"，而屏幕上那个窗口其实早就进了轻量模式。
#[derive(Debug, Clone, Copy)]
pub struct RuntimeFacts {
    /// 本进程就是界面进程（界面里的「诊断」按钮走这条），可以报运行态。
    pub live: bool,
    /// 仅 `live` 时有意义：现在是否处于轻量模式。
    pub light_active: bool,
    /// 仅 `live` 时有意义：窗口已隐藏多久（未隐藏为零）。
    pub hidden_for: std::time::Duration,
}

impl RuntimeFacts {
    /// 无界面进程观察到的诊断：运行态字段一律未知。
    pub fn offline() -> Self {
        Self {
            live: false,
            light_active: false,
            hidden_for: std::time::Duration::ZERO,
        }
    }

    /// 界面进程内观察到的诊断：`light_active` 需由调用方从真实 gate 里取。
    pub fn in_app(light_active: bool, hidden_for: std::time::Duration) -> Self {
        Self {
            live: true,
            light_active,
            hidden_for,
        }
    }
}

/// 生成诊断信息。`json=true` 时是机器可读的 JSON，否则是分行的人读文本。
pub fn diagnostics_text(json: bool) -> Result<String> {
    let d = diagnostics()?;
    if json {
        Ok(serde_json::to_string_pretty(&d)?)
    } else {
        Ok(render_diagnostics(&d))
    }
}

/// 无界面进程的诊断（`--diagnostics`）。
///
/// 运行态字段在返回的 JSON 里是 null，见 [`RuntimeFacts`]；界面里的诊断请用
/// [`diagnostics_with`] 传入 [`RuntimeFacts::in_app`]。
pub fn diagnostics() -> Result<serde_json::Value> {
    diagnostics_with(RuntimeFacts::offline())
}

/// 收集诊断数据。**不启动内核、不联网**：诊断要在断网、内核坏掉时也能用。
pub fn diagnostics_with(facts: RuntimeFacts) -> Result<serde_json::Value> {
    let data_dir = config::data_dir();
    let cfg_path = config::config_path();
    let cfg = config::load();
    let mut warnings: Vec<String> = Vec::new();

    // 配置文件是否能被解析：`config::load()` 读坏了会静默回落到默认值，
    // 那对用户是"设置全丢了"，对排查是"看不出原因"，所以这里单独再读一次。
    let (cfg_exists, cfg_error) = match std::fs::read(&cfg_path) {
        Ok(bytes) => match serde_json::from_slice::<config::AppConfig>(&bytes) {
            Ok(_) => (true, None),
            Err(e) => (true, Some(crate::logging::redact(&format!("{e}")))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, None),
        Err(e) => (true, Some(crate::logging::redact(&format!("{e}")))),
    };
    if let Some(err) = &cfg_error {
        warnings.push(format!("配置文件读不动，已按默认值运行：{err}"));
    }

    // 内核：只做定位与版本探测，不启动。
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    // 自定义路径无效时 `resolve` 报错而不是回落到随包/PATH（回落到别的内核会换掉
    // 用户特意指定的 fork），所以这里要把错误原样报出来，不能当成"没找到内核"。
    let (located, resolve_error) = match core::resolve(&cfg.core.path, &exe_dir, &data_dir) {
        Ok(l) => (l, None),
        Err(e) => (None, Some(crate::logging::redact(&format!("{e:#}")))),
    };
    let (core_exe, core_source, core_version, core_error) = match &located {
        Some(l) => {
            let (ver, err) = match core::probe_version(&l.exe) {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(crate::logging::redact(&format!("{e:#}")))),
            };
            (
                Some(mask_path(&l.exe)),
                Some(l.source.label().to_string()),
                ver,
                err,
            )
        }
        None => {
            if resolve_error.is_none() {
                warnings.push(
                    "没有找到 rclone 内核：在「设置 → 内核与连接」里指定内核路径（core.path），或让程序下载官方内核"
                        .to_string(),
                );
            }
            (None, None, None, resolve_error)
        }
    };
    if located.is_none() && cfg.core.normalized_mode() == "managed" && core_error.is_none() {
        warnings.push("内核模式是 managed，但本机没有可用的 rclone".to_string());
    }

    // 插件：扫目录，报告已装与坏掉的。
    let plugins_root = data_dir.join("plugins");
    let gui_version = env!("CARGO_PKG_VERSION");
    let mut installed: Vec<serde_json::Value> = Vec::new();
    let mut plugin_errors: Vec<String> = Vec::new();
    if plugins_root.is_dir() {
        let (bundle_rows, bundle_bad) = plugin_host::bundle::scan(&plugins_root, gui_version);
        for p in bundle_rows {
            installed.push(plugin_json(
                &p.config.id,
                &p.config.version,
                cfg.plugin_enabled
                    .get(&p.config.id)
                    .copied()
                    .unwrap_or(false),
                p.verified,
            ));
        }
        let (old_rows, old_bad) = plugin_host::package::scan_dir(&plugins_root, gui_version);
        for p in old_rows {
            installed.push(plugin_json(
                &p.meta.id,
                &p.meta.version,
                cfg.plugin_enabled.get(&p.meta.id).copied().unwrap_or(false),
                p.verified,
            ));
        }
        for (dir, why) in bundle_bad.into_iter().chain(old_bad) {
            plugin_errors.push(format!(
                "{}：{}",
                mask_str(&dir),
                crate::logging::redact(&why)
            ));
        }
    }
    installed.sort_by(|a, b| {
        a["id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["id"].as_str().unwrap_or(""))
    });
    if !plugin_errors.is_empty() {
        warnings.push(format!(
            "有 {} 个插件目录无法加载（见 plugins.errors）",
            plugin_errors.len()
        ));
    }

    let enabled_sync = cfg.sync_tasks.iter().filter(|t| t.enabled).count();
    let logs = log_files(&data_dir);

    // 这份数据的观察位置。读的人（包括贴到 issue 里的人）看到 offline 就知道
    // 下面那几处 null 不是"没实现"，而是"这个进程看不到"。
    let scope = if facts.live { "in-app" } else { "offline" };

    // 轻量模式：把"到底清了什么、什么没清"原样报出来。
    // 用户在任务管理器里看到的数字降不下来时，这份清单是唯一能说明白的地方；
    // 也避免把 EmptyWorkingSet 说成"释放了界面内存"。
    //
    // `active` / `hidden_for_seconds` 只有界面进程自己知道。无界面进程里那个计时
    // gate 从来没跑过，若这里报 false / 0，读的人会理解成"界面没进轻量模式"，
    // 而屏幕上那个窗口可能早就进过了——所以离线观察报 null。
    let light = serde_json::json!({
        "enabled": cfg.light.enabled,
        "minutes": crate::lightweight::clamp_minutes(cfg.light.minutes),
        "active": if facts.live {
            serde_json::Value::Bool(facts.light_active)
        } else {
            serde_json::Value::Null
        },
        "hidden_for_seconds": if facts.live {
            serde_json::Value::from(facts.hidden_for.as_secs())
        } else {
            serde_json::Value::Null
        },
        "cleared": crate::lightweight::what_is_cleared(),
        "kept": crate::lightweight::what_is_kept(),
        "observed_from": scope,
        "note": if facts.live {
            String::new()
        } else {
            "active / hidden_for_seconds 为 null：本次诊断来自独立的无界面进程，看不到正在运行的界面是否已进入轻量模式".to_string()
        },
    });

    // 编译进这个二进制的功能。这份清单是写死的、跟着代码走的：用户问的是
    // "这一版到底能做什么"，按配置推断出来的答案会在配置损坏时骗人。
    // 每个 false 都带一句为什么，避免看起来像"还没做完"。
    let features = serde_json::json!({
        "managed_core": true,
        "external_endpoint": true,
        "plugins_wasm": true,
        "kvault": true,
        "mounts": true,
        "serves": true,
        "transfers": true,
        "sync_tasks": true,
        "backup": true,
        "self_update": true,
        "http_server": true,
        "lightweight": true,
        "tray": true,
        "accessibility_static_tree": true,
        // 这个二进制编进了 Slint 的真实 accessibility（crates/app/Cargo.toml 里
        // 点名了 slint 的 "accessibility" 特性，底层是 accesskit）：Windows 上
        // 读屏软件看到的是运行中窗口暴露的 UIA 树。不能因为"导出的是快照"就
        // 说程序没有实时无障碍——那是两件事。
        "accessibility_live_uia": true,
        // 而 `--export-accessibility-tree` 产出的是静态语义快照
        // （kind=semantic-snapshot、runtime=false），这里把它的性质单独写出来。
        "accessibility_export_kind": "semantic-snapshot",
    });

    // 插件编译缓存：只有界面进程会编译插件模块，无界面进程里没有可报的数字。
    let (cache_modules, cache_bytes) = plugin_host::cache_stats_global();
    let cache = serde_json::json!({
        "wasm_modules": if facts.live { serde_json::Value::from(cache_modules) } else { serde_json::Value::Null },
        "wasm_bytes": if facts.live { serde_json::Value::from(cache_bytes) } else { serde_json::Value::Null },
        "wasm_capacity": plugin_host::MAX_CACHED_MODULES,
        "wasm_idle_seconds": plugin_host::CACHE_IDLE.as_secs(),
        "wasm_bytes_note": "wasm 文件大小，不是编译后的内存占用",
        "note": if facts.live {
            String::new()
        } else {
            "wasm_modules / wasm_bytes 为 null：本次诊断来自独立的无界面进程，它没有编译过插件模块".to_string()
        },
    });

    // 当前实际处于什么模式。`core` 来自磁盘上的配置，离线也准确；`ui` 是
    // 运行态，只有界面进程自己知道（见 [`RuntimeFacts`]）。
    let current_mode = serde_json::json!({
        "core": cfg.core.normalized_mode(),
        "ui": if facts.live {
            serde_json::Value::from(if facts.light_active { "lightweight" } else { "normal" })
        } else {
            serde_json::Value::Null
        },
        "lightweight_active": if facts.live {
            serde_json::Value::Bool(facts.light_active)
        } else {
            serde_json::Value::Null
        },
        "observed_from": scope,
        "note": if facts.live {
            String::new()
        } else {
            "ui / lightweight_active 为 null：这是另一个进程的观察，运行中的界面可能正处在轻量模式".to_string()
        },
    });

    Ok(serde_json::json!({
        "schema": 1,
        "tool": "rcgui",
        "version": env!("CARGO_PKG_VERSION"),
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        // `scope` 说明这份数据是谁看到的：offline 时下面的运行态字段为 null。
        "scope": scope,
        "runtime": facts.live,
        "data_dir": mask_str(&data_dir),
        "config_path": mask_str(&cfg_path),
        "current_mode": current_mode,
        "features": features,
        "cache": cache,
        "config": {
            "exists": cfg_exists,
            "error": cfg_error,
            "core_mode": cfg.core.normalized_mode(),
            "core_path": if cfg.core.path.is_empty() { String::new() } else { mask_str(Path::new(&cfg.core.path)) },
            "plugins_enabled": cfg.plugin_enabled.values().filter(|v| **v).count(),
            "sync_tasks": cfg.sync_tasks.len(),
            "sync_enabled": enabled_sync,
            "language": if cfg.language.is_empty() { "system".to_string() } else { cfg.language.clone() },
        },
        "core": {
            "exe": core_exe,
            "source": core_source,
            "version": core_version,
            "probe_error": core_error,
        },
        "plugins": {
            "dir": mask_str(&plugins_root),
            "items": installed,
            "errors": plugin_errors,
        },
        "sync": {
            "tasks": cfg.sync_tasks.len(),
            "enabled": enabled_sync,
            "running": if enabled_sync > 0 { "unknown" } else { "idle" },
        },
        "logs": logs,
        "lightweight": light,
        "warnings": warnings,
    }))
}

fn plugin_json(id: &str, version: &str, enabled: bool, verified: bool) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "version": version,
        "enabled": enabled,
        "verified": verified,
    })
}

fn log_files(data_dir: &Path) -> serde_json::Value {
    let dir = crate::logging::log_dir(data_dir);
    let mut files: Vec<serde_json::Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if !md.is_file() {
                continue;
            }
            files.push(serde_json::json!({
                "name": e.file_name().to_string_lossy(),
                "bytes": md.len(),
            }));
        }
    }
    files.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    serde_json::json!({ "dir": mask_str(&dir), "files": files })
}

/// 人读版本：用 `key: value` 分行，便于用户直接复制到聊天窗口。
fn render_diagnostics(d: &serde_json::Value) -> String {
    let s = |v: &serde_json::Value| match v {
        serde_json::Value::Null => "—".to_string(),
        serde_json::Value::String(x) => x.clone(),
        other => other.to_string(),
    };
    let mut out = String::new();
    let push = |out: &mut String, k: &str, v: String| {
        out.push_str(&format!("{k}: {v}\n"));
    };
    push(
        &mut out,
        "版本",
        format!("{} ({})", s(&d["version"]), s(&d["build"])),
    );
    push(
        &mut out,
        "系统",
        format!("{} / {}", s(&d["os"]), s(&d["arch"])),
    );
    push(
        &mut out,
        "观察范围",
        match s(&d["scope"]).as_str() {
            // 说清楚"看不到"和"没开启"的区别：界面上那几处 — 不是没实现。
            "in-app" => "界面进程内（含运行态）".to_string(),
            _ => "独立无界面进程（运行态字段为 —）".to_string(),
        },
    );
    push(&mut out, "数据目录", s(&d["data_dir"]));
    push(
        &mut out,
        "配置文件",
        format!("{}（存在={}）", s(&d["config_path"]), d["config"]["exists"]),
    );
    push(
        &mut out,
        "内核",
        format!(
            "{} 来源={} 版本={}",
            s(&d["core"]["exe"]),
            s(&d["core"]["source"]),
            s(&d["core"]["version"])
        ),
    );
    push(&mut out, "内核模式", s(&d["config"]["core_mode"]));
    let items = d["plugins"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    push(&mut out, "插件", format!("{} 个", items.len()));
    for p in &items {
        out.push_str(&format!(
            "  - {} {} {}{}\n",
            s(&p["id"]),
            s(&p["version"]),
            if p["enabled"].as_bool().unwrap_or(false) {
                "已启用"
            } else {
                "未启用"
            },
            if p["verified"].as_bool().unwrap_or(true) {
                ""
            } else {
                "（校验未通过）"
            }
        ));
    }
    push(
        &mut out,
        "同步任务",
        format!("{} 个（启用 {}）", d["sync"]["tasks"], d["sync"]["enabled"]),
    );
    push(&mut out, "日志目录", s(&d["logs"]["dir"]));
    push(
        &mut out,
        "轻量模式",
        if !d["lightweight"]["enabled"].as_bool().unwrap_or(false) {
            "关闭".to_string()
        } else if d["lightweight"]["active"].is_null() {
            format!(
                "隐藏 {} 分钟后生效（当前状态看不到：见观察范围）",
                d["lightweight"]["minutes"]
            )
        } else {
            format!(
                "隐藏 {} 分钟后生效（当前{}）",
                d["lightweight"]["minutes"],
                if d["lightweight"]["active"].as_bool().unwrap_or(false) {
                    "已生效"
                } else {
                    "未生效"
                }
            )
        },
    );
    push(
        &mut out,
        "无障碍",
        if d["features"]["accessibility_live_uia"]
            .as_bool()
            .unwrap_or(false)
        {
            "运行中窗口暴露 UIA 树；--export-accessibility-tree 导出的是语义快照".to_string()
        } else {
            "未编译进运行中 UIA（仅静态语义快照）".to_string()
        },
    );
    if let Some(list) = d["warnings"].as_array() {
        if !list.is_empty() {
            out.push_str("需要注意:\n");
            for w in list {
                out.push_str(&format!("  - {}\n", s(w)));
            }
        }
    }
    out
}

/// 列出已安装插件。
pub fn list_plugins(json: bool) -> Result<String> {
    let d = diagnostics()?;
    if json {
        return Ok(serde_json::to_string_pretty(&d["plugins"])?);
    }
    let items = d["plugins"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return Ok("（没有已安装的插件）".to_string());
    }
    let mut out = String::new();
    for p in items {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            p["id"].as_str().unwrap_or(""),
            p["version"].as_str().unwrap_or(""),
            if p["enabled"].as_bool().unwrap_or(false) {
                "enabled"
            } else {
                "disabled"
            },
            if p["verified"].as_bool().unwrap_or(true) {
                "verified"
            } else {
                "unverified"
            },
        ));
    }
    Ok(out)
}

/// 校验一份 rcgui.conf，不写任何文件。
///
/// 复用导入流程里的同一套校验（`conf_io::preview_file`）：命令行说是"能导入"，
/// 界面里也必须能导入，否则这个开关只会误导人。
fn validate_config(path: &str, json: bool) -> i32 {
    let cfg = config::load();
    let current = crate::profiles::current_doc(&cfg);
    let p = Path::new(path);
    if !p.is_file() {
        eprintln!("rcgui: 找不到文件：{}", mask_str(p));
        return EXIT_USAGE;
    }
    match crate::conf_io::preview_file(p, &current) {
        Ok((_conf, preview)) => {
            if json {
                let v = serde_json::json!({
                    "schema": 1,
                    "valid": true,
                    "file": mask_str(p),
                    "summary": preview.summary,
                    "plugins": preview.plugin_count,
                    "sync_tasks": preview.sync_count,
                    "changes": preview.diff,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                println!("{}：可以导入", mask_str(p));
                println!("{}", preview.summary);
                if preview.plugin_count > 0 || preview.sync_count > 0 {
                    println!(
                        "插件 {} 个 · 同步任务 {} 个（插件包体不在文件内，导入后需按清单重新安装）",
                        preview.plugin_count, preview.sync_count
                    );
                }
                println!("{}", crate::conf_io::describe(&preview));
            }
            EXIT_OK
        }
        Err(e) => {
            let msg = crate::logging::redact(&format!("{e:#}"));
            if json {
                let v = serde_json::json!({
                    "schema": 1,
                    "valid": false,
                    "file": mask_str(p),
                    "error": msg,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                eprintln!("rcgui: {} 不能导入：{msg}", mask_str(p));
            }
            EXIT_CONFIG
        }
    }
}

/// 导出界面语义结构快照。
///
/// 文件头 `"kind": "semantic-snapshot"` 与 `"runtime": false` 是刻意的：
/// 读的人（尤其验收的人）必须一眼看出这不是从运行中的窗口抓下来的 UIA 树。
fn export_tree(path: &str) -> i32 {
    let p = Path::new(path);
    let tree = tree_json();
    let text = match serde_json::to_string_pretty(&tree) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rcgui: 生成快照失败：{e}");
            return EXIT_FAIL;
        }
    };
    if let Some(dir) = p.parent() {
        if !dir.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("rcgui: 创建目录 {} 失败：{e}", mask_str(dir));
                return EXIT_FAIL;
            }
        }
    }
    // 先写临时文件再改名：中途失败不会留下半个 JSON。
    let tmp = p.with_extension("tmp-tree");
    let write = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("rcgui: 写入 {} 失败：{e}", mask_str(&tmp));
        return EXIT_FAIL;
    }
    if let Err(e) = std::fs::rename(&tmp, p) {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("rcgui: 保存 {} 失败：{e}", mask_str(p));
        return EXIT_FAIL;
    }
    println!("{}", mask_str(p));
    EXIT_OK
}

/// 快照内容：页面 → 分组 → 控件（名称 + 角色 + 说明）。
pub fn tree_json() -> serde_json::Value {
    let mut pages: Vec<serde_json::Value> = Vec::new();
    for (idx, name) in PAGES.iter().enumerate() {
        let mut children: Vec<serde_json::Value> = Vec::new();
        match *name {
            "设置" => {
                for tab in SETTINGS_TABS {
                    children.push(serde_json::json!({
                        "role": "tab", "name": tab, "note": "设置窗内的一页"
                    }));
                }
            }
            "插件" => {
                for tab in PLUGIN_TABS {
                    children.push(serde_json::json!({
                        "role": "tab", "name": tab, "note": "插件页的两个分页"
                    }));
                }
                children.push(serde_json::json!({
                    "role": "list",
                    "name": "插件列表",
                    "note": "每项含名称、版本、启用开关、设置、卸载；分页按 page 字段筛选"
                }));
            }
            "远程" => {
                children.push(serde_json::json!({
                    "role": "combobox",
                    "name": "类型",
                    "note": "可通过 type-query 过滤；选中项对应真实的 rclone 后端 id"
                }));
            }
            "传输" => {
                children.push(serde_json::json!({
                    "role": "list", "name": "传输任务", "note": "含进度、速度、取消"
                }));
            }
            _ => {}
        }
        pages.push(serde_json::json!({
            "role": "tab",
            "name": name,
            "index": idx,
            "children": children,
        }));
    }

    let actions: Vec<serde_json::Value> = MAINTENANCE_ACTIONS
        .iter()
        .map(|(id, note)| serde_json::json!({ "action": id, "note": note }))
        .collect();

    serde_json::json!({
        "schema": 1,
        "kind": "semantic-snapshot",
        "runtime": false,
        "generated_by": format!("rcgui {} --export-accessibility-tree", env!("CARGO_PKG_VERSION")),
        "notice": "这是界面的语义结构快照（应该有哪几页、哪些分组与操作），不是运行时 Windows UIA 树：本命令在无窗口模式下运行，读屏软件实际看到的控件树需要在有窗口的会话里导出。",
        "window": {
            "role": "window",
            "name": "rclone 图形界面",
        },
        "navigation": {
            "role": "tablist",
            "note": "键盘可切换；读屏念出页面名与当前页",
            "items": PAGES,
        },
        "pages": pages,
        "actions": actions,
        "conventions": [
            "所有可聚焦控件都有可读名字，不靠占位符或图标代替标签",
            "焦点可见：键盘 Tab 到的控件有描边，不只靠颜色变化",
            "状态不只靠颜色表达：同步失败/更新可用同时有文字",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_match_ui_page_names() {
        // 与 main.slint 的 page-names 必须逐字一致，否则快照与界面各说一套。
        assert_eq!(PAGES.len(), 8);
        assert_eq!(PAGES[0], "仪表盘");
        assert_eq!(PAGES[7], "设置");
        assert_eq!(SETTINGS_TABS.len(), 4);
    }

    #[test]
    fn tree_is_marked_as_snapshot_not_uia() {
        let t = tree_json();
        assert_eq!(t["kind"], "semantic-snapshot");
        assert_eq!(t["runtime"], false);
        let notice = t["notice"].as_str().unwrap();
        assert!(notice.contains("不是运行时"), "{notice}");
        let pages = t["pages"].as_array().unwrap();
        assert_eq!(pages.len(), PAGES.len());
    }

    #[test]
    fn tree_contains_every_maintenance_action() {
        let t = tree_json();
        let ids: Vec<String> = t["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["action"].as_str().unwrap().to_string())
            .collect();
        for want in [
            "core-detect",
            "core-install",
            "core-start",
            "core-stop",
            "core-save",
            "check-update",
            "apply-update",
            "store-refresh",
            "store-install",
            "store-update",
            "backup-save",
            "backup-push",
            "backup-pull",
            "sync-save",
            "sync-run",
            "sync-toggle",
            "sync-delete",
            "diagnostics",
        ] {
            assert!(ids.contains(&want.to_string()), "缺少动作 {want}");
        }
    }

    #[test]
    fn mask_path_hides_home_prefix() {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let p = Path::new(&home).join("Documents").join("x.conf");
        let masked = mask_path(&p);
        assert!(masked.starts_with("%USERPROFILE%"), "{masked}");
        assert!(
            !masked.contains(home.trim_end_matches(['\\', '/'])),
            "{masked}"
        );
    }

    #[test]
    fn diagnostics_json_has_stable_keys_and_no_secrets() {
        let d = diagnostics().expect("diagnostics");
        for key in [
            "schema",
            "version",
            "build",
            "os",
            "arch",
            "data_dir",
            "config_path",
            // 这三项是"这一版能做什么/现在在什么模式/缓存压了多少"的答案，
            // 脚本靠它们判断，不能改名或消失。
            "current_mode",
            "features",
            "cache",
            // `scope`/`runtime` 说明这份数据是谁看到的：offline 时运行态字段为 null。
            "scope",
            "runtime",
            "config",
            "core",
            "plugins",
            "sync",
            "logs",
            "lightweight",
            "warnings",
        ] {
            assert!(d.get(key).is_some(), "缺少字段 {key}");
        }
        // 能力清单必须是显式的布尔值，不能是"配置里开着就 true"——
        // 否则配置坏了会报出程序没有的能力。
        assert!(d["features"]["managed_core"].as_bool().unwrap_or(false));
        // crates/app/Cargo.toml 里真的点开了 slint 的 "accessibility"，Windows 上
        // 读屏看到的是运行中窗口的 UIA 树。不能因为"静态导出的只是快照"就说没这能力。
        assert_eq!(d["features"]["accessibility_live_uia"], true);
        assert_eq!(
            d["features"]["accessibility_export_kind"],
            "semantic-snapshot"
        );
        // 无界面进程看不到运行态：要报 null 并说明，而不是填本进程的默认值——
        // 用户会拿它判断"界面到底有没有进轻量模式"。
        assert_eq!(d["scope"], "offline");
        assert_eq!(d["runtime"], false);
        assert!(d["cache"]["wasm_modules"].is_null(), "{:?}", d["cache"]);
        assert!(!d["cache"]["note"].as_str().unwrap_or("").is_empty());
        assert!(d["lightweight"]["active"].is_null());
        assert!(!d["lightweight"]["note"].as_str().unwrap_or("").is_empty());
        assert!(d["current_mode"]["ui"].is_null());
        assert!(!d["current_mode"]["note"].as_str().unwrap_or("").is_empty());
        assert!(d["current_mode"]["core"].as_str().is_some());
        let cfg = config::load();
        let text = serde_json::to_string(&d).unwrap();
        if !cfg.backup.pass.is_empty() {
            assert!(!text.contains(&cfg.backup.pass), "诊断输出里出现了备份密码");
        }
        // 渲染成文本也不能崩，且要说清"看不到"和"没开启"的区别
        let human = render_diagnostics(&d);
        assert!(human.contains("版本:"));
        assert!(human.contains("观察范围: 独立无界面进程"));
    }

    #[test]
    fn in_app_diagnostics_report_live_runtime_state() {
        // 界面里的「诊断」按钮能看见真的运行态，不能把 offline 的 null 抄过来。
        let facts = RuntimeFacts::in_app(true, std::time::Duration::from_secs(720));
        let d = diagnostics_with(facts).expect("diagnostics");
        assert_eq!(d["scope"], "in-app");
        assert_eq!(d["runtime"], true);
        assert_eq!(d["lightweight"]["active"], true);
        assert_eq!(d["lightweight"]["hidden_for_seconds"], 720);
        assert_eq!(d["current_mode"]["ui"], "lightweight");
        assert!(d["lightweight"]["note"].as_str().unwrap_or("x").is_empty());
        // 界面进程里缓存数字是真值（可能为 0，但它是数字，不是 null）。
        assert!(d["cache"]["wasm_modules"].is_number(), "{:?}", d["cache"]);
        let human = render_diagnostics(&d);
        assert!(human.contains("观察范围: 界面进程内"));
    }

    #[test]
    fn diagnostics_without_core_still_works() {
        // 定位不到内核时字段为 null、warnings 里给出一条，而不是整体失败。
        let d = diagnostics().expect("diagnostics");
        if d["core"]["exe"].is_null() {
            let warnings: Vec<String> = d["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w.as_str().unwrap().to_string())
                .collect();
            assert!(warnings.iter().any(|w| w.contains("内核")), "{warnings:?}");
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unknown_flag_is_usage_error_not_gui() {
        // 关键回归：早先任何 `--xxx` 都放行去开界面，脚本里拼错一个字母就变成
        // 「命令不返回、屏幕上多一个窗口」。
        assert_eq!(parse_args(&args(&["--bad"])).err(), Some(EXIT_USAGE));
        assert_eq!(parse_args(&args(&["--diag"])).err(), Some(EXIT_USAGE));
        assert_eq!(
            parse_args(&args(&["--diagnostics", "--nope"])).err(),
            Some(EXIT_USAGE)
        );
    }

    #[test]
    fn gui_flags_pass_through_and_keep_scanning() {
        let f = parse_args(&args(&["--silent", "--diagnostics", "--json"])).expect("应放行");
        assert!(f.want_diag && f.json && !f.want_version);
        // 只有界面开关时什么都不做 → 回到图形界面
        let f = parse_args(&args(&["--silent"])).expect("应放行");
        assert!(!f.want_diag && f.validate.is_none() && f.tree.is_none() && !f.want_help);
    }

    #[test]
    fn value_flags_require_a_value() {
        for flag in ["--validate-config", "--export-accessibility-tree"] {
            assert_eq!(
                parse_args(&args(&[flag])).err(),
                Some(EXIT_USAGE),
                "{flag} 缺值"
            );
        }
        let f = parse_args(&args(&["--validate-config", "x.conf"])).expect("带值应放行");
        assert_eq!(f.validate.as_deref(), Some("x.conf"));
    }
}
