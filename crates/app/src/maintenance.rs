//! 维护页的全部动作：rclone 内核、应用/插件更新、插件商店、备份同步、同步任务、诊断。
//!
//! 界面只在 [`MaintenanceG`] 上发一个 `maintenance-action(名字, id)`，具体做什么全在这里。
//! 每个动作都必须真的做事，并把结果写回对应的 `*-status` 字段——界面没有别的反馈渠道，
//! 一个动作静默失败，用户看到的就是"点了没反应"。
//!
//! 约定：
//! - 面向用户的文案走 `rcgui_i18n::t` / `f`；日志与 status 里**不出现凭据**。
//! - managed 内核的端口与密码是每次启动随机生成的，只留在内存（`core::Running`），
//!   绝不写进 `config.json`——否则下一次启动会拿一个已死的密码去连。
//! - 同一个动作重入时（连点）必须自己挡掉：新增/更新/拉取都有各自的闸门。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use rcgui_i18n::{f, t};
use slint::{ComponentHandle as _, ModelRc, VecModel, Weak};

use crate::config::SyncTask;
use crate::core;
use crate::plugins;
use crate::store;
use crate::sync_tasks;
use crate::update;
use crate::{Cmd, Ctx, MainWindow, MaintenanceG, StoreRow, SyncRow};

/// 同步任务自动执行的采样间隔。到期判断本身很便宜，但 `config::save` 会写盘，
/// 所以这里只在真的有任务到点时才写。
const DUE_TICK: Duration = Duration::from_secs(30);

/// 运行中任务的状态查询间隔。比 `DUE_TICK` 密：任务的成败要跟界面贴得比较近。
const POLL_TICK: Duration = Duration::from_secs(2);

/// 通往界面命令队列的入口。
///
/// 维护动作里有一件事只能由界面线程做：插件列表重扫（装完插件后要重新读磁盘）。
/// 把发送端存在这里，是为了不让每个函数都多一个 `tx` 参数——`handle` 的调用点
/// 在 `handle_cmd_inner` 里，那里本来没有这个发送端。
static TX: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Cmd>>> = Mutex::new(None);

/// 启动时由 `main` 调用一次。没安装时所有 `send` 都静默丢弃。
pub fn install_tx(tx: tokio::sync::mpsc::UnboundedSender<Cmd>) {
    *TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
}

fn send(cmd: Cmd) {
    let guard = TX.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(cmd);
    }
}

/// 商店条目在内存里放多久。刷新是显式动作，不需要过期时间——只在刷新成功后整体替换。
pub struct Maintenance {
    /// rclone 内核（自己起的那个）。外部实例永远不经过这里。
    pub core: core::Core,
    /// 同步任务的"正在跑哪些 job"，与 `cfg.sync_tasks` 分开存：进程重启后不残留。
    pub engine: sync_tasks::Engine,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// 最近一次商店清单。空 = 还没刷新过（商店页显示"未刷新"而不是"没有插件"）。
    store_entries: Vec<store::StoreEntry>,
    /// 检查到的可用更新，等用户点"立即更新"再用。只存内存：更新检查不该在重启后自动应用。
    pending_update: Option<update::UpdateInfo>,
    last_due: Option<Instant>,
    last_poll: Option<Instant>,
    /// 上一次推给界面的同步行指纹，用来避免每秒重设 model 导致列表闪烁/丢选中。
    sync_fingerprint: String,
    /// 内核安装/更新进度，避免同一个安装动作并发重入。
    busy_core: bool,
    busy_store: bool,
}

impl Maintenance {
    pub fn new() -> Self {
        Self {
            core: core::Core::new(),
            engine: sync_tasks::Engine::new(),
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // 状态里只有缓存与标志位；某个线程 panic 之后继续用旧值，比让整个维护页瘫痪安全。
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for Maintenance {
    fn default() -> Self {
        Self::new()
    }
}

// ───────────────────────────── 界面写回 ─────────────────────────────

fn with_ui(ui: &Weak<MainWindow>, f: impl FnOnce(&MainWindow) + Send + 'static) {
    let _ = ui.upgrade_in_event_loop(move |u| f(&u));
}

fn set_core_status(ui: &Weak<MainWindow>, text: impl Into<String>) {
    let text: String = text.into();
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>().set_core_status(text.into());
    });
}

fn set_update_status(ui: &Weak<MainWindow>, text: impl Into<String>) {
    let text: String = text.into();
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>().set_update_status(text.into());
    });
}

fn set_store_status(ui: &Weak<MainWindow>, text: impl Into<String>) {
    let text: String = text.into();
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>().set_store_status(text.into());
    });
}

fn set_backup_status(ui: &Weak<MainWindow>, text: impl Into<String>) {
    let text: String = text.into();
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>().set_backup_status(text.into());
    });
}

fn set_diagnostics_status(ui: &Weak<MainWindow>, text: impl Into<String>) {
    let text: String = text.into();
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>()
            .set_diagnostics_status(text.into());
    });
}

fn set_sync_status(ui: &Weak<MainWindow>, text: impl Into<String>) {
    let text: String = text.into();
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>().set_sync_status(text.into());
    });
}

fn set_busy(ui: &Weak<MainWindow>, busy: bool) {
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>().set_busy(busy);
    });
}

/// 把 `cfg` 里与维护页相关的设置灌进界面。启动时调一次，保存后不用再调。
pub fn load_into_ui(ui: &MainWindow, cfg: &crate::config::AppConfig) {
    let g = ui.global::<MaintenanceG>();
    g.set_core_mode(match cfg.core.normalized_mode() {
        "external" => "external".into(),
        _ => "managed".into(),
    });
    g.set_core_path(cfg.core.path.clone().into());
    g.set_lightweight_enabled(cfg.light.enabled);
    g.set_lightweight_minutes(crate::lightweight::clamp_minutes(cfg.light.minutes) as i32);
    g.set_backup_url(cfg.backup.url.clone().into());
    g.set_backup_user(cfg.backup.user.clone().into());
    // 密码类字段不回填：界面上一次显示的是星号就够，回填等于把密码摊在屏幕上。
    g.set_backup_pass("".into());
    let rows = sync_rows(&cfg.sync_tasks, None);
    g.set_sync_items(ModelRc::new(VecModel::from(rows)));
    g.set_sync_hours(sync_tasks::DEFAULT_HOURS as i32);
}

// ───────────────────────────── 行构造 ─────────────────────────────

/// 同步任务列表。`engine` 为 None 时表示"还没开始跑"，状态只从配置里读。
fn sync_rows(tasks: &[SyncTask], engine: Option<&sync_tasks::Engine>) -> Vec<SyncRow> {
    let now = chrono::Local::now();
    tasks
        .iter()
        .map(|task| {
            let active = engine.and_then(|e| e.active_of(&task.id));
            let running = active.is_some();
            // 等待重连的任务要单独标注，否则界面只有一个笼统的"运行中"，
            // 网络断了十分钟用户也看不出和正常传输有什么区别。
            let waiting_here = engine.map(|e| e.is_waiting(&task.id)).unwrap_or(false);
            let status = if waiting_here {
                f("{}（{}）", &[&t("运行中"), &t("等待重新连接")])
            } else if let Some(a) = active.as_ref() {
                // 带上开始时间：用户要判断"这一轮跑了多久"，只看"运行中"没法判断是不是卡住了。
                f(
                    "{}（{} 开始）",
                    &[&t("运行中"), &a.started.format("%H:%M").to_string()],
                )
            } else if !task.enabled {
                t("已停用").to_string()
            } else if let Some(reason) = task.last_status.strip_prefix("failed: ") {
                f("上次失败：{}", &[&reason])
            } else if task.last_status.is_empty() {
                t("尚未运行").to_string()
            } else {
                t("空闲").to_string()
            };
            let next = if running {
                t("等这一轮结束").to_string()
            } else if !task.enabled {
                "—".to_string()
            } else {
                sync_tasks::next_text(task, now)
            };
            SyncRow {
                id: task.id.clone().into(),
                name: display_name(task).into(),
                local: task.local.clone().into(),
                remote: task.remote.clone().into(),
                direction: sync_tasks::ui_direction(task).into(),
                enabled: task.enabled,
                status: status.into(),
                next: next.into(),
            }
        })
        .collect()
}

