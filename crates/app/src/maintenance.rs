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

/// 托管内核连续起不来时的重试间隔。三次之后停手，等用户点「重试启动内核」。
///
/// 间隔递增：第一次多半是端口刚被别的东西占了一下，等两秒就够；连着三次失败通常是
/// 路径错、被杀软拦下或配置写坏，再每 2 秒拉一次只会把日志刷满，还把真正的原因埋掉。
const CORE_RETRY_GAPS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
];

/// 内核活过这么久，才算"这一轮是好的"，此前的连续失败清零。
///
/// 没有这条，一个每天崩一次的坏配置会永远停在"第 3 次失败，已停止重试"上：
/// 中间那 23 小时都白跑了。
const CORE_HEALTHY_AFTER: Duration = Duration::from_secs(60);

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
    /// 每个同步任务的文件变化监视、待同步状态和重试时刻。只存内存：重启后重新盯一遍，
    /// 靠"最新修改时间是否晚于上次同步"补上停机期间漏掉的变化，而不是重新记一遍基线。
    watchers: crate::sync_watch::Watchers,
    /// 上一次推给界面的同步行指纹，用来避免每秒重设 model 导致列表闪烁/丢选中。
    sync_fingerprint: String,
    /// 内核安装/更新进度，避免同一个安装动作并发重入。
    busy_core: bool,
    busy_store: bool,
    /// 最近一次查到的内核发行版信息（含校验值）。只存内存：查询结果会过期，
    /// 重启后必须重新问一次才能安装。
    core_release: Option<core::ReleaseInfo>,
    /// 托管内核的值守状态。只存内存：程序重启就等于重新开始一轮。
    core_watch: CoreWatch,
}