fn display_name(task: &SyncTask) -> String {
    if task.name.trim().is_empty() {
        // 没起名字就用目标的最后一段，否则列表里全是"（未命名）"。
        task.remote
            .split(':')
            .next_back()
            .map(|s| s.trim_matches('/'))
            .filter(|s| !s.is_empty())
            .unwrap_or(&task.remote)
            .to_string()
    } else {
        task.name.clone()
    }
}

/// 只在内容变了时才重设 model：Slint 的列表重设会丢选中行与滚动位置。
fn push_sync_rows(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let tasks = ctx.cfg.lock().unwrap().sync_tasks.clone();
    let rows = sync_rows(&tasks, Some(&ctx.maint.engine));
    let mut print = String::new();
    for r in &rows {
        print.push_str(&format!(
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\n",
            r.id, r.name, r.local, r.remote, r.direction, r.status, r.next
        ));
    }
    {
        let mut st = ctx.maint.state();
        if st.sync_fingerprint == print {
            return;
        }
        st.sync_fingerprint = print;
    }
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>()
            .set_sync_items(ModelRc::new(VecModel::from(rows)));
    });
}

fn push_store_rows(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let entries = ctx.maint.state().store_entries.clone();
    let installed = store::scan_installed(&plugins::plugins_dir(), &plugins::gui_version());
    let rows: Vec<StoreRow> = store::rows(&entries, &installed)
        .into_iter()
        .map(|r| StoreRow {
            id: r.id.into(),
            name: r.name.into(),
            version: r.version.into(),
            description: r.description.into(),
            installed: r.installed,
            update: r.update,
            permissions: r.permissions.into(),
        })
        .collect();
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>()
            .set_store_items(ModelRc::new(VecModel::from(rows)));
    });
}

// ───────────────────────────── 内核 ─────────────────────────────

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 内核状态串：来源 + 版本 + 在跑还是停了。界面就靠这一行判断"到底有没有内核"。
pub fn core_status_text(ctx: &Arc<Ctx>) -> String {
    let phase = ctx.maint.core.phase();
    if let core::Phase::Running { .. } = phase {
        return phase.label();
    }
    let cfg = ctx.cfg.lock().unwrap().clone();
    let data = crate::config::data_dir();
    match core::resolve(&cfg.core.path, &exe_dir(), &data) {
        Ok(Some(located)) => {
            let version =
                core::probe_version(&located.exe).unwrap_or_else(|_| t("版本未知").to_string());
            f(
                "已找到内核（{}）：{} · {}",
                &[&located.source.label(), &located.exe.display(), &version],
            )
        }
        Ok(None) => format!(
            "{} — {}",
            phase.label(),
            t("没找到 rclone，可以点「下载内核」自动装一份")
        ),
        Err(e) => format!("{} — {e}", phase.label()),
    }
}

pub fn refresh_core_status(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let text = core_status_text(ctx);
    set_core_status(ui, text);
}

/// 按配置启动托管内核。返回 Some(说明) 表示真的起了进程。
///
/// 只在 `core.mode == "managed"` 且能找到内核时动手；找不到、起不来都只返回原因，
/// 由调用方写进界面并继续用外部连接——启动失败不该把整个程序卡死。
pub async fn start_managed_core(ctx: &Arc<Ctx>) -> Option<String> {
    let cfg = ctx.cfg.lock().unwrap().clone();
    if cfg.core.normalized_mode() != "managed" {
        return None;
    }
    let data = crate::config::data_dir();
    let located = match core::resolve(&cfg.core.path, &exe_dir(), &data) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return Some(
                t("没找到 rclone 内核，已跳过自动启动（可在维护页指定路径或下载）").to_string(),
            )
        }
        Err(e) => return Some(e.to_string()),
    };
    if let Err(e) = std::fs::create_dir_all(core::data_core_dir(&data)) {
        return Some(f("无法创建内核目录：{}", &[&e]));
    }
    let spec = core::StartSpec {
        exe: located.exe.clone(),
        config: core::default_config_path(),
        log_file: crate::logging::log_dir(&data).join("rclone.log"),
        extra_args: Vec::new(),
    };
    match ctx.maint.core.start(&spec) {
        Ok(()) => match ctx.maint.core.endpoint() {
            Some((url, _, _)) => {
                // 换了内核就等于换了一组作业号：老一代的 jobid 再拿去查会指到
                // 别的传输上，所以这里必须先换代（见 sync_tasks::Engine::on_core_started）。
                let gen = ctx.maint.engine.on_core_started();
                crate::logging::event(
                    "info",
                    "core",
                    &format!(
                        "托管内核已启动（pid={}，代次={gen}）",
                        ctx.maint.core.pid().unwrap_or(0)
                    ),
                );
                Some(f(
                    "内核已启动：{}（{}，由本程序管理）",
                    &[&url, &located.source.label()],
                ))
            }
            None => Some(t("内核进程已启动，但拿不到它的连接信息").to_string()),
        },
        Err(e) => {
            crate::logging::event(
                "error",
                "core",
                &format!("start {} failed: {e:#}", located.exe.display()),
            );
            Some(f("内核启动失败：{}", &[&e]))
        }
    }
}

// ───────────────────────────── 动作分发 ─────────────────────────────

/// 处理一次 `maintenance-action`。返回 false 表示不进 rclone 状态机（与其它 Cmd 一致）。
pub async fn handle(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, action: &str, id: &str) -> bool {
    let ui = ui.clone();
    match action {
        "core-detect" => core_detect(ctx, &ui).await,
        "core-install" => core_install(ctx, &ui).await,
        "core-start" => core_start(ctx, &ui).await,
        "core-stop" => core_stop(ctx, &ui).await,
        "core-save" => core_save(ctx, &ui).await,
        "check-update" => check_update(ctx, &ui).await,
        "apply-update" => apply_update(ctx, &ui).await,
        "store-refresh" => store_refresh(ctx, &ui).await,
        "store-install" => store_install(ctx, &ui, id, false).await,
        "store-update" => store_install(ctx, &ui, id, true).await,
        "backup-save" => backup_save(ctx, &ui).await,
        "backup-push" => backup_push(ctx, &ui).await,
        "backup-pull" => backup_pull(ctx, &ui).await,
        "sync-save" => sync_save(ctx, &ui, id).await,
        "sync-run" => sync_run(ctx, &ui, id).await,
        "sync-toggle" => sync_toggle(ctx, &ui, id).await,
        "sync-delete" => sync_delete(ctx, &ui, id).await,
        "diagnostics" => diagnostics(ctx, &ui).await,
        other => {
            // 界面与会话之间只靠字符串约定；不认识的名字别静默吞掉。
            set_core_status(&ui, f("未知的维护动作：{}", &[&other]));
            crate::logging::event("warn", "maintenance", &format!("未知动作 {other}"));
        }
    }
    false
}

// ── 内核 ──

async fn core_detect(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let cfg = ctx.cfg.lock().unwrap().clone();
    let data = crate::config::data_dir();
    let text = match core::resolve(&cfg.core.path, &exe_dir(), &data) {
        Ok(Some(l)) => {
            let version = core::probe_version(&l.exe).unwrap_or_else(|_| t("版本未知").to_string());
            f(
                "找到内核（{}）：{} · {}",
                &[&l.source.label(), &l.exe.display(), &version],
            )
        }
        Ok(None) => t("没找到 rclone：程序目录 core/、数据目录 core/、PATH 里都没有").to_string(),
        Err(e) => e.to_string(),
    };
    set_core_status(ui, text);
    refresh_core_status(ctx, ui);
}

async fn core_install(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    {
        let mut st = ctx.maint.state();
        if st.busy_core {
            set_core_status(ui, t("内核安装已在进行中，请稍等"));
            return;
        }
        st.busy_core = true;
    }
    set_busy(ui, true);
    let data = crate::config::data_dir();
    let target = core::data_core_dir(&data);
    // 安装目录由数据目录拼出来，正常情况下不会有问题；但这条路径是唯一会把 rclone.exe
    // 覆盖到别处的地方（相对路径、`..` 都能让它指到别处）。先验证再下载，别等下载完了
    // 才发现目录不合法。
    if let Err(e) = core::guard_target_dir(&target) {
        ctx.maint.state().busy_core = false;
        set_busy(ui, false);
        set_core_status(ui, f("内核安装目录不可用：{}", &[&e]));
        crate::logging::event("error", "core", &format!("安装目录被拒绝：{e}"));
        refresh_core_status(ctx, ui);
        return;
    }
    let version = {
        let cfg = ctx.cfg.lock().unwrap();
        if cfg.core.last_version.trim().is_empty() {
            None
        } else {
            Some(cfg.core.last_version.clone())
        }
    };
    // 替换内核要把旧 exe 重命名成 .bak，Windows 会拒绝正在运行的那个文件。所以先停掉
    // **我们自己启动的**内核；外部接入的实例一律不碰（那个 exe 本来也不是我们要替换的
    // 文件，而且规则是永不结束不是自己启动的进程）。
    let was_running = ctx.maint.core.is_running();
    if was_running {
        ctx.maint.core.stop();
        crate::disconnect_now(ctx, ui).await;
    }
    set_core_status(
        ui,
        if was_running {
            t("已停止自家内核，正在下载官方 rclone 内核…")
        } else {
            t("正在下载官方 rclone 内核…")
        },
    );
    let result = core::install_official(&target, version.as_deref()).await;
    ctx.maint.state().busy_core = false;
    set_busy(ui, false);
    match result {
        Ok(exe) => {
            let v = core::probe_version(&exe).unwrap_or_else(|_| t("版本未知").to_string());
            {
                let mut cfg = ctx.cfg.lock().unwrap();
                // 装了就用这个：把模式切回 managed，否则用户装了内核却还是连外部实例。
                cfg.core.mode = "managed".to_string();
                cfg.core.path = String::new();
                cfg.core.last_version = v.clone();
                let _ = crate::config::save(&cfg);
            }
            with_ui(ui, |u| {
                u.global::<MaintenanceG>().set_core_mode("managed".into());
                u.global::<MaintenanceG>().set_core_path("".into());
            });
            crate::logging::event("info", "core", &format!("官方内核已安装：{}", v));
            set_core_status(ui, f("内核已安装：{}（{}）", &[&exe.display(), &v]));
            if was_running {
                // 装了内核的人下一步一定是启动，替他重启并把结果说清楚，
                // 免得"装完内核却停着"看起来像失败。
                let started = start_managed_core(ctx).await;
                match &started {
                    Some(msg) => {
                        set_core_status(ui, f("内核已安装（{}）并已重启：{}", &[&v, msg]));
                        crate::toast(ui, 1, t("内核已更新并重新启动"));
                    }
                    None => {
                        set_core_status(
                            ui,
                            f("内核已安装（{}），但没能重新启动，请看下面的状态", &[&v]),
                        );
                        crate::toast(ui, 2, t("内核已更新，但自动重启失败，请点「启动内核」"));
                    }
                }
            } else {
                crate::toast(ui, 1, t("内核已安装，点「启动内核」开始使用"));
            }
        }
        Err(e) => {
            crate::logging::event("error", "core", &format!("安装内核失败：{e}"));
            set_core_status(ui, f("内核安装失败：{}", &[&e]));
            crate::toast(ui, 2, f("内核安装失败：{}", &[&e]));
        }
    }
    refresh_core_status(ctx, ui);
}

async fn core_start(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    if ctx.maint.core.is_running() {
        set_core_status(ui, t("内核已经在运行"));
        return;
    }
    set_core_status(ui, t("正在启动内核…"));
    match start_managed_core(ctx).await {
        Some(msg) => set_core_status(ui, msg),
        None => set_core_status(ui, t("当前是 external 模式，不会启动内核")),
    }
    if ctx.maint.core.is_running() {
        // 起来之后立刻握手一次：界面上的"已连接"要跟着变，不能让用户再点一次连接。
        let outcome = crate::connect_report(ctx).await;
        let live = outcome.view.live;
        crate::apply_conn_view(ui, outcome.view);
        if live {
            crate::toast(ui, 1, t("内核已启动并连接"));
        }
    }
    refresh_core_status(ctx, ui);
}

async fn core_stop(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let was = ctx.maint.core.is_running();
    if was {
        ctx.maint.core.stop();
    }
    // 停掉自己起的进程后连接必然失效；只动自己拥有的 child，外部实例一概不碰。
    crate::disconnect_now(ctx, ui).await;
    set_core_status(
        ui,
        if was {
            t("内核已停止")
        } else {
            t("内核本来就没在运行")
        },
    );
    refresh_core_status(ctx, ui);
}

async fn core_save(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let picked = crate::read_ui(ui, |u: MainWindow| {
        (
            u.global::<MaintenanceG>().get_core_mode().to_string(),
            u.global::<MaintenanceG>().get_core_path().to_string(),
        )
    })
    .await;
    let Some((mode, path)) = picked else {
        set_core_status(ui, t("读取界面设置失败"));
        return;
    };
    let mode = if mode == "external" {
        "external"
    } else {
        "managed"
    }
    .to_string();
    let path = path.trim().to_string();
    if !path.is_empty() {
        let p = Path::new(&path);
        if !p.is_file() {
            set_core_status(
                ui,
                f(
                    "指定的内核路径不是一个文件：{}（要么改对，要么清空改用自动查找）",
                    &[&path],
                ),
            );
            return;
        }
    }
    {
        let mut cfg = ctx.cfg.lock().unwrap();
        cfg.core.mode = mode.clone();
        cfg.core.path = path.clone();
    }
    let save_result = {
        let cfg = ctx.cfg.lock().unwrap().clone();
        crate::config::save(&cfg)
    };
    match save_result {
        Ok(()) => {
            let running = ctx.maint.core.is_running();
            let mut msg = if mode == "managed" {
                t("已保存：内核由本程序管理").to_string()
            } else {
                t("已保存：外部模式（只连接别人启动的 rclone，不会起停进程）").to_string()
            };
            if running && mode == "external" {
                msg.push_str(&f(
                    "　注意：本程序启动的内核还在运行（pid {}），需要的话点「停止内核」。",
                    &[&ctx.maint.core.pid().unwrap_or(0)],
                ));
            }
            set_core_status(ui, msg);
        }
        Err(e) => set_core_status(ui, f("保存内核设置失败：{}", &[&e])),
    }
    refresh_core_status(ctx, ui);
}