/// 托管内核的自动重启记账。
///
/// 内核现在跟程序同生共死（界面上的手动启停已经拿掉），起不来就没人管了，
/// 所以这里要记三件事：失败了几次、下次什么时候再试、这一次起来了多久。
#[derive(Default)]
struct CoreWatch {
    /// 连续失败次数。内核活过 `CORE_HEALTHY_AFTER` 之后清零。
    failures: u32,
    /// 下次允许尝试启动的时间。`None` = 现在就可以试。
    next_try: Option<Instant>,
    /// 当前这一轮内核是什么时候被看见"在跑"的，用来判断跑够了没有。
    started_at: Option<Instant>,
    /// 已经放弃自动重试，等用户点「重试启动内核」。
    gave_up: bool,
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

/// 是否显示「重试启动内核」。
///
/// 这不是把手动启停放回来：运维权仍在自动值守手里，这个按钮只在值守放弃之后出现，
/// 用户可以换路径、插上 U 盘、关掉杀软之后再叫它试一次。
fn set_core_retry_needed(ui: &Weak<MainWindow>, needed: bool) {
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>().set_core_needs_retry(needed);
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

pub(crate) fn set_sync_status(ui: &Weak<MainWindow>, text: impl Into<String>) {
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
    g.set_core_path(cfg.core.path.clone().into());
    g.set_core_flavor(cfg.core.normalized_flavor().as_str().into());
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

/// 进「文件夹同步」页时把已保存的任务灌进界面。
///
/// 只重画任务列表，不动表单里那几个输入框：用户可能正填到一半切走又切回来，
/// 进去一次就把草稿清掉是最容易被骂的那种"贴心"。
pub fn load_sync_into_ui(ctx: &Arc<Ctx>, ui: &MainWindow) {
    let cfg = ctx.cfg.lock().unwrap().clone();
    let rows = sync_rows(&cfg.sync_tasks, Some(&ctx.maint.engine));
    let g = ui.global::<MaintenanceG>();
    g.set_sync_items(ModelRc::new(VecModel::from(rows)));
}

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
                // 这一行现在还会不会因为本机变化自己跑起来。别只显示"启用"：
                // 用户想知道的是"我改完文件它会不会自己动"，那是两件事。
                auto: crate::sync_watch::watches(task),
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
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\n",
            r.id, r.name, r.local, r.remote, r.direction, r.enabled, r.auto, r.status, r.next
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
    // 权限那一列是 `[string]`，对应的 Slint 模型内部是 `Rc`，不能跨线程。
    // 所以先在后台把纯数据（String / Vec<String>，都是 Send）准备好，
    // 模型留到 UI 线程的闭包里再建——顺序反过来就编译不过。
    with_ui(ui, move |u| {
        let rows: Vec<StoreRow> = store::rows(&entries, &installed)
            .into_iter()
            .map(|r| StoreRow {
                id: r.id.into(),
                name: r.name.into(),
                version: r.version.into(),
                description: r.description.into(),
                installed: r.installed,
                update: r.update,
                permissions: ModelRc::new(VecModel::from(
                    r.permissions
                        .into_iter()
                        .map(slint::SharedString::from)
                        .collect::<Vec<_>>(),
                )),
            })
            .collect();
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

/// 用户装过托管内核、这次却没用到数据目录那份时，把原因写进日志。
///
/// 界面上只会显示"找到的内核来自程序目录 core/"，看的人无法分辨是没装过、还是装过
/// 却用不上；两种情况都该留一条可查的记录：文件没了（被删、被安全软件清掉），或者
/// 文件在但跑不起来（下载被截断、被杀软换成空壳）。
fn note_managed_skipped(core_cfg: &crate::config::CoreCfg, located: &core::Located) {
    if !core_cfg.has_managed_install()
        || core_cfg.normalized_mode() != "managed"
        || !core_cfg.path.trim().is_empty()
        || matches!(located.source, core::Source::DataDir)
    {
        return;
    }
    let data_exe = core::data_core_exe(&crate::config::data_dir());
    let why = if data_exe.is_file() {
        "文件还在但跑不起来（下载不完整或被安全软件改坏）"
    } else {
        "文件不存在（被删除或被安全软件清掉）"
    };
    crate::logging::event(
        "warn",
        "core",
        &format!(
            "已装过的托管内核用不上，回落到{}：{}；{why}",
            located.source.label(),
            data_exe.display()
        ),
    );
}

/// 内核状态串：来源 + 版本 + 在跑还是停了。界面就靠这一行判断"到底有没有内核"。
pub fn core_status_text(ctx: &Arc<Ctx>) -> String {
    let phase = ctx.maint.core.phase();
    if let core::Phase::Running { .. } = phase {
        return phase.label();
    }
    let cfg = ctx.cfg.lock().unwrap().clone();
    let data = crate::config::data_dir();
    match core::resolve(
        &cfg.core.path,
        &exe_dir(),
        &data,
        cfg.core.has_managed_install(),
    ) {
        Ok(Some(located)) => {
            let version =
                core::probe_version(&located.exe).unwrap_or_else(|_| t("版本未知").to_string());
            let found = f(
                "已找到内核（{}）：{} · {}",
                &[&located.source.label(), &located.exe.display(), &version],
            );
            // 找得到可执行文件不代表它跑起来了。之前这里直接返回"已找到内核"，
            // 于是崩掉的内核在界面上看起来跟停着的一模一样——原因全丢了。
            match phase {
                core::Phase::Failed(_) | core::Phase::Exited(_) => {
                    format!("{} — {}", phase.label(), found)
                }
                _ => found,
            }
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

/// 按配置启动托管内核。
///
/// 返回的 `Some(说明)` **不代表进程起来了**：找不到内核、启动失败也都会返回一句
/// 给用户看的原因。要判断成败只看 `ctx.maint.core.is_running()`（或 `restart_outcome`）。
///
/// 只在 `core.mode == "managed"` 且能找到内核时动手，失败不拦启动——
/// 启动失败不该把整个程序卡死。
pub async fn start_managed_core(ctx: &Arc<Ctx>) -> Option<String> {
    let cfg = ctx.cfg.lock().unwrap().clone();
    if cfg.core.normalized_mode() != "managed" {
        return None;
    }
    let data = crate::config::data_dir();
    let located = match core::resolve(
        &cfg.core.path,
        &exe_dir(),
        &data,
        cfg.core.has_managed_install(),
    ) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return Some(
                t("没找到 rclone 内核，已跳过自动启动（可在维护页指定路径或下载）").to_string(),
            )
        }
        Err(e) => return Some(e.to_string()),
    };
    note_managed_skipped(&cfg.core, &located);
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
                // 把用的是哪个文件、从哪一档找来的记下来：同版本换渠道时两个内核的
                // 版本号只差后缀，日志里没有这两项就只能靠文件大小猜（真发生过）。
                crate::logging::event(
                    "info",
                    "core",
                    &format!(
                        "托管内核已启动（pid={}，代次={gen}，{}：{}）",
                        ctx.maint.core.pid().unwrap_or(0),
                        located.source.label(),
                        located.exe.display()
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
        "core-check" => core_check(ctx, &ui).await,
        "core-flavor" => core_flavor(ctx, &ui, id).await,
        "core-adopt-managed" => core_adopt_managed(ctx, &ui).await,
        "core-install" => core_install(ctx, &ui).await,
        "core-start" => core_start(ctx, &ui).await,
        "core-stop" => core_stop(ctx, &ui).await,
        "core-retry" => core_retry(ctx, &ui).await,
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
        "sync-pick-local" => sync_pick_local(&ui).await,
        "sync-pick-remote" => sync_pick_remote(ctx, &ui).await,
        "sync-run" => sync_run(ctx, &ui, id).await,
        "sync-toggle" => sync_toggle(ctx, &ui, id).await,
        "sync-auto" => sync_auto(ctx, &ui, id).await,
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
    let text = match core::resolve(
        &cfg.core.path,
        &exe_dir(),
        &data,
        cfg.core.has_managed_install(),
    ) {
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

/// 查一下当前渠道有没有新版本，把结论写进状态行。
///
/// 只查询、不下载：一次下载是 30MB 流量加几秒钟，用户在设置页点"检查更新"时
/// 还不想让它悄悄落盘。查到的东西留在内存里，重启后必须重新查。
async fn core_check(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    {
        let mut st = ctx.maint.state();
        if st.busy_core {
            set_core_status(ui, t("内核操作已在进行中，请稍等"));
            return;
        }
        st.busy_core = true;
    }
    set_busy(ui, true);
    let (flavor, installed) = {
        let cfg = ctx.cfg.lock().unwrap();
        (cfg.core.normalized_flavor(), cfg.core.last_version.clone())
    };
    set_core_status(ui, f("正在查询{}的最新版本…", &[&flavor.label()]));
    let result = core::release_info(flavor).await;
    {
        let mut st = ctx.maint.state();
        st.busy_core = false;
        if let Ok(info) = &result {
            st.core_release = Some(info.clone());
        }
    }
    set_busy(ui, false);
    match result {
        Ok(info) => {
            let size = if info.size_text().is_empty() {
                t("大小未知").to_string()
            } else {
                info.size_text()
            };
            let local = if installed.trim().is_empty() {
                t("尚未安装").to_string()
            } else {
                installed.clone()
            };
            let verdict = if installed.trim().is_empty() {
                t("可以安装").to_string()
            } else if core::newer_than(&info.version, &installed) {
                t("有新版本").to_string()
            } else {
                t("已是最新").to_string()
            };
            crate::logging::event(
                "info",
                "core",
                &format!(
                    "{} 最新 {}（{}，{}），本机 {}，{}",
                    flavor.as_str(),
                    info.tag,
                    info.asset,
                    info.published_at,
                    local,
                    verdict
                ),
            );
            // 状态行只放用户能一眼用上的：版本、大小、本机、结论。发行说明第一行
            // 跟在后面，写不下时界面自己省略号收尾。
            let mut status = f(
                "{} 最新 {}（{}）：本机 {}，{}",
                &[&flavor.label(), &info.tag, &size, &local, &verdict],
            );
            if !info.notes.trim().is_empty() {
                status.push_str(" · ");
                status.push_str(info.notes.trim());
            }
            set_core_status(ui, status);
        }
        Err(e) => {
            crate::logging::event("error", "core", &format!("查询内核版本失败：{e}"));
            set_core_status(ui, f("查询内核版本失败：{}", &[&e]));
            crate::toast(ui, 2, t("查不到最新内核，请检查网络或稍后再试"));
            refresh_core_status(ctx, ui);
        }
    }
}

/// 切换下载渠道（原版 / 增强版）。只决定"以后下载哪个"，不动已经装好的内核。
async fn core_flavor(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, id: &str) {
    let want = core::Flavor::parse(id);
    let (mode, custom, explicit, running, recorded, last_version) = {
        let cfg = ctx.cfg.lock().unwrap();
        (
            cfg.core.normalized_mode(),
            cfg.core.path.trim().to_string(),
            cfg.core.flavor.trim().to_string(),
            ctx.maint.core.is_running(),
            cfg.core.recorded_flavor(),
            cfg.core.last_version.clone(),
        )
    };
    // 本机那份是哪个渠道：配置里记过就信记录，没记过就按版本串推（老配置）。
    let installed_flavor = recorded.or_else(|| {
        if last_version.trim().is_empty() {
            None
        } else {
            Some(core::Flavor::of_version(&last_version))
        }
    });
    // 同版本换渠道也算换：`1.76.0` 与 `1.76.0-extra` 数字段一样、二进制不一样。
    let switching_binary = installed_flavor.is_some_and(|f| f != want);
    if want.as_str() == explicit {
        // 点自己已经选中的那个：不写盘、不警告，只把状态行说清楚。
        set_core_status(ui, f("已经使用{}", &[&want.label()]));
        return;
    }
    {
        let mut cfg = ctx.cfg.lock().unwrap();
        cfg.core.flavor = want.as_str().to_string();
        if let Err(e) = crate::config::save(&cfg) {
            crate::logging::event("error", "core", &format!("保存内核渠道失败：{e}"));
            set_core_status(ui, f("保存内核渠道失败：{}", &[&e]));
            return;
        }
    }
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>()
            .set_core_flavor(want.as_str().into());
    });
    crate::logging::event("info", "core", &format!("内核渠道切换为 {}", want.as_str()));
    // 同版本换渠道：数字一样不代表不用换，重下之前先说清本机装的是哪个渠道。
    // 这段要跟着界面语言走，所以也走 `f()`；没换渠道时是空串，模板末尾的 `{}` 就空着。
    let same_version_note = if switching_binary {
        f(
            "（本机装的是{}，版本号相同也要重下：两份 exe 不同）",
            &[&installed_flavor.map(|f| f.label()).unwrap_or("")],
        )
    } else {
        String::new()
    };
    if mode != "managed" {
        // 外部模式不存在"我们下载的内核"，记下来等切回托管用。
        set_core_status(
            ui,
            f(
                "已记住{}：外部模式下不会下载内核{}",
                &[&want.label(), &same_version_note],
            ),
        );
    } else if !custom.is_empty() {
        // 「程序路径」优先是真的，但别让人以为只能手动清空——旁边就有按钮。
        set_core_status(
            ui,
            f(
                "已记住{}；「程序路径」里指定的内核仍优先，要换成下载的版本请点「改用托管内核」（原文件保留，不覆盖）{}",
                &[&want.label(), &same_version_note],
            ),
        );
    } else if running {
        set_core_status(
            ui,
            f(
                "已切换到{}：需要重启内核才生效，正在进行的传输与挂载会中断{}",
                &[&want.label(), &same_version_note],
            ),
        );
        crate::toast(
            ui,
            2,
            t("切换渠道后要重启内核；现在这份仍在运行，点「安装 / 更新内核」会一并替换"),
        );
    } else {
        set_core_status(
            ui,
            f(
                "已切换到{}：点「检查更新」看有没有新版本，再点「安装 / 更新内核」下载{}",
                &[&want.label(), &same_version_note],
            ),
        );
    }
}

/// 改用托管内核：把手填的路径收进备选，之后由我们自己下载与替换。
///
/// 只改配置、不碰文件——用户填的那份 exe 原地留着，我们只是不再用它。下载与替换留在
/// `core-install`，于是「不再用你挑的那份」和「真把它换掉」是两次点击，中间那步用户
/// 能看清后果（换内核要重启内核，挂载与传输会断）。
async fn core_adopt_managed(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    if ctx.maint.state().busy_core {
        set_core_status(ui, t("内核操作已在进行中，请稍等"));
        return;
    }
    let (kept, running) = {
        let mut cfg = ctx.cfg.lock().unwrap();
        let was_external = cfg.core.normalized_mode() != "managed";
        let kept = cfg.core.adopt_managed();
        let running = ctx.maint.core.is_running();
        if kept.is_some() || was_external {
            if let Err(e) = crate::config::save(&cfg) {
                crate::logging::event("error", "core", &format!("保存内核路径失败：{e}"));
                set_core_status(ui, f("保存内核路径失败：{}", &[&e]));
                return;
            }
        }
        (kept, running)
    };
    with_ui(ui, |u| {
        let g = u.global::<MaintenanceG>();
        g.set_core_path("".into());
    });
    match kept {
        Some(p) => {
            crate::logging::event(
                "info",
                "core",
                &format!("改用托管内核；原路径保留在备选里：{p}"),
            );
            set_core_status(
                ui,
                f(
                    "已改用托管内核：原内核 {} 保留在原处（记进备选，不删文件）；点「安装 / 更新内核」下载所选渠道的版本",
                    &[&p],
                ),
            );
            if running {
                crate::toast(
                    ui,
                    2,
                    t("正在运行的内核是从那个路径启动的；点「安装 / 更新内核」会先停掉它（传输与挂载会中断）"),
                );
            } else {
                crate::toast(ui, 1, t("已改用托管内核；原文件保留，随时可以填回去"));
            }
        }
        None => {
            set_core_status(ui, t("没有手填的内核路径，已经在用托管内核"));
        }
    }
    refresh_core_status(ctx, ui);
}

/// 重启内核之后的三种真实结局。
///
/// 判定只看两件事实：进程还在不在、握手成不成。**不看** `start_managed_core`
/// 返回的那句话——它把失败原因也包在 `Some` 里，照着 `Some` 报「已重启」就会把
/// 启动失败说成成功，用户看到「已更新并重启」却连不上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartOutcome<'a> {
    /// 进程起来了，而且连上了。
    Connected,
    /// 进程起来了，但没能连上：端口还没就绪、鉴权被拒、对端能力不足都可能。
    RunningOffline,
    /// 进程根本没起来，附带启动那一步给的说明。
    NotRunning(&'a str),
}

fn restart_outcome<'a>(started: Option<&'a str>, running: bool, live: bool) -> RestartOutcome<'a> {
    if !running {
        // 起不来时 `started` 里是我们的原因文案；真没有就给一句通用的，
        // 不留空字符串让用户面对一个没有下半句的状态栏。
        return RestartOutcome::NotRunning(started.unwrap_or(t("没有找到可以启动的内核")));
    }
    if live {
        RestartOutcome::Connected
    } else {
        RestartOutcome::RunningOffline
    }
}

/// 下载并替换「我们自己的」内核，渠道取设置里选的那一个。
///
/// 三条不做的事：外部模式不下载（那台 rclone 不是我们的）、用户手填的内核路径不覆盖
/// （那是他特意挑的 fork）、同版本且能跑的 exe 不重下（30MB 换不来任何东西）。
async fn core_install(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    {
        let mut st = ctx.maint.state();
        if st.busy_core {
            set_core_status(ui, t("内核操作已在进行中，请稍等"));
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
    let (mode, custom, flavor) = {
        let cfg = ctx.cfg.lock().unwrap();
        (
            cfg.core.normalized_mode(),
            cfg.core.path.trim().to_string(),
            cfg.core.normalized_flavor(),
        )
    };
    // 外部模式只连别人跑着的 rclone：替换本地 exe 对那个进程毫无影响，只会让人
    // 以为自己连的那份被换掉了。
    if mode != "managed" {
        ctx.maint.state().busy_core = false;
        set_busy(ui, false);
        set_core_status(ui, t("当前是外部模式，不会下载或替换内核"));
        crate::toast(
            ui,
            2,
            t("外部模式下不提供内核下载；需要的话先切到「托管内核」"),
        );
        return;
    }
    // 用户亲手指定的内核永远优先：那通常是他为了某个后端（例如 teldrive）特意挑的
    // fork，用下载来的版本盖掉等于把选择丢了。要换只有一条明路：「改用托管内核」那一下
    // 先把路径收进备选（文件不动），下一次点这里才轮到我们下载替换。
    if !custom.is_empty() {
        ctx.maint.state().busy_core = false;
        set_busy(ui, false);
        set_core_status(
            ui,
            f(
                "已手动指定内核路径（{}），不会用下载的版本覆盖它",
                &[&custom],
            ),
        );
        crate::toast(
            ui,
            2,
            t("要改用下载的内核，请先点「改用托管内核」（原文件会保留，不会被覆盖）"),
        );
        return;
    }
    set_core_status(ui, f("正在查询{}的最新版本…", &[&flavor.label()]));
    let info = match core::release_info(flavor).await {
        Ok(i) => i,
        Err(e) => {
            ctx.maint.state().busy_core = false;
            set_busy(ui, false);
            crate::logging::event("error", "core", &format!("查询内核版本失败：{e}"));
            set_core_status(ui, f("查询内核版本失败：{}", &[&e]));
            crate::toast(ui, 2, t("查不到最新内核，请检查网络或稍后再试"));
            refresh_core_status(ctx, ui);
            return;
        }
    };
    ctx.maint.state().core_release = Some(info.clone());
    // 同版本就不重下；但"能跑"必须实测，否则一个坏掉的 exe 永远修不好。版本相同还得
    // 看渠道：`1.76.0` 与 `1.76.0-extra` 数字段一样、二进制不一样，换渠道必须重下。
    let installed_exe = core::data_core_exe(&data);
    let probed = core::probe_version(&installed_exe).ok();
    let recorded = ctx.cfg.lock().unwrap().core.recorded_flavor();
    if !core::needs_install(flavor, &info.version, probed.as_deref(), recorded) {
        ctx.maint.state().busy_core = false;
        set_busy(ui, false);
        set_core_status(
            ui,
            f(
                "{} 已经是最新（{}），不需要重新下载",
                &[&flavor.label(), &info.tag],
            ),
        );
        crate::toast(ui, 1, t("已是最新版本，不需要重新下载"));
        refresh_core_status(ctx, ui);
        return;
    }
    // 替换内核要把旧 exe 重命名成 .bak，Windows 会拒绝正在运行的那个文件。所以先停掉
    // **我们自己启动的**内核；外部接入的实例一律不碰（那个 exe 本来也不是我们要替换的
    // 文件，而且规则是永不结束不是自己启动的进程）。
    let was_running = ctx.maint.core.is_running();
    if was_running {
        ctx.maint.core.stop();
        // 端口随刚才那个进程一起消失。留着它当"上次快照"，用户会一直看到
        // "已断开（显示的是上次快照）"和一个再也不会应答的端口。
        forget_core_endpoint(ctx, ui, Some(t("正在更换内核，下载完成后会自动重新连接"))).await;
    }
    let size = if info.size_text().is_empty() {
        t("大小未知").to_string()
    } else {
        info.size_text()
    };
    set_core_status(
        ui,
        if was_running {
            // 停内核会连带停掉挂载和正在传输的任务，这句必须说出来。
            f(
                "已停止自家内核（正在进行的传输与挂载会中断），正在下载 {} {}…",
                &[&flavor.label(), &info.tag],
            )
        } else {
            f(
                "正在下载 {} {}（{}）…",
                &[&flavor.label(), &info.tag, &size],
            )
        },
    );
    let result = core::install_release(&target, &info).await;
    ctx.maint.state().busy_core = false;
    set_busy(ui, false);
    match result {
        Ok(exe) => {
            let v = core::probe_version(&exe).unwrap_or_else(|_| info.version.clone());
            {
                let mut cfg = ctx.cfg.lock().unwrap();
                // 装了就用这个：把模式切回 managed，否则用户装了内核却还是连外部实例。
                cfg.core.mode = "managed".to_string();
                cfg.core.last_version = v.clone();
                // 渠道写成显式值：以后即使 last_version 被清掉也不会退回推断。
                cfg.core.flavor = flavor.as_str().to_string();
                let _ = crate::config::save(&cfg);
            }
            let channel = flavor.as_str();
            with_ui(ui, move |u| {
                u.global::<MaintenanceG>().set_core_flavor(channel.into());
            });
            crate::logging::event(
                "info",
                "core",
                &format!("{} 内核已安装：{}（{}）", flavor.as_str(), exe.display(), v),
            );
            set_core_status(ui, f("内核已安装：{}（{}）", &[&exe.display(), &v]));
            if was_running {
                // 装了内核的人下一步一定是启动，替他重启并把结果说清楚，
                // 免得"装完内核却停着"看起来像失败。
                let started = start_managed_core(ctx).await;
                // `start_managed_core` 的 `Some` 里也可能是失败原因（"内核启动失败：…"），
                // 所以"到底起来没有"只能看进程本身。
                let running = ctx.maint.core.is_running();
                let live = if running {
                    // 新进程是新端口 + 新随机凭据：必须用它的地址重新握手，
                    // 否则界面停在上一个内核的端口上，看起来永远连不上。
                    reconnect_after_core_change(ctx, ui).await
                } else {
                    false
                };
                let endpoint = ctx
                    .maint
                    .core
                    .endpoint()
                    .map(|(url, _, _)| url)
                    .unwrap_or_default();
                match restart_outcome(started.as_deref(), running, live) {
                    RestartOutcome::Connected => {
                        set_core_status(
                            ui,
                            f(
                                "内核已安装（{}）并在 {} 上重新启动，已重新连接",
                                &[&v, &endpoint],
                            ),
                        );
                        crate::toast(ui, 1, t("内核已更新并重新连接"));
                    }
                    RestartOutcome::RunningOffline => {
                        set_core_status(
                            ui,
                            f(
                                "内核已安装（{}）并在 {} 上重新启动，但没能连上，请看下面的状态",
                                &[&v, &endpoint],
                            ),
                        );
                        crate::toast(
                            ui,
                            2,
                            t("新内核已启动，但没能连上；程序会自动重试，状态里会写明原因"),
                        );
                    }
                    RestartOutcome::NotRunning(why) => {
                        set_core_status(ui, f("内核已安装（{}），但没能重新启动：{}", &[&v, &why]));
                        crate::toast(ui, 2, t("内核已更新，但没能重新启动；程序会自动重试"));
                    }
                }
            } else {
                crate::toast(ui, 1, t("内核已安装，程序会自动启动它"));
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

/// 我们自己停掉了内核：那个随机端口跟着进程一起没了，别再留着它当"上次快照"。
///
/// 只在自己拥有的子进程被停掉之后调用。外部实例是别人的进程，它的连接状态
/// 以及快照都不该因为这里的一个动作被抹掉。
///
/// `hint` 会被当成这条连接状态的失败原因：忘掉端点后状态是「未连接」、错误为空，
/// 光看仪表盘分不清是「正在换内核」还是「又出故障了」，所以由调用方给一句话。
async fn forget_core_endpoint(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, hint: Option<&str>) {
    ctx.rc.forget().await;
    let mut view = crate::snapshot_view(ctx).await;
    if let Some(hint) = hint {
        view.error = hint.to_string();
    }
    crate::apply_conn_view(ui, view);
}

/// 换了一次内核（新进程、新端口、新随机凭据）之后，用**它自己的**地址重新握手。
///
/// 新旧端口没有任何继承关系，所以这里绝不沿用旧端点，只认 `ctx.maint.core.endpoint()`。
/// 内核启动只等到 TCP 可连，鉴权请求可能还差一小会儿，因此允许短暂重试；
/// 返回是否真的连上，调用方据此决定说"已重新连接"还是把失败原因留着。
async fn reconnect_after_core_change(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) -> bool {
    const ATTEMPTS: u32 = 5;
    const GAP: Duration = Duration::from_millis(400);
    let mut view = None;
    for attempt in 0..ATTEMPTS {
        if !ctx.maint.core.is_running() {
            // 进程自己退了（端口被占、启动即崩）：别再去敲一个没有人听的端口。
            let mut v = crate::snapshot_view(ctx).await;
            v.error = t("新内核进程已退出，没有可连的端点").to_string();
            view = Some(v);
            break;
        }
        let out = crate::connect_report(ctx).await;
        let live = out.view.live;
        view = Some(out.view);
        if live {
            break;
        }
        if attempt + 1 < ATTEMPTS {
            tokio::time::sleep(GAP).await;
        }
    }
    let view = match view {
        Some(v) => v,
        None => crate::snapshot_view(ctx).await,
    };
    let live = view.live;
    // 只记地址，不带凭据：内核的密码只属于那个进程。
    let endpoint = view.endpoint.clone();
    crate::apply_conn_view(ui, view);
    if live {
        // 仪表盘已经指向新端口，但远程列表、挂载、任务还是旧内核那一份。
        // 这里主动拉一次（force）：等下一跳轮询不可靠——轮询有「内容没变就不重绘」
        // 的闸门，取数失败时还会沿用旧快照，界面就停在上一代内核的数据上。
        let ctx2 = ctx.clone();
        let ui2 = ui.clone();
        tokio::spawn(async move {
            crate::refresh::refresh_state(&ctx2, &ui2, true).await;
        });
        crate::logging::event("info", "core", &format!("内核换代后已重新连接：{endpoint}"));
    } else {
        crate::logging::event("warn", "core", &format!("内核换代后没能连上：{endpoint}"));
    }
    live
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
        // 新内核用的是新端口和新随机凭据，所以走同一个换代重连入口。
        if reconnect_after_core_change(ctx, ui).await {
            crate::toast(ui, 1, t("内核已启动并连接"));
        }
    }
    refresh_core_status(ctx, ui);
}

async fn core_stop(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let was = ctx.maint.core.is_running();
    if was {
        ctx.maint.core.stop();
        // 停掉自己起的进程后那个端口就没了，把端点一起忘掉；否则界面会一直显示
        // "已断开（显示的是上次快照）"配一个再也不会应答的端口。
        // 外部实例不在这里：它没被我们停，连接状态一个字都不动。
        // 用户自己点的停止，不需要额外解释，因此不给提示语。
        forget_core_endpoint(ctx, ui, None).await;
    }
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
        u.global::<MaintenanceG>().get_core_path().to_string()
    })
    .await;
    let Some(path) = picked else {
        set_core_status(ui, t("读取界面设置失败"));
        return;
    };
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
        // 严格按托管保存：界面上已经没有「外部 rclone」这个选项，任何残留的
        // 旧值都在这里被纠正，不需要用户自己去找开关。
        cfg.core.mode = "managed".to_string();
        cfg.core.path = path.clone();
    }
    let save_result = {
        let cfg = ctx.cfg.lock().unwrap().clone();
        crate::config::save(&cfg)
    };
    match save_result {
        Ok(()) => set_core_status(ui, t("已保存：内核由本程序管理")),
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

/// 挑本机的源目录，选中才写进「本地文件夹」。
///
/// 系统选择框会一直阻塞到用户点确定或取消，这段时间本任务收不了别的 Cmd——
/// 这跟本机浏览页的「选择目录」是同一个做法，对话框挡住窗口时本来也点不到别的。
/// 用户取消（返回 None）就什么都不写，保留原来填的值。
async fn sync_pick_local(ui: &Weak<MainWindow>) {
    let cur = crate::read_ui(ui, |u: MainWindow| {
        u.global::<MaintenanceG>().get_sync_local().to_string()
    })
    .await
    .unwrap_or_default();
    let mut dlg = rfd::FileDialog::new().set_title(t("选择本地文件夹"));
    let start = cur.trim();
    if !start.is_empty() && Path::new(start).is_dir() {
        dlg = dlg.set_directory(start);
    }
    let Some(dir) = dlg.pick_folder() else {
        return;
    };
    let text = dir.display().to_string();
    let shown = text.clone();
    with_ui(ui, move |u| {
        u.global::<MaintenanceG>().set_sync_local(shown.into());
    });
    set_sync_status(ui, f("已选择本地文件夹：{}", &[&text]));
}

/// 打开远程目录浏览窗，选出来的结果写回「云端目录」。
///
/// 列目录要真的走一遍网络，所以不在这里做：把带 scope 的 `Cmd::BrowseOpen` 交给
/// 浏览窗那条路，它负责列、负责把失败原因写回来（`browse_ok_sync` 写字段）。
async fn sync_pick_remote(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    if !ctx.rc.is_live() {
        set_sync_status(ui, t("还没连上 rclone，等连上再选云端目录"));
        return;
    }
    set_sync_status(ui, t("正在读取远程目录…"));
    send(Cmd::BrowseOpen { scope: 2 });
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
        // 没选过就是 None：盯不盯由模式决定（只复制盯，镜像不盯），见 sync_watch::watches。
        auto_sync: None,
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
            let auto_sync = slot.auto_sync;
            *slot = task.clone();
            slot.last_run = last_run;
            slot.last_status = last_status;
            // 「自动」是这一行上的开关，改名字改间隔不该把它重置回默认。
            slot.auto_sync = auto_sync;
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

/// 开关「本机有变化就提前跑一轮」。
///
/// 下行任务（云端 → 本机）上这个开关存着但不生效：那方向里本机文件的变化正是用户自己的
/// 编辑，拿它去触发同步会把编辑覆盖掉。所以这里如实说"不会触发"，而不是显示一个亮着的
/// 开关让用户以为它在管用。
async fn sync_auto(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, id: &str) {
    let (now_auto, up, mirror) = {
        let mut cfg = ctx.cfg.lock().unwrap();
        let Some(slot) = cfg.sync_tasks.iter_mut().find(|t| t.id == id) else {
            set_sync_status(ui, f("找不到任务：{}", &[&id]));
            return;
        };
        // 从现在的实际状态取反，而不是翻转字段：镜像任务没选过时是「不开」，第一次点要变成开。
        let want = !crate::sync_watch::watches(slot);
        slot.auto_sync = Some(want);
        let up = sync_tasks::is_up(slot);
        let mirror = sync_tasks::is_mirror(slot);
        let _ = crate::config::save(&cfg);
        (want, up, mirror)
    };
    // 开关变了也要让指纹变：否则列表上的「自动」标记要等下一次状态变化才刷新。
    ctx.maint.state().sync_fingerprint.clear();
    push_sync_rows(ctx, ui);
    let text = if !now_auto {
        t("已关闭自动同步：只按间隔运行").to_string()
    } else if !up {
        t("已打开自动同步，但这是「云端 → 本机」的任务：本机文件的变化不会触发同步").to_string()
    } else if mirror {
        t("已打开自动同步：本机文件夹一变就提前跑一轮。这是镜像任务，本机的删除也会跟着同步到云端").to_string()
    } else {
        t("已打开自动同步：本机文件夹有变化就提前跑一轮").to_string()
    };
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

// ───────────────────────────── 内核值守 ─────────────────────────────

/// 托管内核的值守：崩了拉起来，拉不动就说清原因、把决定权交回用户。
///
/// 每 2 秒一次（`tick` 里调）。三条边界：
/// - 只碰自己起的进程。外部实例根本不进 `ctx.maint.core`，这里无从误伤；
/// - 安装/更新进行中（`busy_core`）一律让路：那一步自己会重启内核，
///   值守这时候插一手只会把它刚起的进程杀掉；
/// - 连续三次起不来就停手，状态行留原因，界面上出现「重试启动内核」。
async fn supervise_core(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    if ctx.cfg.lock().unwrap().core.normalized_mode() != "managed" {
        return;
    }

    // 先收尸。哪怕这一轮马上要重启，用户也该先看到"它死了、退出码是多少"。
    if let Some(reason) = ctx.maint.core.reap_dead() {
        crate::logging::event("warn", "core", &reason);
        // 只标记失效、保留上次快照：进程没了不等于数据没了。
        ctx.rc.set_disconnected(Some(reason.clone())).await;
        let ran_long = {
            let mut st = ctx.maint.state();
            let long = st
                .core_watch
                .started_at
                .map(|t| t.elapsed() >= CORE_HEALTHY_AFTER)
                .unwrap_or(false);
            st.core_watch.started_at = None;
            if long {
                // 跑够久才崩的不算"起不来"，从零开始记。
                st.core_watch.failures = 0;
            }
            if !st.core_watch.gave_up {
                st.core_watch.next_try = Some(Instant::now() + CORE_RETRY_GAPS[0]);
            }
            long
        };
        if ran_long {
            crate::logging::event("info", "core", "上一轮内核活得够久，失败计数已清零");
        }
        crate::toast(ui, 2, reason);
    }

    if ctx.maint.core.is_running() {
        // 在跑：记下这一轮的起点；跑够时长就把失败计数清零。
        let mut st = ctx.maint.state();
        match st.core_watch.started_at {
            None => st.core_watch.started_at = Some(Instant::now()),
            Some(started) if started.elapsed() >= CORE_HEALTHY_AFTER => {
                st.core_watch.failures = 0;
            }
            _ => {}
        }
        return;
    }

    let due = {
        let st = ctx.maint.state();
        if st.core_watch.gave_up || st.busy_core {
            return;
        }
        let now = Instant::now();
        st.core_watch.next_try.map(|t| now >= t).unwrap_or(true)
    };
    if !due {
        return;
    }
    attempt_core_start(ctx, ui).await;
}

/// 试一次启动，并按"进程在不在 + 握手成不成"记账。
///
/// `start_managed_core` 返回的那句话**不参与判定**：它把"没找到内核""启动失败"
/// 也包在 `Some` 里，照着 `Some` 报成功会把失败说成成功。
async fn attempt_core_start(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let started = start_managed_core(ctx).await;
    let running = ctx.maint.core.is_running();
    let live = if running {
        crate::handshake_live(ctx).await
    } else {
        false
    };
    match restart_outcome(started.as_deref(), running, live) {
        RestartOutcome::Connected => {
            {
                let mut st = ctx.maint.state();
                st.core_watch = CoreWatch {
                    started_at: Some(Instant::now()),
                    ..CoreWatch::default()
                };
            }
            set_core_retry_needed(ui, false);
            refresh_core_status(ctx, ui);
            crate::toast(ui, 1, t("内核已启动并连上。"));
            // 内核换代了：上次记着的挂载/服务要重新下给新的那个进程。
            crate::spawn_restore(ctx, ui);
        }
        RestartOutcome::RunningOffline => {
            // 进程活着但连不上（端口没就绪、鉴权被拒、对端能力不足）。
            // 不杀它，交给连接重试；这一轮仍算失败，否则永远卡在"未连接"也不报错。
            crate::spawn_connect_retry(ctx, ui);
            let why = t("内核进程在跑，但握手没成功").to_string();
            note_core_failure(ctx, ui, &why);
        }
        RestartOutcome::NotRunning(why) => {
            let why = why.to_string();
            note_core_failure(ctx, ui, &why);
        }
    }
}

/// 记一次失败：递增计数、排下一次重试，第三次之后停手并交回用户。
fn note_core_failure(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, why: &str) {
    crate::logging::event("warn", "core", why);
    let gave_up = {
        let mut st = ctx.maint.state();
        st.core_watch.failures = st.core_watch.failures.saturating_add(1);
        let n = st.core_watch.failures as usize;
        if n >= CORE_RETRY_GAPS.len() {
            st.core_watch.gave_up = true;
            st.core_watch.next_try = None;
            true
        } else {
            st.core_watch.next_try = Some(Instant::now() + CORE_RETRY_GAPS[n - 1]);
            false
        }
    };
    refresh_core_status(ctx, ui);
    if gave_up {
        set_core_retry_needed(ui, true);
        crate::toast(
            ui,
            2,
            f(
                "内核连续 {} 次没能启动，已停止自动重试：{}",
                &[&CORE_RETRY_GAPS.len(), &why],
            ),
        );
    }
}

/// 「重试启动内核」：清零值守记账，立刻再试一次。
///
/// 这不是把手动启停放回来——运维权仍在值守手里，这个按钮只在值守放弃之后出现，
/// 让用户在换路径、插回移动硬盘、关掉杀软之后能叫它再试，而不必重开程序。
async fn core_retry(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    {
        let mut st = ctx.maint.state();
        st.core_watch = CoreWatch::default();
    }
    set_core_retry_needed(ui, false);
    set_core_status(ui, t("正在重试启动内核…"));
    attempt_core_start(ctx, ui).await;
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
    // 值守放在最前面：内核没了，后面这些轮询全都无从谈起。
    supervise_core(ctx, ui).await;
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
    // 本机目录的变化靠系统通知，这里只收一拍事件队列，不扫盘。
    let tasks: Vec<SyncTask> = {
        let cfg = ctx.cfg.lock().unwrap();
        cfg.sync_tasks.clone()
    };
    let (ready, problems) = {
        let now = Instant::now();
        let mut st = ctx.maint.state();
        // 该盯的目录对齐一遍：任务删了、停用了、方向改了，监视线程跟着撤掉。
        let mut problems = st.watchers.sync(&tasks);
        let tick = st.watchers.pump(now);
        problems.extend(tick.problems);
        (tick.ready, problems)
    };
    if let Some((id, why)) = problems.first() {
        let name = tasks
            .iter()
            .find(|t| &t.id == id)
            .map(display_name)
            .unwrap_or_else(|| id.clone());
        set_sync_status(
            ui,
            f(
                "任务「{}」的文件夹看不住，这一轮不自动同步：{}",
                &[&name, why],
            ),
        );
    }
    if !ready.is_empty() {
        let fired: Vec<SyncTask> = tasks
            .iter()
            .filter(|t| ready.iter().any(|id| id == &t.id))
            .cloned()
            .collect();
        run_tasks(ctx, ui, fired).await;
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
    // 这一轮结束了：把监视端记的"正在跑"清掉。失败时待同步状态还留着，过一会儿再试。
    {
        let now = Instant::now();
        let mut st = ctx.maint.state();
        for item in &finished {
            st.watchers
                .note_round_finished(&item.task_id, item.status == "ok", now);
        }
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
    run_tasks(ctx, ui, due).await;
}

/// 起一轮这些任务。内核没连上、或者已经有 job 在跑的任务都直接跳过。
async fn run_tasks(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, tasks: Vec<SyncTask>) {
    if tasks.is_empty() {
        return;
    }
    if !ctx.rc.is_live() {
        // 连内核都没连上：变化是欠着的，得把它记回去，否则监视端以为这一轮已经在跑。
        let now = Instant::now();
        let mut st = ctx.maint.state();
        for task in &tasks {
            st.watchers.note_started(&task.id, false, now);
        }
        return;
    }
    let due: Vec<SyncTask> = tasks
        .into_iter()
        .filter(|t| !ctx.maint.engine.is_active(&t.id))
        .collect();
    if due.is_empty() {
        return;
    }
    let client = ctx.rc.client().await;
    let mut started = Vec::new();
    for task in due {
        match sync_tasks::start(&client, &ctx.maint.engine, &task).await {
            Ok(jobid) => started.push((task, jobid)),
            Err(e) => {
                ctx.maint
                    .state()
                    .watchers
                    .note_started(&task.id, false, Instant::now());
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

/// 旧的「外部 rclone」连法 → 托管内核。
///
/// 新界面不再让用户填 RC 地址和口令，也不再提供手动连接/断开：内核随本应用启停，
/// 连接由程序自己建立。因此 `core.mode == "external"` 已经没有对应界面了——
/// 留着它，程序启动时就会去连一个用户既看不见、也没法再改的地址，
/// 而界面上会一直显示"没连上"却说不出原因。
///
/// **只改 `mode`，别的字段一个字节都不动。** `rclone.url/user/pass/connect_on_start`
/// 原样留在配置里：用户当初填它是有原因的（那台机器上的 rclone 可能还跑着别的服务），
/// 删掉等于替他决定以后不再用。`core.path`、`kept_paths`、`flavor` 同理。
///
/// 不需要另外记一个"已迁移"标志：迁移完 `mode` 就是 `"managed"`，
/// 下次再进来这个函数直接返回，天然只做一次。
pub fn migrate_external_to_managed(ctx: &Arc<Ctx>) -> Vec<String> {
    let mut notes = Vec::new();
    {
        let cfg = ctx.cfg.lock().unwrap();
        if cfg.core.normalized_mode() != "external" {
            return notes;
        }
    }
    let mut cfg = ctx.cfg.lock().unwrap();
    let old_url = cfg.rclone.url.clone();
    cfg.core.mode = "managed".to_string();
    match crate::config::save(&cfg) {
        Ok(()) => notes.push(f(
            "原来的「外部 rclone」连接方式已改为托管内核；你填的地址仍然留在配置里（{}）",
            &[&old_url],
        )),
        Err(e) => notes.push(f("已改用托管内核，但保存配置失败：{}", &[&e])),
    }
    drop(cfg);
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
            auto_sync: None,
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

    /// 启动失败不能被报成「已重启」。
    ///
    /// `start_managed_core` 成功和失败都返回 `Some`（失败时 `Some` 里是原因），
    /// 所以判定只能看进程在不在。这里连「进程不在却拿着一个地址」的畸形输入也
    /// 一并钉住：那种情况下说「已重启并连接」比不说更糟。
    #[test]
    fn a_failed_start_is_not_a_restart() {
        let why = "内核启动失败：没找到内核";
        assert_eq!(
            restart_outcome(Some(why), false, false),
            RestartOutcome::NotRunning(why)
        );
        assert_eq!(
            restart_outcome(Some("http://127.0.0.1:55981/"), false, true),
            RestartOutcome::NotRunning("http://127.0.0.1:55981/")
        );
    }

    /// 起来了但没连上，要和「起来了也连上了」分开说，否则用户以为已经能用。
    #[test]
    fn running_without_a_handshake_is_its_own_outcome() {
        let started = Some("内核已启动：http://127.0.0.1:55981/");
        assert_eq!(
            restart_outcome(started, true, false),
            RestartOutcome::RunningOffline
        );
        assert_eq!(
            restart_outcome(started, true, true),
            RestartOutcome::Connected
        );
    }

    /// 连原因都没有时也要留一句话，别让状态栏停在半句上。
    #[test]
    fn a_silent_start_failure_still_gets_a_reason() {
        match restart_outcome(None, false, false) {
            RestartOutcome::NotRunning(why) => assert!(!why.trim().is_empty()),
            other => panic!("{other:?}"),
        }
    }

    /// 取 `core_install` 的函数体源码。
    ///
    /// `Ctx` 在单测里构造不出来（要插件宿、KV、tokio 全套），`crates/app/tests/`
    /// 又 import 不到 bin crate，所以凡是要守住"这一步确实接上了"的地方都退到这里
    /// 扫源码。这不是镜像实现：断言的是调用点存在，行为由被调用的函数各自的测试负责。
    fn core_install_body() -> &'static str {
        let src = include_str!("maintenance.rs");
        // 用两个函数名做两端锚点，不写死行尾：CRLF 检出下 `"\n}\n"` 这类终止符永远匹配不到。
        crate::source_scan::region(src, "async fn core_install(", "async fn core_start(")
            .expect("core_install 与 core_start 都应该还在，并且相邻")
    }

    /// 换内核之后必须重新握手——这条钉的是实际验收里卡住的那个 bug。
    #[test]
    fn installing_a_core_reconnects_instead_of_leaving_the_old_endpoint() {
        let body = core_install_body();
        assert!(
            body.contains("forget_core_endpoint("),
            "停掉自家内核后要先忘掉旧端点，否则界面一直显示那个已经不存在的端口"
        );
        assert!(
            body.contains("reconnect_after_core_change("),
            "新内核起来后要用它的新端口和新凭据重新握手"
        );
        assert!(
            !body.contains("disconnect_now("),
            "旧写法只把状态降级成「显示的是上次快照」，端口还留在旧值上"
        );
    }

    /// 外部模式和手填路径都不该走到"停内核"那一步，更不该换文件。
    ///
    /// 外部接的实例不是我们拉起的进程，手填的路径通常是用户特意挑的 fork；
    /// 两个分支必须先返回，否则点一次安装就把别人的进程或用户的选择动了。
    #[test]
    fn install_never_touches_a_kernel_it_does_not_own() {
        let body = core_install_body();
        let stop = body
            .find("ctx.maint.core.stop()")
            .expect("core_install 里应该有停内核那一步");
        let external = body
            .find("当前是外部模式，不会下载或替换内核")
            .expect("外部模式的早退不见了");
        let custom = body
            .find("已手动指定内核路径（{}），不会用下载的版本覆盖它")
            .expect("手填路径的早退不见了");
        assert!(
            external < stop,
            "外部模式必须先去返回，不能顺路停掉某个进程"
        );
        assert!(custom < stop, "手填路径必须先返回，原文件不能被动");
    }
}