// ── 应用更新 ──

/// 当前安装形态：便携包解压目录里有 `portable` 标记文件。
fn update_kind() -> update::Kind {
    if crate::config::data_dir()
        .parent()
        .map(|d| d.join("portable").exists())
        .unwrap_or(false)
    {
        update::Kind::Portable
    } else {
        update::Kind::Setup
    }
}

async fn check_update(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    if update::try_acquire().is_none() {
        set_update_status(ui, t("已经有更新动作在进行"));
        return;
    }
    let kind = update_kind();
    set_update_status(ui, t("正在查询 GitHub 最新发布…"));
    let current = update::current_version().to_string();
    match update::check(&current, kind).await {
        Ok(info) => {
            let text = f(
                "当前 {} · 最新 {}（{}）",
                &[&info.current, &info.latest, &kind.as_str()],
            );
            let has_new = update::is_newer(&info.latest, &info.current);
            let final_text = if has_new {
                f("{} — 可以更新", &[&text])
            } else {
                f("{} — 已是最新", &[&text])
            };
            ctx.maint.state().pending_update = Some(info);
            set_update_status(ui, final_text);
        }
        Err(e) => {
            ctx.maint.state().pending_update = None;
            // 界面上只有一行状态，用户回头要追为什么查不到更新时只有日志能对上时间点。
            crate::logging::event("error", "update", &format!("check_update failed: {e:#}"));
            set_update_status(ui, f("检查更新失败：{}", &[&e]));
        }
    }
    // 插件更新与程序更新一起给出结论：用户点的是一次"检查更新"。
    let entries = ctx.maint.state().store_entries.clone();
    if entries.is_empty() {
        set_update_status(ui, t("程序更新已检查（插件更新需要先刷新插件商店）"));
    } else {
        let installed = store::scan_installed(&plugins::plugins_dir(), &plugins::gui_version());
        let targets = update::plugin_update_targets(&entries, &installed);
        if !targets.is_empty() {
            let names: Vec<String> = targets.iter().map(|e| e.name.clone()).collect();
            set_update_status(
                ui,
                f(
                    "有 {} 个插件可更新：{}",
                    &[&(targets.len()), &(names.join("、"))],
                ),
            );
        }
    }
}

async fn apply_update(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let info = ctx.maint.state().pending_update.clone();
    let Some(info) = info else {
        set_update_status(ui, t("先点「检查更新」，确认有新版本后再更新"));
        return;
    };
    let Some(_guard) = update::try_acquire() else {
        set_update_status(ui, t("已经有更新动作在进行"));
        return;
    };
    if !update::is_newer(&info.latest, &info.current) {
        set_update_status(ui, t("当前已是最新版本，没有可安装的更新"));
        return;
    }
    let staging = crate::config::data_dir().join("updates");
    set_update_status(ui, t("正在下载并校验更新包…"));
    let staged = match update::stage(&info, &staging).await {
        Ok(s) => s,
        Err(e) => {
            crate::logging::event(
                "error",
                "update",
                &format!("stage {} -> {} failed: {e:#}", info.current, info.latest),
            );
            set_update_status(ui, f("下载更新失败：{}", &[&e]));
            return;
        }
    };
    match update::apply(&staged) {
        Ok(path) => {
            crate::logging::event(
                "info",
                "update",
                &format!(
                    "applied {} ({}) via {}",
                    staged.version,
                    staged.kind.as_str(),
                    path.display()
                ),
            );
            match staged.kind {
                update::Kind::Portable => {
                    let text = f(
                        "已排好替换脚本：{}。程序将在 3 秒后退出，文件替换在退出后完成，重新打开即是新版本。",
                        &[&path.display()],
                    );
                    set_update_status(ui, text.clone());
                    crate::toast(ui, 1, text);
                    // 替换脚本最多等本进程 120 秒，超时就放弃并留下旧文件。这里主动退出，
                    // 否则用户关掉窗口才发现"点了没反应"，而脚本早已经在等超时。
                    tokio::spawn(async {
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        send(Cmd::Quit);
                    });
                }
                update::Kind::Setup => {
                    let text = f(
                        "已启动安装程序：{}。按提示完成后重新打开本程序即可。",
                        &[&path.display()],
                    );
                    set_update_status(ui, text.clone());
                    crate::toast(ui, 1, text);
                }
            }
        }
        Err(e) => {
            crate::logging::event(
                "error",
                "update",
                &format!("apply {} failed: {e:#}", staged.version),
            );
            set_update_status(ui, f("应用更新失败：{}", &[&e]));
        }
    }
}

// ── 插件商店 ──

async fn store_refresh(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    {
        let mut st = ctx.maint.state();
        if st.busy_store {
            set_store_status(ui, t("商店刷新已在进行中"));
            return;
        }
        st.busy_store = true;
    }
    set_busy(ui, true);
    set_store_status(ui, t("正在拉取商店清单…"));
    let result = store::fetch_index(store::DEFAULT_STORE_URL).await;
    set_busy(ui, false);
    match result {
        Ok(parsed) => {
            let count = parsed.entries.len();
            let warnings = parsed.warnings.clone();
            {
                let mut st = ctx.maint.state();
                st.busy_store = false;
                st.store_entries = parsed.entries;
            }
            push_store_rows(ctx, ui);
            // 清单里被跳过的条目要一起报出来：它们不会出现在列表里，静默丢弃
            // 会让用户以为商店只有这么多插件。
            if warnings.is_empty() {
                set_store_status(ui, f("商店已刷新，共 {} 个插件", &[&count]));
            } else {
                set_store_status(
                    ui,
                    f(
                        "商店已刷新，共 {} 个插件；跳过了 {} 条：{}",
                        &[&count, &(warnings.len()), &(warnings.join("；"))],
                    ),
                );
            }
        }
        Err(e) => {
            ctx.maint.state().busy_store = false;
            crate::logging::event("error", "store", &format!("refresh failed: {e:#}"));
            set_store_status(ui, f("商店刷新失败：{}", &[&e]));
        }
    }
}

async fn store_install(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, id: &str, expect_update: bool) {
    if id.trim().is_empty() {
        set_store_status(ui, t("没指定要装哪个插件"));
        return;
    }
    let entry = {
        let st = ctx.maint.state();
        st.store_entries.iter().find(|e| e.id == id).cloned()
    };
    let Some(entry) = entry else {
        set_store_status(ui, f("商店里没有 {}，请先刷新", &[&id]));
        return;
    };
    let plugins_root = plugins::plugins_dir();
    let installed = store::scan_installed(&plugins_root, &plugins::gui_version());
    let already = installed.iter().any(|i| i.id == entry.id);
    if expect_update && !already {
        set_store_status(ui, f("{} 还没装，不能更新", &[&entry.name]));
        return;
    }
    if !expect_update && already {
        set_store_status(ui, f("{} 已经装过了，要换版本请点更新", &[&entry.name]));
        return;
    }
    {
        let mut st = ctx.maint.state();
        if st.busy_store {
            set_store_status(ui, t("商店动作已在进行中"));
            return;
        }
        st.busy_store = true;
    }
    set_busy(ui, true);
    set_store_status(
        ui,
        f(
            "正在下载并校验 {} {}…（权限：{}）",
            &[
                &entry.name,
                &entry.version,
                &if entry.permissions_text().is_empty() {
                    t("无额外权限").to_string()
                } else {
                    entry.permissions_text()
                },
            ],
        ),
    );
    let tmp = crate::config::data_dir().join("store-tmp");
    let gui_version = plugins::gui_version();
    let result = store::install(&entry, &plugins_root, &tmp, &gui_version).await;
    set_busy(ui, false);
    ctx.maint.state().busy_store = false;
    match result {
        Ok(outcome) => {
            // 装完立刻让插件库重新扫一遍：否则新插件在下一次重启前都看不见。
            let _ = ctx.store.invalidate(&entry.id);
            send(Cmd::PluginsRescan);
            push_store_rows(ctx, ui);
            // Kept 意味着商店给的版本不比对已装的高，实际什么也没换：
            // 这时候说"已安装"会让用户以为升级成功了。
            let message = outcome.message();
            let replaced = !matches!(outcome, plugin_host::package::InstallOutcome::Kept { .. });
            if replaced {
                set_store_status(
                    ui,
                    f(
                        "{}（未自动启用，请到插件页确认权限后自行启用）",
                        &[&message],
                    ),
                );
            } else {
                set_store_status(ui, message.clone());
            }
            crate::logging::event(
                "info",
                "store",
                &format!("install {}: {}", entry.id, message),
            );
        }
        Err(e) => {
            crate::logging::event(
                "error",
                "store",
                &format!("install {} failed: {e:#}", entry.id),
            );
            set_store_status(ui, f("安装失败：{}", &[&e]));
        }
    }
}

// ── 备份（WebDAV） ──

fn backup_fields(ctx: &Arc<Ctx>) -> (String, String, String) {
    let cfg = ctx.cfg.lock().unwrap();
    (
        cfg.backup.url.clone(),
        cfg.backup.user.clone(),
        cfg.backup.pass.clone(),
    )
}

async fn backup_save(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let picked = crate::read_ui(ui, |u: MainWindow| {
        let g = u.global::<MaintenanceG>();
        (
            g.get_backup_url().to_string(),
            g.get_backup_user().to_string(),
            g.get_backup_pass().to_string(),
        )
    })
    .await;
    let Some((url, user, pass)) = picked else {
        set_backup_status(ui, t("读取界面设置失败"));
        return;
    };
    let url = url.trim().to_string();
    if url.is_empty() {
        {
            let mut cfg = ctx.cfg.lock().unwrap();
            cfg.backup.url = String::new();
            cfg.backup.user = user.trim().to_string();
            // 密码留空表示"不动已保存的密码"，清空需要显式把地址也清掉。
            if !pass.is_empty() {
                cfg.backup.pass = pass;
            }
        }
        let save = {
            let cfg = ctx.cfg.lock().unwrap().clone();
            crate::config::save(&cfg)
        };
        match save {
            Ok(()) => set_backup_status(ui, t("已关闭自动备份（地址为空）")),
            Err(e) => set_backup_status(ui, f("保存失败：{}", &[&e])),
        }
        return;
    }
    if let Err(e) = crate::backup::check_url(&url) {
        set_backup_status(ui, f("地址不可用：{}", &[&e]));
        return;
    }
    {
        let mut cfg = ctx.cfg.lock().unwrap();
        cfg.backup.url = url.clone();
        cfg.backup.user = user.trim().to_string();
        if !pass.is_empty() {
            cfg.backup.pass = pass;
        }
    }
    let save = {
        let cfg = ctx.cfg.lock().unwrap().clone();
        crate::config::save(&cfg)
    };
    match save {
        Ok(()) => set_backup_status(
            ui,
            f(
                "已保存备份地址：{}",
                &[&crate::backup::describe_target(&url)],
            ),
        ),
        Err(e) => set_backup_status(ui, f("保存失败：{}", &[&e])),
    }
}

/// 导出当前配置成一份 rcgui.conf 字节（与 `export_conf` 同一套来源）。
async fn bundle_bytes(ctx: &Arc<Ctx>, include_secrets: bool) -> Result<Vec<u8>> {
    let (dump, live_serves, live_mounts) = if ctx.rc.is_live() {
        let client = ctx.rc.client().await;
        let dump = client.config_dump().await.ok();
        let serves = client
            .serve_list()
            .await
            .ok()
            .map(|v| crate::conf_io::live_serves_from(&v))
            .unwrap_or_default();
        let mounts = client
            .mounts_raw()
            .await
            .ok()
            .map(|v| crate::conf_io::live_mounts_from(&v))
            .unwrap_or_default();
        (dump, serves, mounts)
    } else {
        (None, Vec::new(), Vec::new())
    };
    let cfg = ctx.cfg.lock().unwrap().clone();
    let providers = crate::get_providers(ctx).await;
    let plugin_refs = plugins::bundle_refs(ctx);
    let bundle = crate::conf_io::build_bundle(
        &cfg,
        dump.as_ref(),
        include_secrets,
        Some(&providers),
        &live_serves,
        &live_mounts,
        &plugin_refs,
    );
    let mut out = Vec::new();
    crate::conf_io::write_bundle_to(&mut out, &bundle)?;
    Ok(out)
}

async fn backup_push(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let (url, user, pass) = backup_fields(ctx);
    if url.trim().is_empty() {
        set_backup_status(ui, t("先填备份地址并保存"));
        return;
    }
    let include_secrets = crate::read_ui(ui, |u: MainWindow| {
        u.global::<MaintenanceG>().get_backup_include_secrets()
    })
    .await
    .unwrap_or(false);
    // 整段（导出 + 本地副本 + 上传）都由同一个 Session 占着闸门：
    // 中途别处再点一次上传/下载会被挡在门外，不会两边互相覆盖。
    let Ok(session) = crate::backup::Session::begin() else {
        set_backup_status(ui, t("已有备份动作在进行"));
        return;
    };
    set_busy(ui, true);
    set_backup_status(ui, t("正在生成导出包…"));
    let bytes = match bundle_bytes(ctx, include_secrets).await {
        Ok(b) => b,
        Err(e) => {
            set_busy(ui, false);
            crate::logging::event("error", "backup", &format!("push bundle failed: {e:#}"));
            set_backup_status(ui, f("生成导出包失败：{}", &[&e]));
            return;
        }
    };
    let size = bytes.len();
    // 本地留一份：远端失败时用户还有东西可拿。
    let local = session.save(
        &crate::config::data_dir(),
        &crate::backup::local_name_for(&url),
        &bytes,
    );
    set_backup_status(ui, t("正在上传…"));
    let result = session.push(&url, &user, &pass, bytes).await;
    set_busy(ui, false);
    match result {
        Ok(()) => {
            let extra = match local {
                Ok(p) => f("（本地副本：{}）", &[&p.display()]),
                Err(_) => String::new(),
            };
            set_backup_status(
                ui,
                f(
                    "已上传 {} 字节{}{}",
                    &[
                        &size,
                        &extra,
                        &if include_secrets {
                            "，包含凭据"
                        } else {
                            "，不含凭据"
                        },
                    ],
                ),
            );
        }
        // 只记失败原因（日志层会把 URL 里的 userinfo、token 之类遮掉），不记内容本身：
        // 导出包里可能有 OAuth 令牌，字节数够了，不需要把它写进日志。
        Err(e) => {
            crate::logging::event(
                "error",
                "backup",
                &format!("push {} bytes failed: {e:#}", size),
            );
            set_backup_status(ui, f("上传失败：{}", &[&e]));
        }
    }
}

async fn backup_pull(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let (url, user, pass) = backup_fields(ctx);
    if url.trim().is_empty() {
        set_backup_status(ui, t("先填备份地址并保存"));
        return;
    }
    // 下载、落盘副本、解析成预览都在这一个 Session 里完成：预览期间闸门不放开，
    // 否则用户核对到一半又被另一次上传替换掉远端内容，看到的就不是同一份东西了。
    let Ok(session) = crate::backup::Session::begin() else {
        set_backup_status(ui, t("已有备份动作在进行"));
        return;
    };
    set_busy(ui, true);
    set_backup_status(ui, t("正在下载备份…"));
    let data = crate::config::data_dir();
    let name = crate::backup::local_name_for(&url);
    let fetched = session.pull(&url, &user, &pass).await;
    set_busy(ui, false);
    let (bytes, saved) = match fetched {
        Ok(b) => {
            let saved = session.save(&data, &name, &b);
            (b, saved)
        }
        Err(e) => {
            crate::logging::event("error", "backup", &format!("pull failed: {e:#}"));
            set_backup_status(ui, f("下载失败：{}", &[&e]));
            return;
        }
    };
    // 拉下来的只是候选：走与"导入文件"同一条预览路径，队列进 pending_import，
    // 用户在设置页点「应用导入」才算真的改本机配置。
    let cfg = ctx.cfg.lock().unwrap().clone();
    let current = crate::profiles::current_doc(&cfg);
    let source = saved.unwrap_or_else(|_| PathBuf::from(&name));
    match crate::conf_io::preview_bytes(&bytes, &source, &current) {
        Ok((conf, preview)) => {
            let info = crate::conf_io::describe_detail(&preview);
            *ctx.pending_import.lock().unwrap() = Some(conf);
            let info2 = info.clone();
            with_ui(ui, move |u| {
                let g = u.global::<crate::SettingsG>();
                g.set_conf_pending(true);
                g.set_conf_info(info2.into());
            });
            set_backup_status(
                ui,
                f(
                    "已下载 {} 字节，尚未写入；去设置页核对后点「应用导入」",
                    &[&bytes.len()],
                ),
            );
        }
        Err(e) => set_backup_status(ui, f("备份文件无法解析：{}", &[&e])),
    }
}

// ── 同步任务 ──

fn direction_from_ui(v: i32) -> &'static str {
    if v == 1 {
        sync_tasks::DIR_DOWN
    } else {
        sync_tasks::DIR_UP
    }
}

/// 新建任务的 id：时间戳 + 进程内自增，保证同一秒内连点两次也不会撞。
fn new_task_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "sync-{}-{}",
        chrono::Local::now().format("%Y%m%d%H%M%S"),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

async fn sync_save(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, id: &str) {
    let picked = crate::read_ui(ui, |u: MainWindow| {
        let g = u.global::<MaintenanceG>();
        (
            g.get_sync_name().to_string(),
            g.get_sync_local().to_string(),
            g.get_sync_remote().to_string(),
            g.get_sync_direction(),
            g.get_sync_hours(),
        )
    })
    .await;
    let Some((name, local, remote, direction, hours)) = picked else {
        set_sync_status(ui, t("读取界面设置失败"));
        return;
    };
    let editing = !id.trim().is_empty();
    let task = SyncTask {
        id: if editing {
            id.trim().to_string()
        } else {
            new_task_id()
        },
        name: name.trim().to_string(),
        local: local.trim().to_string(),
        remote: remote.trim().to_string(),
        direction: direction_from_ui(direction).to_string(),
        // 界面这一版没有镜像选项：只做 copy。要删目标必须另外显式开启。
        mode: sync_tasks::MODE_COPY.to_string(),
        enabled: true,
        interval_hours: hours.max(0) as u32,
        last_run: String::new(),
        last_status: String::new(),
    };
    if let Err(e) = sync_tasks::validate(&task) {
        set_sync_status(ui, f("任务没保存：{}", &[&e]));
        return;
    }
    {
        let mut cfg = ctx.cfg.lock().unwrap();
        if editing {
            let Some(slot) = cfg.sync_tasks.iter_mut().find(|t| t.id == task.id) else {
                set_sync_status(ui, f("找不到要修改的任务：{}", &[&id]));
                return;
            };
            // 保留原有的运行记录：改个间隔不该把"上次跑成没跑成"擦掉。
            let last_run = slot.last_run.clone();
            let last_status = slot.last_status.clone();
            *slot = task.clone();
            slot.last_run = last_run;
            slot.last_status = last_status;
        } else {
            cfg.sync_tasks.push(task.clone());
        }
        if let Err(e) = crate::config::save(&cfg) {
            set_sync_status(ui, f("保存失败：{}", &[&e]));
            return;
        }
    }
    with_ui(ui, |u| {
        let g = u.global::<MaintenanceG>();
        // 保存成功后把输入框清空，避免用户以为还没存上。
        g.set_sync_name("".into());
        g.set_sync_local("".into());
        g.set_sync_remote("".into());
    });
    push_sync_rows(ctx, ui);
    set_sync_status(
        ui,
        if editing {
            f("任务已更新：{}", &[&display_name(&task)])
        } else {
            f("任务已添加：{}", &[&display_name(&task)])
        },
    );
}

async fn sync_run(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, id: &str) {
    let Some(task) = ctx
        .cfg
        .lock()
        .unwrap()
        .sync_tasks
        .iter()
        .find(|t| t.id == id)
        .cloned()
    else {
        set_sync_status(ui, f("找不到任务：{}", &[&id]));
        return;
    };
    if !ctx.rc.is_live() {
        set_sync_status(ui, t("还没连上 rclone，等连上再试"));
        return;
    }
    let client = ctx.rc.client().await;
    match sync_tasks::start(&client, &ctx.maint.engine, &task).await {
        Ok(jobid) => {
            // 立刻记下"已经跑过"，否则调度器 30 秒后又会看到它到期。
            let stamp = rcgui_profile::now_iso();
            {
                let mut cfg = ctx.cfg.lock().unwrap();
                if let Some(slot) = cfg.sync_tasks.iter_mut().find(|t| t.id == id) {
                    slot.last_run = stamp;
                    slot.last_status = String::new();
                }
                let _ = crate::config::save(&cfg);
            }
            push_sync_rows(ctx, ui);
            set_sync_status(
                ui,
                f(
                    "{} 已开始（job {}），传输在后台继续",
                    &[&display_name(&task), &jobid],
                ),
            );
        }
        Err(e) => {
            crate::logging::event(
                "error",
                "sync",
                &format!("start {} failed: {e:#}", display_name(&task)),
            );
            set_sync_status(ui, f("启动失败：{}", &[&e]));
        }
    }
}

async fn sync_toggle(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, id: &str) {
    let now_enabled = {
        let mut cfg = ctx.cfg.lock().unwrap();
        let Some(slot) = cfg.sync_tasks.iter_mut().find(|t| t.id == id) else {
            set_sync_status(ui, f("找不到任务：{}", &[&id]));
            return;
        };
        slot.enabled = !slot.enabled;
        let v = slot.enabled;
        let _ = crate::config::save(&cfg);
        v
    };
    push_sync_rows(ctx, ui);
    let running = ctx.maint.engine.is_active(id);
    let mut text = if now_enabled {
        t("已启用，会按间隔自动运行").to_string()
    } else {
        t("已停用，不会再自动启动").to_string()
    };
    if !now_enabled && running {
        // 停用只是不再排下一轮；正在跑的这一次让它跑完，否则用户会以为文件传了一半。
        text.push_str(&t("。正在进行的这一轮会跑完，之后不再启动。"));
    }
    set_sync_status(ui, text);
}

async fn sync_delete(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, id: &str) {
    // 先把正在跑的那一轮停掉再删记录，否则任务没了、job 还在后台写文件。
    if let Some(active) = ctx.maint.engine.active_of(id) {
        if ctx.rc.is_live() {
            let client = ctx.rc.client().await;
            if let Err(e) = client.job_stop(active.jobid).await {
                crate::logging::event(
                    "warn",
                    "sync",
                    &format!("job_stop {} failed while deleting: {e:#}", active.jobid),
                );
                set_sync_status(
                    ui,
                    f(
                        "停不掉正在跑的 job {}（{}），已保留任务记录；请等它结束后再删",
                        &[&active.jobid, &e],
                    ),
                );
                return;
            }
        }
        ctx.maint.engine.release(id);
    }
    let removed = {
        let mut cfg = ctx.cfg.lock().unwrap();
        let before = cfg.sync_tasks.len();
        cfg.sync_tasks.retain(|t| t.id != id);
        if cfg.sync_tasks.len() == before {
            return;
        }
        let _ = crate::config::save(&cfg);
        true
    };
    if removed {
        push_sync_rows(ctx, ui);
        set_sync_status(ui, t("任务已删除（目标端的文件不会被删）"));
    }
}

// ── 诊断 ──

async fn diagnostics(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    // 这是界面里的「诊断」按钮，所以能报真的运行态：轻量模式此刻是否生效、
    // 窗口隐藏了多久。命令行那边（`--diagnostics`）是另一个进程，看不到这些，
    // 报的是 offline 作用域。两边用同一份结构，字段含义一致。
    let facts = crate::cli::RuntimeFacts::in_app(
        crate::lightweight::is_light(),
        crate::lightweight::hidden_for(),
    );
    match crate::cli::diagnostics_with(facts) {
        Ok(value) => {
            let data = crate::config::data_dir();
            let dir = crate::logging::log_dir(&data);
            let _ = std::fs::create_dir_all(&dir);
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let path = dir.join(format!("diagnostics-{stamp}.json"));
            let body = serde_json::to_vec_pretty(&value).unwrap_or_default();
            let saved = crate::config::write_atomic(&path, &body);
            let plugin_count = plugins::scan_rows(ctx).await.len();
            let head = f(
                "已生成诊断快照：内核 {} · 插件 {} 个",
                &[&ctx.maint.core.phase().label(), &(plugin_count)],
            );
            match saved {
                Ok(()) => set_diagnostics_status(ui, f("{}\n{}", &[&head, &(path.display())])),
                Err(e) => set_diagnostics_status(ui, f("{}（写文件失败：{}）", &[&head, &e])),
            }
        }
        Err(e) => set_diagnostics_status(ui, f("生成诊断失败：{}", &[&e])),
    }
}

// ───────────────────────────── 同步调度 ─────────────────────────────

/// 后台定时器：到点起任务、轮询在跑的 job、刷新列表。
///
/// 轻量模式**不**停这个循环——它不依赖界面，任务该跑还得跑。
pub async fn watch(ctx: Arc<Ctx>, ui: Weak<MainWindow>) {
    loop {
        tokio::time::sleep(POLL_TICK).await;
        tick(&ctx, &ui).await;
    }
}

/// 一次调度采样。拆出来是为了能单独测"到点该不该起"。
pub async fn tick(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let now = Instant::now();
    let poll_due = {
        let st = ctx.maint.state();
        st.last_poll
            .map(|t| now.duration_since(t) >= POLL_TICK)
            .unwrap_or(true)
    };
    if poll_due {
        ctx.maint.state().last_poll = Some(now);
        poll_finished(ctx, ui).await;
    }
    let due_now = {
        let st = ctx.maint.state();
        st.last_due
            .map(|t| now.duration_since(t) >= DUE_TICK)
            .unwrap_or(true)
    };
    if due_now {
        ctx.maint.state().last_due = Some(now);
        spawn_due(ctx, ui).await;
    }
    push_sync_rows(ctx, ui);
}

/// 收尾已经结束的 job，写回 last_status。
async fn poll_finished(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    if !ctx.rc.is_live() {
        return;
    }
    if ctx.maint.engine.active_ids().is_empty() {
        return;
    }
    let client = ctx.rc.client().await;
    let finished = sync_tasks::poll(&client, &ctx.maint.engine).await;
    if finished.is_empty() {
        return;
    }
    let changed = {
        let mut cfg = ctx.cfg.lock().unwrap();
        let mut changed = false;
        for item in &finished {
            if let Some(slot) = cfg.sync_tasks.iter_mut().find(|t| t.id == item.task_id) {
                slot.last_status = item.status.clone();
                changed = true;
            }
        }
        if changed {
            let _ = crate::config::save(&cfg);
        }
        changed
    };
    if changed {
        for item in &finished {
            let level = if item.status.starts_with("failed") {
                2
            } else {
                1
            };
            let msg = if item.status == "ok" {
                f("同步任务「{}」已完成", &[&item.task_id])
            } else {
                f("同步任务「{}」失败：{}", &[&item.task_id, &item.status])
            };
            crate::toast(ui, level, msg);
        }
    }
}

/// 到点的任务起一轮。已有 job 在跑的任务跳过（不重复启动同一目录）。
async fn spawn_due(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    if !ctx.rc.is_live() {
        return;
    }
    let now = chrono::Local::now();
    let due: Vec<SyncTask> = {
        let cfg = ctx.cfg.lock().unwrap();
        cfg.sync_tasks
            .iter()
            .filter(|t| t.enabled)
            .filter(|t| !ctx.maint.engine.is_active(&t.id))
            .filter(|t| sync_tasks::is_due(t, now))
            .cloned()
            .collect()
    };
    if due.is_empty() {
        return;
    }
    let client = ctx.rc.client().await;
    let mut started = Vec::new();
    for task in due {
        match sync_tasks::start(&client, &ctx.maint.engine, &task).await {
            Ok(jobid) => started.push((task, jobid)),
            Err(e) => {
                let mut cfg = ctx.cfg.lock().unwrap();
                if let Some(slot) = cfg.sync_tasks.iter_mut().find(|t| t.id == task.id) {
                    slot.last_status = format!("failed: {e}");
                    slot.last_run = rcgui_profile::now_iso();
                }
                let _ = crate::config::save(&cfg);
                crate::logging::event(
                    "warn",
                    "sync",
                    &format!("task {} start failed: {e}", task.id),
                );
                let msg = f("同步任务「{}」启动失败：{}", &[&display_name(&task), &e]);
                set_sync_status(ui, msg);
            }
        }
    }
    if !started.is_empty() {
        let stamp = rcgui_profile::now_iso();
        {
            let mut cfg = ctx.cfg.lock().unwrap();
            for (task, _) in &started {
                if let Some(slot) = cfg.sync_tasks.iter_mut().find(|t| t.id == task.id) {
                    slot.last_run = stamp.clone();
                    slot.last_status = String::new();
                }
            }
            let _ = crate::config::save(&cfg);
        }
        let names: Vec<String> = started.iter().map(|(t, _)| display_name(t)).collect();
        set_sync_status(
            ui,
            f(
                "已启动 {} 个同步任务：{}",
                &[&(names.len()), &(names.join("、"))],
            ),
        );
    }
}

// ───────────────────────────── oneclick 迁移 ─────────────────────────────

/// 把旧 oneclick 插件里值得保留的东西搬进新架构，然后让它别再管事。
///
/// 只搬两样：
/// - 用户自己选过的 rclone.exe 路径 —— 那可能是插件专用的内核（teldrive 之类），
///   丢了等于让那些远程直接不可用；
/// - "启动时自动连接"的偏好。
///
/// **不搬**端口/用户名/密码/no_auth：新架构每次启动自己挑空闲端口、自己生成随机凭据，
/// 搬过来的固定端口既可能与别人的实例撞车，也会让旧密码变成明文残留。
///
/// 迁移后把插件置为停用，并清掉它的启停钩子——否则用户一启用插件，就会再起一个 rclone，
/// 和本程序管理的那个抢同一份配置文件。
pub fn migrate_oneclick(ctx: &Arc<Ctx>) -> Vec<String> {
    let mut notes = Vec::new();
    {
        let cfg = ctx.cfg.lock().unwrap();
        if cfg.oneclick_migrated {
            return notes;
        }
    }
    let kv = ctx.store.get_all("onerclone");
    if kv.is_null() {
        // 没装过 oneclick：标记一下，省得每次启动都来查一遍。
        let mut cfg = ctx.cfg.lock().unwrap();
        cfg.oneclick_migrated = true;
        let _ = crate::config::save(&cfg);
        return notes;
    }
    let exe = kv
        .get("exe")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let auto_connect = kv.get("auto_connect").and_then(|v| v.as_bool());
    {
        let mut cfg = ctx.cfg.lock().unwrap();
        if !exe.is_empty() && cfg.core.path.trim().is_empty() && Path::new(&exe).is_file() {
            cfg.core.path = exe.clone();
            cfg.core.mode = "managed".to_string();
            notes.push(f("已沿用 oneclick 里的内核路径：{}", &[&exe]));
        } else if !exe.is_empty() && !Path::new(&exe).is_file() {
            notes.push(f("oneclick 里记的内核路径已不存在，未沿用：{}", &[&exe]));
        }
        if let Some(on) = auto_connect {
            cfg.rclone.connect_on_start = on;
        }
        cfg.plugin_enabled.insert("onerclone".to_string(), false);
        cfg.oneclick_migrated = true;
        match crate::config::save(&cfg) {
            Ok(()) => {}
            Err(e) => notes.push(f("迁移后保存配置失败：{}", &[&e])),
        }
    }
    // 清掉插件自己的启停钩子。直接改它目录下的 config.json——这是插件作者在 MIGRATION.md
    // 里给的迁移路径：钩子留着就会在本程序之外再起一个 rclone。
    let dir = plugins::plugins_dir().join("onerclone");
    if dir.is_dir() {
        match strip_plugin_hooks(&dir) {
            Ok(true) => notes.push(
                t("已停用 oneclick 插件并清掉它的启动/退出钩子（内核改由本程序管理）").to_string(),
            ),
            Ok(false) => {}
            Err(e) => notes.push(f("清理 oneclick 钩子失败：{}", &[&e])),
        }
    }
    for n in &notes {
        crate::logging::event("info", "migrate", n);
    }
    notes
}

/// 把插件 config.json 里的 `on_start` / `on_stop` / `on_exit` 置空。
///
/// 返回 true 表示确实改写了文件。字段不存在也算成功但不改文件——避免无意义地动用户的插件目录。
fn strip_plugin_hooks(dir: &Path) -> Result<bool> {
    let path = dir.join("config.json");
    let raw = std::fs::read(&path).map_err(|e| anyhow!("读 {} 失败：{}", path.display(), e))?;
    let mut value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|e| anyhow!("{} 不是合法 JSON：{}", path.display(), e))?;
    let Some(root) = value.as_object_mut() else {
        return Err(anyhow!("{} 的顶层不是对象", path.display()));
    };
    let mut touched = false;
    // 钩子既可能写在顶层，也可能写在 plugin_setting 里。
    for key in ["on_start", "on_stop", "on_exit"] {
        if root.get(key).map(|v| !v.is_null()).unwrap_or(false) {
            root.insert(key.to_string(), serde_json::Value::Null);
            touched = true;
        }
    }
    if let Some(setting) = root
        .get_mut("plugin_setting")
        .and_then(|v| v.as_object_mut())
    {
        for key in ["on_start", "on_stop", "on_exit"] {
            if setting.get(key).map(|v| !v.is_null()).unwrap_or(false) {
                setting.insert(key.to_string(), serde_json::Value::Null);
                touched = true;
            }
        }
    }
    if !touched {
        return Ok(false);
    }
    let body = serde_json::to_vec_pretty(&value)?;
    crate::config::write_atomic(&path, &body)?;
    Ok(true)
}

// ───────────────────────────── 测试 ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn task(direction: &str, id: &str) -> SyncTask {
        SyncTask {
            id: id.to_string(),
            name: String::new(),
            local: "C:\\data\\pics".into(),
            remote: "gdrive:backup/pics".into(),
            direction: direction.to_string(),
            mode: sync_tasks::MODE_COPY.to_string(),
            enabled: true,
            interval_hours: 6,
            last_run: String::new(),
            last_status: String::new(),
        }
    }

    #[test]
    fn direction_maps_both_ways() {
        assert_eq!(direction_from_ui(0), sync_tasks::DIR_UP);
        assert_eq!(direction_from_ui(1), sync_tasks::DIR_DOWN);
        // 越界值按默认（上行）处理，不能变成第三个方向
        assert_eq!(direction_from_ui(7), sync_tasks::DIR_UP);
        assert_eq!(
            sync_tasks::ui_direction(&task(sync_tasks::DIR_DOWN, "a")),
            "remote-to-local"
        );
    }

    #[test]
    fn unnamed_task_falls_back_to_remote_tail() {
        assert_eq!(display_name(&task(sync_tasks::DIR_UP, "a")), "backup/pics");
        let mut named = task(sync_tasks::DIR_UP, "b");
        named.name = "  照片  ".into();
        assert_eq!(display_name(&named), "  照片  ");
    }

    #[test]
    fn rows_show_running_state_and_next() {
        let engine = sync_tasks::Engine::new();
        let tasks = vec![task(sync_tasks::DIR_UP, "a")];
        let rows = sync_rows(&tasks, Some(&engine));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status.as_str(), "尚未运行");
        assert_eq!(rows[0].direction.as_str(), "local-to-remote");
    }

    #[test]
    fn disabled_task_has_no_next_run() {
        let mut t = task(sync_tasks::DIR_UP, "a");
        t.enabled = false;
        let rows = sync_rows(&[t], None);
        assert_eq!(rows[0].next.as_str(), "—");
        assert_eq!(rows[0].status.as_str(), "已停用");
    }

    #[test]
    fn failed_task_keeps_reason() {
        let mut t = task(sync_tasks::DIR_UP, "a");
        t.last_status = "failed: 连接超时".into();
        let rows = sync_rows(&[t], None);
        assert!(rows[0].status.contains("连接超时"), "{}", rows[0].status);
    }

    /// 生成的任务 id 必须唯一：同一秒内连点两次不能合并成一条。
    #[test]
    fn task_ids_are_unique() {
        let a = new_task_id();
        let b = new_task_id();
        assert_ne!(a, b);
        assert!(a.starts_with("sync-"));
    }

    #[test]
    fn hooks_are_cleared_in_both_places() {
        let dir = std::env::temp_dir().join(format!("rcgui-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({
            "id": "onerclone",
            "on_start": "start_rclone",
            "plugin_setting": { "on_exit": "stop_rclone", "title": "x" }
        });
        std::fs::write(dir.join("config.json"), serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(strip_plugin_hooks(&dir).unwrap());
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
        assert!(after["on_start"].is_null());
        assert!(after["plugin_setting"]["on_exit"].is_null());
        // 其余字段必须原样保留
        assert_eq!(after["plugin_setting"]["title"], "x");
        assert_eq!(after["id"], "onerclone");
        // 第二次跑没有可清的东西，不该再写文件
        assert!(!strip_plugin_hooks(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_config_is_an_error_not_silent_success() {
        let dir = std::env::temp_dir().join(format!("rcgui-nohooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(strip_plugin_hooks(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
