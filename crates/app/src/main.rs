// release 版不弹出控制台窗口（debug 保留便于调试）
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Rclone GUI（Windows 桌面版）：Rust + Slint + rclone RC + WASM 插件
//!
//! 架构：主线程跑 Slint UI；后台 std 线程内 tokio runtime 负责
//! rclone 生命周期 / 内嵌 HTTP / 统计轮询 / UI 命令处理；托盘独立线程。

mod backup;
mod caps;
mod cli;
mod conf_io;
mod config;
mod core;
mod http;
mod icon_asset;
mod kv;
mod kvault;
mod lightweight;
mod logging;
mod maintenance;
mod persist;
mod plugins;
mod profiles;
mod proxy;
mod reconcile;
mod refresh;
mod remote_types;
#[cfg(test)]
mod source_scan;
mod state;
mod store;
mod sync_tasks;
mod update;

/// 测试期独占界面语言。
///
/// 语言是进程级全局量（`rcgui_i18n::set_lang`）：一个用例切成英文时，同时在跑的
/// 其他用例读到的文案就是英文，断言中文的用例会随机失败。这把锁把依赖语言的用例
/// 串起来，并在作用域结束时恢复进入前的语言。
#[cfg(test)]
pub mod test_lang {
    use rcgui_i18n::{lang, set_lang, Lang};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn gate() -> &'static Mutex<()> {
        static GATE: OnceLock<Mutex<()>> = OnceLock::new();
        GATE.get_or_init(|| Mutex::new(()))
    }

    pub struct LangGuard {
        held: Option<MutexGuard<'static, ()>>,
        previous: Lang,
    }

    impl Drop for LangGuard {
        fn drop(&mut self) {
            // 先改回原语言再放锁，否则下一个用例可能拿到还没恢复的值。
            set_lang(self.previous);
            self.held.take();
        }
    }

    /// 在作用域内把界面语言固定成 `choice`，Drop 时恢复原语言。
    pub fn use_lang(choice: Lang) -> LangGuard {
        let held = gate().lock().unwrap_or_else(|e| e.into_inner());
        let previous = lang();
        set_lang(choice);
        LangGuard {
            held: Some(held),
            previous,
        }
    }
}

use anyhow::anyhow;
use icon_asset::make_icon_rgba;
use rcgui_rc::{ConnStatus, Handshake, ListEntry, RcState};
use rclone_client::{local_fs, RcloneManager};
use serde_json::{json, Value};
use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

slint::include_modules!();

// ---------------- 命令 ----------------

#[derive(Debug)]
enum Cmd {
    RefreshDashboard,
    FilesInit,
    /// 文件页刷新（seq: UI 请求序号，-1 表示总是应用结果）
    FilesRefresh {
        seq: i32,
    },
    /// 进入子目录（成功后才更新路径，防止网络慢时拼出不存在的路径）
    FilesEnter {
        name: String,
        seq: i32,
    },
    /// 返回上级目录
    FilesUp {
        seq: i32,
    },
    FilesDownload {
        name: String,
        local: PathBuf,
    },
    FilesMkdir,
    FilesDel {
        name: String,
        is_dir: bool,
    },
    FilesRename {
        from: String,
        to: String,
        is_dir: bool,
    },
    FilesCopy {
        name: String,
        is_dir: bool,
    },
    /// 上传本机文件到当前远程目录（同名自动加序号，不覆盖）
    /// dest 为 Some 时上传到指定远程目录（「上传到…」对话框），否则用当前目录
    FilesUpload {
        paths: Vec<PathBuf>,
        dest: Option<String>,
    },
    /// 多选模式勾选：shift 为真时从上次点击行到本行做范围勾选
    FilesCheck {
        idx: i32,
        shift: bool,
    },
    /// 多选模式批量选择：0=清空 1=全选 2=反选 3=进入/退出多选模式
    FilesSelect {
        mode: i32,
    },
    /// 排序（"name" / "size"）：同键再次点击切换升降序，然后重新列一次目录
    FilesSort {
        key: String,
    },
    /// 本机窗格列目录（path 为空 = 用当前路径 / 用户主目录）
    LocalRefresh {
        path: Option<String>,
    },
    /// 本机窗格进入子目录
    LocalEnter {
        name: String,
    },
    /// 本机窗格返回上级
    LocalUp,
    /// 本机窗格换一个目录（资源管理器选择器）
    LocalChooseDir,
    /// 本机窗格的某一项上传到远程当前目录
    LocalUpload {
        name: String,
        is_dir: bool,
    },
    /// 本机多选勾选：shift 为真时从上次点击行到本行做范围勾选
    LocalCheck {
        idx: i32,
        shift: bool,
    },
    /// 本机多选批量选择：0=清空 1=全选 2=反选 3=进入/退出多选模式
    LocalSelect {
        mode: i32,
    },
    /// 本机排序（"name" / "size" / "mtime"）：同键再点切换升降序后重新列目录
    LocalSort {
        key: String,
    },
    /// 「上传到…」：列出远程目标目录下的子文件夹
    LocalUploadToList {
        path: String,
        seq: i32,
    },
    /// 「上传到…」：把本机勾选项上传到指定远程目录
    LocalUploadTo {
        dest: String,
    },
    /// 批量下载到本机目录（勾选项）
    FilesDownloadMany {
        items: Vec<(String, bool)>,
        dir: PathBuf,
    },
    /// 批量删除（勾选项，已在界面上确认过）
    FilesDelMany {
        items: Vec<(String, bool)>,
    },
    /// 面包屑跳转：直接列出目标路径（成功后才更新界面）
    FilesJump {
        path: String,
        seq: i32,
    },
    /// 下载到用户选定的本地目录（文件同名自动加序号；文件夹递归复制）
    FilesDownloadToDir {
        name: String,
        dir: PathBuf,
        is_dir: bool,
    },
    /// 「移动到…」：列出目标目录下的子文件夹
    FilesMoveList {
        path: String,
        seq: i32,
    },
    /// 「移动到…」：把当前项移到目标目录
    FilesMove {
        name: String,
        is_dir: bool,
        dest: String,
    },
    MountsRefresh,
    MountAdd,
    /// 一键安装 WinFsp（走 winget）。`mirror` = 用设置页配置的镜像源而不是官方源。
    ///
    /// 单独一个命令而不是塞进 MountAdd：安装要走 winget 子进程（几十秒），
    /// 必须能在 UI 上显示「安装中…」并禁掉按钮，跟挂载表单的生命周期完全不同。
    WinFspInstall {
        mirror: bool,
    },
    MountRemove {
        point: String,
    },
    /// 从历史记录里选择远程路径（scope: 0 挂载 / 1 服务）
    HistPick {
        scope: i32,
        path: String,
    },
    BrowseOpen {
        scope: i32,
    },
    BrowseGoto {
        name: String,
    },
    BrowseOk,
    RemotesRefresh,
    /// 搜索文本变化（防抖后触发；scope: 0 远程 / 1 挂载 / 2 服务）
    SearchApply {
        scope: i32,
    },
    RemoteAdd,
    RemoteEditLoad {
        name: String,
    },
    RemoteDelete {
        name: String,
    },
    RemoteSave,
    RemoteCancel,
    RemoteTypeChanged,
    /// 类型下拉上方的搜索框：只过滤显示，不改变已选中的真实类型。
    RemoteTypeSearch(String),
    // ---------------- 代理（按 remote 分配） ----------------
    /// 打开/刷新代理配置列表（供远程页的「代理」列与配置卡片使用）
    ProxyListRefresh,
    /// 打开代理配置编辑器（`id` 为空 = 新建）
    ProxyEditLoad {
        id: String,
    },
    /// 保存代理配置编辑器内容
    ProxySave,
    /// 删除一条代理配置（UI 已确认关联影响）
    ProxyDelete {
        id: String,
    },
    /// 用户点「删除代理配置」：先算关联再弹确认（不真正删除）
    ProxyDeleteAsk {
        id: String,
    },
    /// 把某 remote 的代理设为第 `idx` 项（0 = 不代理，n = 第 n 条配置）
    ProxyBind {
        remote: String,
        idx: i32,
    },
    /// 编辑器取消
    ProxyCancel,
    ServesRefresh,
    ServeStart,
    ServeStop {
        id: String,
    },
    /// 传输页：进入 / 刷新
    TransfersEnter,
    HideDoneChanged,
    /// 应用全局限速（上传/下载）
    LimitApply,
    JobStop {
        id: i64,
    },
    OptionsLoad,
    OptionsSave,
    PluginsRescan,
    PluginToggle {
        idx: i32,
    },
    /// 从文件（zip）或文件夹添加插件包
    PluginAddFromFile,
    PluginAddFromDir,
    /// 打开插件目录
    PluginOpenDir,
    /// 展开某个插件：拉取并合成它的通用设置面板
    PluginPanelOpen {
        idx: i32,
    },
    /// 收起当前展开的面板
    PluginPanelClose,
    /// 面板里某个输入字段被改动
    PluginPanelField {
        key: String,
        value: String,
    },
    /// 从面板的下拉里**选中**一项（写盘后必须重拉面板，否则输入框显示旧值）
    PluginPanelPick {
        key: String,
        value: String,
    },
    /// 面板里某个按钮被点击
    PluginPanelAction {
        action: String,
    },
    /// 卸载插件（删目录 + 清启用记录；内置插件留墓碑）
    PluginUninstall {
        idx: i32,
    },
    /// 恢复被卸载的内置插件
    PluginRestore {
        idx: i32,
    },
    /// 按 id 恢复被卸载的内置插件（卸载后列表里没有那一行，只能靠 id）
    PluginRestoreById {
        id: String,
    },
    /// 插件列表检索词变化
    PluginSearch,
    SettingsSave,
    SettingsTest,
    /// 导出 rcgui.conf 到指定路径
    ConfExport {
        path: PathBuf,
        include_secrets: bool,
    },
    /// 解析指定 rcgui.conf 并生成预览（不写盘）
    ConfPreview {
        path: PathBuf,
    },
    /// 应用上一次预览过的导入（先备份当前档案）
    ConfApply,
    /// 复选框切换权限后：判断是否需要重启，并弹出确认弹层
    AdminToggle,
    /// 用户确认「以管理员身份运行」：按 UI 里的目标状态重启自身
    AdminRestart,
    /// 用户取消重启：把复选框恢复成当前真实权限状态
    AdminCancel,
    /// 托盘图标注册失败：把原因告知用户（不能静默）
    TrayFailed(String),
    /// 窗口从后台回到前台：立即对齐一次状态（不等下一次轮询）
    Foregrounded,
    /// 用户点了窗口的关闭按钮：按「关闭到托盘」设置决定隐藏 / 询问 / 退出
    WindowClose,
    /// 首次关闭提示里选了「确认」：勾选并隐藏到托盘
    CloseToTrayConfirm,
    /// 首次关闭提示里选了「取消」：什么都不做（下次关闭还会问）
    CloseToTrayCancel,
    /// 首次关闭提示里选了「直接关闭」：取消勾选并退出
    CloseToTrayQuit,
    /// 维护页上的按钮：`action` 是动作名，`id` 是它作用的条目（商店/同步任务），没有就是空串
    Maint {
        action: String,
        id: String,
    },
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
    /// 插件长任务（下载 / 解压 / 安装）的进度广播，UI 每 200ms 拉一次
    progress: Arc<caps::ProgressHub>,
    cfg: Arc<std::sync::Mutex<config::AppConfig>>,
    http_restart: Arc<tokio::sync::Notify>,
    /// (name, type, usage) 全量远程缓存，筛选时避免重复请求
    remotes_cache: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    /// config/providers 结果缓存（后端类型参数模板）
    providers_cache: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    /// 最近一次拉取 providers 失败的原因（空 = 成功）。用于在类型下拉为空时
    /// 告诉用户「为什么选不了类型」，而不是只给一个空下拉框。
    providers_err: Arc<std::sync::Mutex<Option<String>>>,
    /// 活动挂载缓存（含挂载中的 pending 项），用于筛选与盘符重复检测
    mounts_cache: Arc<std::sync::Mutex<Vec<MountRow>>>,
    /// 正在挂载中的 (rclone fs, 挂载点)
    pending_mounts: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// 用户已主动卸载过的挂载点（规范化 key）。
    ///
    /// 用途：一个 `MountAdd` 的「等待盘符出现」循环可能还在跑（它最长会等到 deadline），
    /// 此时用户已经点了卸载。若不记录这件事，循环醒来会看到「rclone 不认该挂载点」
    /// 并把它当成挂载失败，弹出「未生效」的错误提示 —— 而实际上是用户自己撤掉的。
    /// 等待循环每轮检查这个集合，一旦命中就静默退出、不报错。
    cancelled_mounts: Arc<std::sync::Mutex<Vec<String>>>,
    /// 运行中服务缓存，用于筛选
    serves_cache: Arc<std::sync::Mutex<Vec<ServeRow>>>,
    /// 最近一次已下发的全局限速 rate 字符串（避免重复调用 core/bwlimit）
    last_bwlimit: Arc<std::sync::Mutex<String>>,
    /// 瞬时速度采样（上次累计字节, 时刻）。rclone 的 speed 是平均值，看起来不会动。
    speed_meter: Arc<std::sync::Mutex<Option<(f64, std::time::Instant)>>>,
    /// 窗口是否可见且未最小化（由 UI 线程每 500ms 采样写入）：门控轮询
    visible: Arc<std::sync::atomic::AtomicBool>,
    /// 连接/握手进行中（防止连点重复发起）
    conn_busy: Arc<std::sync::atomic::AtomicBool>,
    /// 已通过「导入预览」校验、等待用户点「应用导入」的文档。
    /// 两步流程本身就是一次确认：预览不写盘，应用才写盘并先备份。
    pending_import: Arc<std::sync::Mutex<Option<rcgui_profile::bundle::RcguiConf>>>,
    /// 内嵌 HTTP 服务当前实例：重启前必须优雅关闭旧实例，否则端口仍被占用（os error 10048）
    http_server: Arc<tokio::sync::Mutex<Option<HttpServer>>>,
    /// 两条更新线路的仲裁器：世代号 + 用户操作闸门 + 已应用快照
    hub: Arc<state::Hub>,
    /// 远程用量（`operations/about`）上次抓取时刻：要走网络，不能跟 1s 轮询同频
    usage_at: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// 本机 rclone 盘符探测（实例不匹配提示用，带节流）
    drives: Arc<state::DriveProbe>,
    /// GUI 自己发起的「移动 / 重命名」登记（core/transferred 分不清这两者，见 `OpHints`）
    op_hints: Arc<std::sync::Mutex<OpHints>>,
    /// 传输页「已完成」行的快照：1 s 快路径直接复用，不必每跳都重取 core/transferred
    tasks_done: Arc<std::sync::Mutex<Vec<TaskRow>>>,
    /// 上次完整重建时正在传的 (job, 文件名)。与当前集合不同就说明要重取 core/transferred；
    /// None = 还没建过快照（轮询第一跳 t=1 先进快路径，必须能区分「空任务」和「还没查过」）。
    tasks_live: Arc<std::sync::Mutex<Option<std::collections::HashSet<(String, String)>>>>,
    /// 维护页的后端：rclone 内核自启、插件商店、备份、同步调度、应用更新
    maint: Arc<maintenance::Maintenance>,
    /// 新建远程时的类型清单。界面只拿到显示名，真实 id 与落盘用的 backend
    /// 都在这张表里；`RemotesG.type-idx` 是 `visible` 的下标。
    type_choices: Arc<std::sync::Mutex<TypeChoices>>,
}

/// 运行中的内嵌 HTTP 服务句柄
struct HttpServer {
    shutdown: tokio::sync::watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Ctx {
    /// 一次插件调用的能力集合。
    ///
    /// 收 `&Arc<Self>` 是为了算 [`kv_bridge_owns`]——它要看插件清单，而清单在
    /// `Ctx` 里。两个调用点（`plugins.rs` 的 HTTP 入口与面板按钮）都手里有
    /// `&Arc<Ctx>`，所以这个签名不用改动调用方。
    ///
    /// 判一次就固定下来：插件在同一次调用里发多个 `rclone.*`，不该出现「前一个
    /// 走站点 API、后一个落到 DAV」的分裂结论。
    fn caps(self: &Arc<Self>) -> Arc<caps::HostCaps> {
        Arc::new(caps::HostCaps {
            rc: self.rc.clone(),
            store: self.store.clone(),
            progress: self.progress.clone(),
            kv_bridge: kv_bridge_owns(self),
        })
    }
}

/// 「操作进行中」的 UI 标志守卫：置位后由 Drop 保证复位。
///
/// 挂载/启动服务这类操作有多条返回路径（校验失败、超时、诊断后 return），
/// 手动 `set_busy(false)` 很容易漏掉一条 —— 漏掉时按钮会永久灰着、spinner 一直转。
/// 用它把「复位」变成作用域结束时的必然结果（含提前 return 与 panic 展开）。
struct BusyGuard {
    ui: Weak<MainWindow>,
    which: BusyKind,
}

#[derive(Clone, Copy)]
enum BusyKind {
    Mounts,
    Serves,
}

impl BusyGuard {
    fn new(ui: &Weak<MainWindow>, which: BusyKind) -> Self {
        let g = Self {
            ui: ui.clone(),
            which,
        };
        g.set(true);
        g
    }

    fn set(&self, on: bool) {
        let which = self.which;
        let _ = self.ui.upgrade_in_event_loop(move |ui| match which {
            BusyKind::Mounts => ui.global::<MountsG>().set_busy(on),
            BusyKind::Serves => ui.global::<ServesG>().set_busy(on),
        });
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.set(false);
    }
}

// ---------------- 工具 ----------------

/// 设置远程页「忙碌」指示（驱动侧边栏与工具条 spinner）。
///
/// 每个耗时操作开始时置 true、结束时置 false。用 RAII 守卫保证任何提前 return
/// 或错误路径都会复位，不让 spinner 卡住。
fn set_remotes_busy(ui: &Weak<MainWindow>, busy: bool) {
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.global::<RemotesG>().set_loading(busy);
    });
}

/// 忙碌守卫：构造时置位，Drop 时复位。
struct RemotesBusyGuard(Weak<MainWindow>);

impl RemotesBusyGuard {
    fn new(ui: &Weak<MainWindow>) -> Self {
        set_remotes_busy(ui, true);
        Self(ui.clone())
    }
}

impl Drop for RemotesBusyGuard {
    fn drop(&mut self) {
        set_remotes_busy(&self.0, false);
    }
}

/// 历史记录上限。
///
/// toast 很频繁（每次刷新失败、每条恢复失败都会来一条），不设上限会无限增长，
/// 既吃内存也让界面里的 ListView 变慢。200 条足够回看一次操作的全过程。
const HISTORY_CAP: usize = 200;

/// 把一条消息追加到「历史记录」。
///
/// 与 toast 共用同一个入口：`toast_ui` 里调用，所以**任何**提示都会留档，
/// 不需要各处单独记得去记 —— 这正是「历史记录要包含所有提示」的要求。
fn push_log_history(ui: &MainWindow, kind: i32, msg: &str) {
    let g = ui.global::<ToastG>();
    let rows = g.get_history();
    let mut v: Vec<LogRow> = Vec::with_capacity(rows.row_count() + 1);
    // 最新的放最前面，用户一眼就能看到刚才发生了什么
    v.push(LogRow {
        kind: kind.to_string().into(),
        time: chrono::Local::now().format("%H:%M:%S").to_string().into(),
        msg: msg.to_string().into(),
    });
    for i in 0..rows.row_count() {
        if let Some(r) = rows.row_data(i) {
            v.push(r);
        }
        if v.len() >= HISTORY_CAP {
            break;
        }
    }
    let has_err = v.iter().any(|r| r.kind.as_str() == "2");
    // 「错误」计数只统计**界面上还留着**的错误条数（用户选的是这个语义）：
    // 历史超过上限被截断、或点「清空」后，这个数会跟着回退，不会变成只增不减的累计值。
    let err_count = v.iter().filter(|r| r.kind.as_str() == "2").count() as i32;
    g.set_history(ModelRc::from(Rc::new(VecModel::from(v))));
    g.set_has_errors(has_err);
    g.set_error_count(err_count);
}

fn toast_ui(ui: &MainWindow, kind: i32, msg: impl Into<String>) {
    // 错误统一过一遍「代理归因」：只有当 rclone 原文里带明确代理证据
    // （proxyconnect / 407 / 502）时才改写，其它错误原样透出。
    // 详见 `crate::proxy::annotate_error`。
    let msg = if kind == 2 {
        crate::proxy::annotate_error(&msg.into(), None)
    } else {
        msg.into()
    };
    push_log_history(ui, kind, &msg);
    ui.global::<ToastG>().set_kind(kind);
    ui.global::<ToastG>().set_msg(msg.into());
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

/// 「用户希望窗口现在是显示着的」：由显式的显示/隐藏动作维护，不由窗口句柄反推。
///
/// 为什么不能从 winit 句柄反推可见性：本程序开了 `SLINT_DESTROY_WINDOW_ON_HIDE`
/// （见 `init_destroy_on_hide`），隐藏会把原生窗口真正销毁，`with_winit_window`
/// 于是返回 `None`；而启动早期「窗口还没建出来」同样是 `None`。两种情况的可见性
/// 正好相反，用一个 `unwrap_or(true)` 去猜必然猜错一边——隐藏后仍被当成可见，
/// 轻量模式永远进不去（后台照旧每秒取数），而窗口一旦真被销毁就再也唤不回来。
static WINDOW_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// 让 Slint 在隐藏时销毁原生窗口（组件状态和窗口尺寸/位置仍保留）。
///
/// 必须在创建任何窗口**之前**调用：Slint 的 winit 后端在 `set_visibility(Hidden)`
/// 那一刻读这个环境变量，之后再改就没用了。
///
/// 换来的东西是实打实的：software renderer 的 surface/context 和整个原生窗口都会
/// 释放（`i-slint-backend-winit` 的 `suspend()`）；窗口再次显示时由后端的
/// `ensure_window()` 用保存下来的 attributes 重建，因此不需要我们自己记尺寸。
fn init_destroy_on_hide() {
    // 显式写成 "1"：Slint 只判断变量在不在，值是什么无所谓，但写清楚更好审。
    std::env::set_var("SLINT_DESTROY_WINDOW_ON_HIDE", "1");
}

/// 把主窗口藏到托盘（隐藏即销毁原生窗口，见 `init_destroy_on_hide`）。
///
/// 用 `hide()` 而不是最小化：最小化会在任务栏留一个按钮，而「隐藏到托盘区」
/// 的语义是任务栏里看不到它、只能从托盘图标唤起。
fn hide_to_tray(ui: &Weak<MainWindow>) {
    WINDOW_SHOWN.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = ui.upgrade_in_event_loop(|ui| {
        ui.hide().ok();
    });
}

/// 该 remote 绑定的代理地址（`host:port`）；未绑定返回 `None`。
///
/// 仅用于错误提示的**弱归因**：rclone 对「远端不可达」和「代理不可达」都会
/// 给出 `request send failed / timeout / EOF` 这类文本，没有代理证据时不能
/// 断言是谁的问题，只知道该远程配了代理时才加一句「可能与代理有关」。
fn remote_proxy_endpoint(ctx: &Arc<Ctx>, remote: &str) -> Option<String> {
    let cfg = ctx.cfg.lock().ok()?;
    crate::proxy::bound_profile(&cfg, remote).map(|p| format!("{}:{}", p.host, p.port))
}

/// 从远程缓存里取某 remote 的类型（缓存未加载时返回 `None`）。
fn remote_type(ctx: &Arc<Ctx>, remote: &str) -> Option<String> {
    let cache = ctx.remotes_cache.lock().ok()?;
    cache
        .iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(remote))
        .map(|(_, t, _)| t.clone())
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
    hk.open_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Run",
        KEY_QUERY_VALUE,
    )
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
        rcgui_i18n::t(
            "未检测到 WinFsp。挂载功能需要安装 WinFsp：https://winfsp.dev/rel/ 安装后重启本程序。",
        )
        .into(),
    )
}

/// 改 WinFsp 横幅的状态。集中一处，省得几个分支各写一遍 global。
///
/// **必须走 `upgrade_in_event_loop`**：Cmd 派发在后台线程上跑，直接
/// `upgrade()` 拿不到窗口（返回 None），改属性会静默失效——启动时那次能生效
/// 只是因为它在 UI 线程上。这里和 `toast()` 用同一套搬运动作。
fn winfsp_banner(ui: &Weak<MainWindow>, busy: bool, msg: Option<&str>, done: Option<bool>) {
    let msg = msg.map(|m| m.to_string());
    let _ = ui.upgrade_in_event_loop(move |w| {
        let g = w.global::<MountsG>();
        g.set_winfsp_busy(busy);
        if let Some(m) = msg {
            g.set_winfsp_msg(m.into());
        }
        if let Some(d) = done {
            g.set_winfsp_done(d);
        }
    });
}

/// 从子进程输出里挑一句能贴给用户看的话（winget 的输出很长，toast 只容得下一行）。
///
/// 优先 stderr 的最后一条非空行（winget 的错误都在那儿）；都没有就说「无输出」，
/// 而不是把几十行日志塞进 toast。
fn tail_line(stdout: &[u8], stderr: &[u8]) -> String {
    for raw in [stderr, stdout] {
        let s = String::from_utf8_lossy(raw);
        if let Some(last) = s
            .lines()
            .rev()
            .map(str::trim)
            // 去掉 winget 的进度条字符行（"  █████  ███" 之类）
            .find(|l| !l.is_empty() && !l.chars().all(|c| "█▒░ ".contains(c)))
        {
            let mut one = last.to_string();
            if one.chars().count() > 160 {
                one = one.chars().take(160).collect::<String>() + "…";
            }
            return one;
        }
    }
    rcgui_i18n::t("（无输出）").into()
}

/// winget 的「没有可用的较新版本」退出码（0x8A15002B）。
///
/// 包已经装着、且源里没有更新版本时 winget 就返回它——在这种场景下它不是错误，
/// 只是「无事可做」。所以判定时不能只看 `status.success()`。
const WINGET_ERR_UPDATE_NOT_APPLICABLE: i32 = -1_978_335_189;

/// 一键安装 WinFsp（走 winget）。
///
/// 顺序：装 → 立刻重测 → 按「本进程能不能用」落状态。三个细节都是踩过的坑：
/// 1. `--disable-interactivity`：不加的话 winget 会在无窗口子进程里等交互，看起来像卡死；
/// 2. 装完必须**重新检测**而不能假定成功：winget 返回 0 也可能因为「已是最新」而没装；
/// 3. 检测到已安装 ≠ 本进程能用：WinFsp 的驱动/服务在进程启动时才加载，
///    所以只置 `winfsp-done`（待重启），绝不置 `winfsp-ok`——否则用户以为能挂载，
///    真去挂载时才失败，比现在先说清楚糟得多。
///
/// 镜像源只拼 `--source <设置页里填的源名>`：源地址是用户自己 `winget source add` 注册的，
/// 程序不内置、也不替用户改 winget 配置。源名留空时直接报错，不悄悄回落官方源
/// ——那会让人以为走了镜像。
async fn install_winfsp(ui: &Weak<MainWindow>, mirror: bool, mirror_source: String) {
    let src = mirror_source.trim();
    if mirror && src.is_empty() {
        toast(
            ui,
            2,
            rcgui_i18n::t(
                "还没配置 winget 镜像源名：请到「设置 → winget 镜像源」填写，或改用官方源",
            ),
        );
        return;
    }
    // 装之前先探一次：本来就有 WinFsp 就不用再跑 winget（横幅按钮此时本就禁用了，
    // 这里是兜底，避免设置页/快捷键之类的新入口绕过按钮状态）。
    if winfsp_installed().0 {
        winfsp_banner(
            ui,
            false,
            Some(rcgui_i18n::t("WinFsp 已安装，重启本程序后生效")),
            Some(true),
        );
        return;
    }
    winfsp_banner(
        ui,
        true,
        Some(rcgui_i18n::t(
            "正在用 winget 安装 WinFsp，如弹出授权窗口请点「是」…",
        )),
        None,
    );

    let source = if mirror { Some(src.to_string()) } else { None };
    let res = tokio::task::spawn_blocking(move || -> std::io::Result<std::process::Output> {
        let mut c = std::process::Command::new("winget");
        c.args([
            "install",
            "--id",
            "WinFsp.WinFsp",
            "-e",
            "--accept-package-agreements",
            "--accept-source-agreements",
            "--disable-interactivity",
        ]);
        if let Some(s) = source.as_deref() {
            c.args(["--source", s]);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：别弹一个黑框出来
        }
        c.output()
    })
    .await;

    // 先解除 busy：卡住的转圈比慢一点更难受，后面用横幅文案讲结果
    winfsp_banner(ui, false, None, None);

    let (ok_now, _) = winfsp_installed();
    match res {
        Ok(Ok(out)) => {
            if ok_now {
                // 判定以「装完能不能检测到」为准，不以退出码为准：
                // winget 对「已经装了」会返回非 0（见下面的 UPDATE_NOT_APPLICABLE），
                // 只看退出码会把「本来就好」报成失败。
                winfsp_banner(
                    ui,
                    false,
                    Some(rcgui_i18n::t("WinFsp 已安装，重启本程序后生效")),
                    Some(true),
                );
                toast(
                    ui,
                    1,
                    rcgui_i18n::t("WinFsp 安装成功：重启本程序后即可使用挂载"),
                );
            } else if out.status.success() {
                // 退出码 0 但目录还是不在：常见于 winget 把它当「已是最新」跳过
                winfsp_banner(
                    ui,
                    false,
                    Some(rcgui_i18n::t(
                        "winget 报告成功，但仍未检测到 WinFsp；请重启本程序再试或手动安装",
                    )),
                    None,
                );
                toast(ui, 2, rcgui_i18n::t("winget 结束了，但没检测到 WinFsp"));
            } else if out.status.code() == Some(WINGET_ERR_UPDATE_NOT_APPLICABLE) {
                // winget 说「没有可用的较新版本」，即它认为已经装着；可我们检测不到。
                // 这种自相矛盾要如实讲，别报成「安装失败」把用户引去重装。
                winfsp_banner(
                    ui,
                    false,
                    Some(rcgui_i18n::t(
                        "winget 认为已是最新版本，但没检测到 WinFsp；请重启本程序再试",
                    )),
                    None,
                );
                toast(
                    ui,
                    2,
                    rcgui_i18n::t("winget 认为已是最新，但没检测到 WinFsp"),
                );
            } else {
                let why = tail_line(&out.stdout, &out.stderr);
                winfsp_banner(
                    ui,
                    false,
                    Some(&rcgui_i18n::f("安装失败：{}", &[&(why)])),
                    None,
                );
                toast(ui, 2, rcgui_i18n::f("WinFsp 安装失败：{}", &[&(why)]));
            }
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            winfsp_banner(
                ui,
                false,
                Some(rcgui_i18n::t(
                    "系统里找不到 winget（App Installer 未安装）：请手动下载安装 WinFsp",
                )),
                None,
            );
            toast(
                ui,
                2,
                rcgui_i18n::t(
                    "找不到 winget：请从 https://winfsp.dev/rel/ 手动安装，或先装「应用安装程序」",
                ),
            );
        }
        Ok(Err(e)) => {
            winfsp_banner(
                ui,
                false,
                Some(&rcgui_i18n::f("启动 winget 失败：{}", &[&(e)])),
                None,
            );
            toast(ui, 2, rcgui_i18n::f("启动 winget 失败：{}", &[&(e)]));
        }
        Err(e) => {
            winfsp_banner(
                ui,
                false,
                Some(&rcgui_i18n::f("安装线程异常：{}", &[&(e)])),
                None,
            );
            toast(ui, 2, rcgui_i18n::f("安装线程异常：{}", &[&(e)]));
        }
    }
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// 在已有名字里挑一个没被占用的：`新建文件夹` → `新建文件夹 (1)` → `新建文件夹 (2)`…
///
/// 用**半角括号 + 空格**（` (1)`），与 Windows 资源管理器的习惯一致，也方便用户
/// 直接看出这是自动加的后缀。比较时大小写不敏感（部分后端不区分大小写，
/// 用 `New` 和 `new` 会撞名）。
///
/// 抽成纯函数是为了单测：重名覆盖是「静默」缺陷 —— 不报错，只是创建失败或
/// 悄悄写到别人身上，一旦回归没人会立刻发现。
pub(crate) fn unique_dir_name(base: &str, existing: &[String]) -> String {
    let taken = |name: &str| existing.iter().any(|e| e.eq_ignore_ascii_case(name));
    if !taken(base) {
        return base.to_string();
    }
    // 上限只为防呆：目录里真有一万个同名文件夹时不要无限循环
    for n in 1..=9999 {
        let candidate = format!("{base} ({n})");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{base} (9999)")
}

fn split_name(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// 上一级路径（已在根目录时返回空串）。
fn parent_path(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// 路径面包屑：首项是远程名，之后每一级目录。
///
/// 首项的 `path` 为空串（远程根）；`is_current` 只标在最右一级上。
fn file_crumbs(remote: &str, path: &str) -> Vec<CrumbRow> {
    let mut out = Vec::new();
    if remote.is_empty() {
        return out;
    }
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    out.push(CrumbRow {
        name: remote.into(),
        path: "".into(),
        is_current: parts.is_empty(),
        is_ellipsis: false,
    });
    let mut acc = String::new();
    for (i, part) in parts.iter().enumerate() {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        out.push(CrumbRow {
            name: (*part).into(),
            path: acc.clone().into(),
            is_current: i + 1 == parts.len(),
            is_ellipsis: false,
        });
    }
    out
}

/// 估算一段文字在 13px 字号下的宽度（像素）。
///
/// Slint 的 `preferred-width` 只能在 .slint 里读，Rust 侧没有字体度量接口；
/// 这里按字符类别估算：ASCII/拉丁约 7.2px，CJK/全角约 13.2px，空格 3.6px。
/// 结果只用来决定「折叠几个祖先」，宁可估宽一点（多折一个）也不要溢出。
fn crumb_text_width(s: &str) -> f32 {
    s.chars()
        .map(|c| {
            if c == ' ' {
                3.6
            } else if (c as u32) < 0x2E80 {
                7.2
            } else {
                13.2
            }
        })
        .sum()
}

/// 一级面包屑占用的宽度（左右各 4px 内边距 + 文字）。
fn crumb_row_width(c: &CrumbRow) -> f32 {
    8.0 + crumb_text_width(c.name.as_str())
}

/// 按可用宽度折叠祖先，返回 `(显示的项, 被折叠的项)`。
///
/// 规则（像资源管理器）：
/// 1. 全部放得下 → 原样显示；
/// 2. 放不下 → 保留远程根 + 「…」 + 从右边能放下的目录（当前目录必留）；
/// 3. 连「根 + … + 当前」都放不下 → 去掉根，只保留「…」+ 后缀；
/// 4. 连「…」都放不下 → 只显示当前目录。
fn fit_crumbs(all: &[CrumbRow], avail: f32) -> (Vec<CrumbRow>, Vec<CrumbRow>) {
    if all.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // 路径框左右各 10px 内边距，再留 4px 给估算误差
    let budget = (avail - 24.0).max(40.0);
    let total: f32 =
        all.iter().map(crumb_row_width).sum::<f32>() + (all.len().saturating_sub(1) as f32) * 9.0;
    if total <= budget {
        return (all.to_vec(), Vec::new());
    }

    let ell = CrumbRow {
        name: "…".into(),
        path: "".into(),
        is_current: false,
        is_ellipsis: true,
    };
    let last = all.len() - 1;
    let ell_w = crumb_row_width(&ell) + 9.0;
    let root_w = crumb_row_width(&all[0]) + 9.0;
    let last_w = crumb_row_width(&all[last]);

    // 连「…」+ 当前都放不下：只显示当前目录
    if ell_w + last_w > budget {
        return (vec![all[last].clone()], all[..last].to_vec());
    }

    let mut suffix_start = last;
    let mut suffix_w = last_w;
    let keep_root = root_w + ell_w + last_w <= budget;
    if keep_root {
        while suffix_start > 1 {
            let w = crumb_row_width(&all[suffix_start - 1]) + 9.0;
            if root_w + ell_w + suffix_w + w > budget {
                break;
            }
            suffix_start -= 1;
            suffix_w += w;
        }
    } else {
        while suffix_start > 0 {
            let w = crumb_row_width(&all[suffix_start - 1]) + 9.0;
            if ell_w + suffix_w + w > budget {
                break;
            }
            suffix_start -= 1;
            suffix_w += w;
        }
    }

    let hidden = if keep_root {
        all[1..suffix_start].to_vec()
    } else {
        all[..suffix_start].to_vec()
    };
    let mut shown = Vec::new();
    if keep_root {
        shown.push(all[0].clone());
    }
    if !hidden.is_empty() {
        shown.push(ell);
    }
    shown.extend_from_slice(&all[suffix_start..]);
    (shown, hidden)
}

fn crumb_model(rows: Vec<CrumbRow>) -> ModelRc<CrumbRow> {
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

fn collect_crumbs(model: &ModelRc<CrumbRow>) -> Vec<CrumbRow> {
    (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect()
}

/// 把完整面包屑链按当前路径框宽度折叠后写回 UI（内容没变就不重设，避免布局抖动）。
fn apply_crumbs(ui: &MainWindow, all: Vec<CrumbRow>) {
    let g = ui.global::<FilesG>();
    let width = g.get_path_box_width();
    let (shown, hidden) = if width > 0.0 {
        fit_crumbs(&all, width)
    } else {
        // 首帧还不知道路径框多宽：先全部显示，宁可溢出也不要只剩当前目录
        (all.clone(), Vec::new())
    };
    if collect_crumbs(&g.get_crumb_all()) != all {
        g.set_crumb_all(crumb_model(all));
    }
    if collect_crumbs(&g.get_crumbs()) != shown {
        g.set_crumbs(crumb_model(shown));
    }
    if collect_crumbs(&g.get_crumb_hidden()) != hidden {
        g.set_crumb_hidden(crumb_model(hidden));
    }
}

/// 把 `operations/list` 的结果转成表格行。
///
/// 目录始终排在文件前；同类按 `key`（"name" / "size" / "mtime"）排序，`desc` 为真时反向。
/// 排序放在这里（而不是 Slint 里）是因为文件大小在 UI 上是格式化后的字符串
/// （"1.1 KB"），拿它排序会把 "9 KB" 排到 "10 KB" 后面。
fn file_rows(entries: &[ListEntry], key: &str, desc: bool) -> Vec<FileRow> {
    let mut src: Vec<&ListEntry> = entries.iter().collect();
    src.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let ord = if key == "size" {
                a.size.cmp(&b.size).then_with(|| a.name.cmp(&b.name))
            } else if key == "mtime" {
                // rclone 给的 mod_time 是 RFC3339 字符串，字典序即时间序，
                // 不必解析成时间点；同秒时退回名字比较，保证顺序稳定。
                a.mod_time
                    .cmp(&b.mod_time)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            } else {
                // 名字排序大小写不敏感：Windows 用户对 a.txt / B.txt 混排有直觉预期
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            };
            if desc {
                ord.reverse()
            } else {
                ord
            }
        })
    });
    src.into_iter()
        .map(|e| FileRow {
            name: e.name.clone().into(),
            is_dir: e.is_dir,
            size: fmt_bytes(e.size as f64).into(),
            mtime: fmt_mtime(&e.mod_time).into(),
        })
        .collect()
}

/// 目录里挑一个没被占用的文件名：`a.txt` → `a (1).txt`（保留扩展名）。
///
/// 上传时一律避开同名项：rclone 的 copyfile 会直接覆盖，而用户从资源管理器
/// 拖进来的往往是同名的新版本，静默覆盖不是一个安全的默认行为。
fn unique_file_name(name: &str, existing: &[String]) -> String {
    let taken = |n: &str| existing.iter().any(|e| e.eq_ignore_ascii_case(n));
    if !taken(name) {
        return name.to_string();
    }
    let (base, ext) = split_name(name);
    for i in 1..=9999 {
        let candidate = format!("{base} ({i}){ext}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{base} (9999){ext}")
}

/// 本机目录列表（双栏左半用）：目录在前，同类按 `key`（"name" / "size" / "mtime"）。
///
/// 完全走 `std::fs`：浏览本机不该产生 RC 请求；读不到元数据的项（权限/竞态）跳过，
/// 不让一行坏数据毁掉整个目录。
fn local_rows(dir: &Path, key: &str, desc: bool) -> anyhow::Result<Vec<FileRow>> {
    let mut items: Vec<(String, bool, u64, std::time::SystemTime)> = Vec::new();
    for ent in std::fs::read_dir(dir)? {
        let Ok(ent) = ent else { continue };
        let name = ent.file_name().to_string_lossy().to_string();
        let Ok(meta) = ent.metadata() else { continue };
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() };
        let mtime = meta
            .modified()
            .unwrap_or_else(|_| std::time::SystemTime::now());
        items.push((name, is_dir, size, mtime));
    }
    items.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| {
            // 与远程侧同一套语义：大小/时间相同时退回名字，保证顺序稳定。
            // `SystemTime` 可以直接比较（内部是 Duration since UNIX_EPOCH）。
            let ord = if key == "size" {
                a.2.cmp(&b.2)
                    .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
            } else if key == "mtime" {
                a.3.cmp(&b.3)
                    .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
            } else {
                a.0.to_lowercase().cmp(&b.0.to_lowercase())
            };
            if desc {
                ord.reverse()
            } else {
                ord
            }
        })
    });
    Ok(items
        .into_iter()
        .map(|(name, is_dir, size, mtime)| FileRow {
            name: name.into(),
            is_dir,
            size: fmt_bytes(size as f64).into(),
            mtime: fmt_local_mtime(mtime).into(),
        })
        .collect())
}

/// 本机文件时间 → 与 `fmt_mtime` 同款的 "YYYY-MM-DD HH:MM"（本机时区）。
///
/// rclone 侧给的是 RFC3339 字符串，本机侧拿到的是 `SystemTime`，两者格式要一致，
/// 否则同一个列表里两种时间样式并存。
fn fmt_local_mtime(t: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// 写入文件列表：同时重建勾选数组。
///
/// Slint 侧按 `FilesG.checked[i]` 索引取勾选态，两个数组长度必须一致，
/// 否则越界取值。因此所有写 entries 的地方都走这个函数。
fn set_file_entries(ui: &MainWindow, rows: Vec<FileRow>) {
    // 轻量模式期间界面不在屏幕上，这时才回来的取数结果不写进模型：
    // 写进去等于把刚清掉的几千行重新装回来，而用户根本看不到。
    if crate::lightweight::suppress_rows(rows.len()) {
        return;
    }
    let g = ui.global::<FilesG>();
    let n = rows.len();
    g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![false; n]))));
    g.set_checked_count(0);
    g.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// 当前被勾选的条目（多选模式下的 下载到本机 / 删除 用）。
fn checked_items(ui: &MainWindow) -> Vec<(String, bool)> {
    let g = ui.global::<FilesG>();
    let entries = g.get_entries();
    let checked = g.get_checked();
    let mut out = Vec::new();
    for i in 0..entries.row_count() {
        if !checked.row_data(i).unwrap_or(false) {
            continue;
        }
        if let Some(r) = entries.row_data(i) {
            out.push((r.name.to_string(), r.is_dir));
        }
    }
    out
}

/// 本机窗格当前被勾选的条目（本机多选「上传」用）。
fn local_checked_items(ui: &MainWindow) -> Vec<(String, bool)> {
    let g = ui.global::<LocalG>();
    let entries = g.get_entries();
    let checked = g.get_checked();
    let mut out = Vec::new();
    for i in 0..entries.row_count() {
        if !checked.row_data(i).unwrap_or(false) {
            continue;
        }
        if let Some(r) = entries.row_data(i) {
            out.push((r.name.to_string(), r.is_dir));
        }
    }
    out
}

/// 本机目录下不冲突的落盘路径：`name`、`name (1)`、`name (2)`…
///
/// 右键「下载到…」用文件夹选择器，没有「是否覆盖」的原生确认框，
/// 因此这里一律避开已存在的文件，宁可多一个副本也不悄悄覆盖。
fn unique_local_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (base, ext) = split_name(name);
    for i in 1..=999 {
        let cand = if ext.is_empty() {
            format!("{base} ({i})")
        } else {
            format!("{base} ({i}){ext}")
        };
        let p = dir.join(cand);
        if !p.exists() {
            return p;
        }
    }
    first
}

/// 本地目录下不冲突的目录名：`name`、`name (1)`、`name (2)`…
///
/// 文件夹下载要和现有目录合并吗？**不**：`sync/copy` 会覆盖同名文件，
/// 与其冒覆盖风险，不如新建一个带序号的目录。
fn unique_local_dir(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    for i in 1..=999 {
        let p = dir.join(format!("{name} ({i})"));
        if !p.exists() {
            return p;
        }
    }
    first
}

/// `path` 是否等于 `ancestor` 或位于其内部（用于阻止把文件夹移进自己）。
fn is_same_or_inside(path: &str, ancestor: &str) -> bool {
    path == ancestor || path.starts_with(&format!("{ancestor}/"))
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
            // 代理列由下面的 set_rows 之后再统一填（需要读档案，避免这里再开一次盘）
            proxy_label: rcgui_i18n::t("不代理").into(),
            proxy_pending: false,
            proxy_conflict: false,
            proxy_index: 0,
        })
        .collect();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        if crate::lightweight::suppress_rows(rows.len()) {
            return;
        }
        ui.global::<RemotesG>()
            .set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    });
    // 填代理列（徽标：待应用 / 需要确认）
    apply_proxy_column(ui).await;
}

/// 把档案里的代理绑定状态刷到远程列表的「代理」列。
///
/// 单独一个函数是因为它在多个时机要重跑：列表刷新、绑定切换后、连接成功后。
async fn apply_proxy_column(ui: &Weak<MainWindow>) {
    let snap = {
        let doc = crate::profiles::store().load().ok().flatten();
        let d = doc.unwrap_or_default();
        let reg = rcgui_profile::ProxyRegistry::new(&d.proxies, &d.remote_proxies);
        // 下拉框候选项（第 0 项固定「不代理」），顺序即 proxies 顺序
        let mut options: Vec<slint::SharedString> = vec![rcgui_i18n::t("不代理").into()];
        let mut hints: Vec<slint::SharedString> = vec![proxy::direct_hint().into()];
        for p in &d.proxies {
            options.push(format!("{}（{}）", p.name, p.display_target()).into());
            hints.push(proxy::credential_hint().into());
        }
        let mut rows_state: std::collections::HashMap<String, (String, bool, bool, i32)> =
            std::collections::HashMap::new();
        for b in &d.remote_proxies {
            let bound = reg.by_id(&b.proxy_id);
            let label = match (b.is_direct(), bound) {
                (true, _) => rcgui_i18n::t("不代理").to_string(),
                (false, Some(p)) => p.display_target(),
                // 悬空引用：不能假装代理已生效
                (false, None) => rcgui_i18n::t("配置已删除").to_string(),
            };
            let idx = if b.is_direct() {
                0
            } else {
                d.proxies
                    .iter()
                    .position(|p| p.id == b.proxy_id)
                    .map(|p| p as i32 + 1)
                    .unwrap_or(0)
            };
            rows_state.insert(
                b.remote.clone(),
                (
                    label,
                    b.needs_apply(),
                    !b.is_direct() && bound.is_none(),
                    idx,
                ),
            );
        }
        (options, hints, rows_state)
    };
    let (options, hints, state) = snap;
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let g = ui.global::<RemotesG>();
        g.set_proxy_options(ModelRc::from(Rc::new(VecModel::from(options))));
        g.set_proxy_option_hints(ModelRc::from(Rc::new(VecModel::from(hints))));
        let rows = g.get_rows();
        for i in 0..rows.row_count() {
            if let Some(mut r) = rows.row_data(i) {
                let name = r.name.to_string();
                let (label, pending, conflict, idx) = state
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| (rcgui_i18n::t("不代理").to_string(), false, false, 0));
                r.proxy_label = label.into();
                r.proxy_pending = pending;
                r.proxy_conflict = conflict;
                r.proxy_index = idx;
                rows.set_row_data(i, r);
            }
        }
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
        if crate::lightweight::suppress_rows(rows.len()) {
            return;
        }
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
        if crate::lightweight::suppress_rows(rows.len()) {
            return;
        }
        ui.global::<ServesG>()
            .set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    });
}

fn remote_name_of(fs: &str) -> Option<String> {
    let fs = fs.strip_prefix("//?/").unwrap_or(fs);
    match fs.find(':') {
        Some(i) => {
            let name = &fs[..i];
            if name.is_empty() || name.len() == 1 || name.contains('/') || name.contains('\\') {
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

/// 传输进度，形如 "46%"；不足 1% 显示 "<1%"。
/// 文件大小未知或百分比不可信（负数、NaN）时返回 "—"，不拿 0% 冒充真实进度。
fn fmt_percent(size: f64, pct: f64) -> String {
    if size <= 0.0 || !pct.is_finite() || pct < 0.0 {
        return "—".into();
    }
    let p = pct.clamp(0.0, 100.0);
    if p > 0.0 && p < 1.0 {
        "<1%".into()
    } else {
        format!("{:.0}%", p)
    }
}

/// 从 rclone 的 group 名（形如 "job/12345"）取 job 号。
/// 取不到时给 0：调用方据此判断该行没有可停的 job，避免把 0 当成真任务发出去。
fn job_id_of(group: &str) -> i32 {
    group
        .strip_prefix("job/")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0) as i32
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

/// 判断一条传输是否来自某个 serve，返回服务名（无法确证时返回空）。
///
/// **为什么不能用 fs 前缀匹配**：`serve/list` 里根目录服务的 `fs` 就是 `teldrive:`，
/// 而 `core/transferred` 的 `srcFs`/`dstFs` 也是 `teldrive:`。用 `starts_with` 判断，
/// 任何来自该远程的任务都会被算作「来自 serve」，而且会命中列表里第一个服务
/// （表现为所有任务都带同一个、且是列表最上方的那个服务标签）。
///
/// rclone 本身**不把传输归因到 serve**：`core/transferred` 只有 `group`
/// （`global_stats` 或 `job/N`），没有 serve 关联信息。因此这里只在能确证时打标签：
/// 服务挂了**子路径**（`fs = teldrive:some/sub`）时，任务路径落在该前缀下才算命中；
/// 根目录服务（`fs = teldrive:`）无法与普通传输区分，一律不标。
fn match_serve_for_task(serves: &[ServeRow], src: &str, dst: &str) -> slint::SharedString {
    // 只考虑带子路径的服务，并按子路径从长到短匹配（更具体的优先）
    let mut candidates: Vec<(&str, &str)> = serves
        .iter()
        .filter_map(|sv| {
            let fs = sv.fs.as_str();
            // 去掉结尾的 ':' 得到 "remote" 或 "remote:sub/path"
            let body = fs.strip_suffix(':').unwrap_or(fs);
            // 只有形如 remote:sub 的才具备可判定的路径前缀
            let (_, sub) = body.split_once(':')?;
            let sub = sub.trim_matches('/');
            if sub.is_empty() {
                return None; // 根目录服务：无法与普通传输区分
            }
            Some((sv.name.as_str(), sub))
        })
        .collect();
    candidates.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    for (name, sub) in candidates {
        // 任务的 fs 形如 "remote:sub/..."，只要其子路径以服务的子路径开头即命中
        let hit = |fs: &str| -> bool {
            let body = fs.strip_suffix(':').unwrap_or(fs);
            match body.split_once(':') {
                Some((_, path)) => {
                    let p = path.trim_matches('/');
                    p == sub || p.starts_with(&format!("{sub}/"))
                }
                None => false,
            }
        };
        if hit(src) || hit(dst) {
            return name.into();
        }
    }
    String::new().into()
}

/// GUI 自己发起的「移动 / 重命名」登记。
///
/// 为什么需要登记：`core/transferred` 只给出 srcFs/dstFs 两个 fs 串，
/// - 文件级 `operations/movefile` 两侧传的是同一个 root（`testlocal:`），
///   于是「同目录改名」与「换目录移动」在记账上完全同形（srcFs == dstFs）；
/// - 目录级 `sync/move` 若走服务端 DirMove，一条都不记；逐文件搬时才记，
///   此时 srcFs 是源目录（`testlocal:dirA`），与文件级形状又不一样。
/// 光看记账猜不出，就由发起的一侧把真实操作记下来，构建列表时优先采用。
#[derive(Default, Clone)]
struct OpHints {
    /// (fs, 相对路径, 标签, 登记时刻)
    items: std::collections::VecDeque<(String, String, String, std::time::Instant)>,
}

impl OpHints {
    /// 登记一次操作：`fs` 是 RC 调用里用的 fs 串（`remote:`），`path` 是相对它的路径。
    fn put(&mut self, fs: &str, path: &str, kind: &str) {
        // 只用来给最近的操作贴标签，过期即弃，避免旧登记误标后来的同名路径
        const TTL: std::time::Duration = std::time::Duration::from_secs(900);
        self.items.retain(|(_, _, _, t)| t.elapsed() < TTL);
        self.items.push_back((
            fs.to_string(),
            path.to_string(),
            kind.to_string(),
            std::time::Instant::now(),
        ));
        while self.items.len() > 256 {
            self.items.pop_front();
        }
    }

    /// 按记账条目反查：`src_fs` / `name` 取自 core/transferred。
    ///
    /// 记账里的 srcFs 有两种形态，都要能对上：
    /// - 远程名：`testlocal:dirA`（真正的远程后端）；
    /// - 后端解析后的本机路径：`//?/D:/.../dirA`（alias / local 这类 remote 会被解析，实测如此）。
    /// Windows 路径大小写不敏感，统一按「正斜杠 + 小写」比较。
    fn lookup(&self, src_fs: &str, name: &str) -> Option<&str> {
        let low = |s: &str| s.replace('\\', "/").to_lowercase();
        let src = low(src_fs);
        let name_low = low(name);
        let joined = format!("{}/{}", src.trim_end_matches('/'), name_low);
        self.items
            .iter()
            .rev()
            .find(|(fs, path, _, _)| {
                let fs_low = low(fs);
                let path_low = low(path);
                let path_low = path_low.trim_end_matches('/');
                // 文件级：srcFs 就是登记的 fs，name 落在登记路径之下
                if src == fs_low
                    && (name_low == path_low || name_low.starts_with(&format!("{path_low}/")))
                {
                    return true;
                }
                // 目录级：srcFs == fs + path（逐文件搬时以源目录为 fs）
                if src == format!("{fs_low}{path_low}") || joined == format!("{fs_low}{path_low}") {
                    return true;
                }
                // 解析成本机路径的形态：只比对路径尾巴（`.../dirA`、`.../dirA/x.txt`）
                src.ends_with(&format!("/{path_low}")) || joined.ends_with(&format!("/{path_low}"))
            })
            .map(|(_, _, kind, _)| kind.as_str())
    }
}

/// core/transferred 单项的派生信息（构建后台任务行用）
struct TItem {
    name: String,
    src: String,
    dst: String,
    bytes: f64,
    size: f64,
    what: String,
}

/// 由 core/transferred 单项推断任务操作标签。
///
/// rclone 的记账约定（v1.76 实测）：
/// - 真的搬了数据：bytes > 0（上传/下载/复制）；
/// - 服务端改名或移动：bytes == 0，且 srcFs/dstFs 都有值——两者相同即同一目录改名，不同即移动；
/// - 删除：what == "deleting"，此时不记 srcFs/dstFs。
/// 0 字节文件的上传/复制在数据上与改名无法区分，这类不推断标签，避免误标；
/// 改名/移动还要求两端 fs 非空——旧版 rclone 没有 what 字段时，删除条目只剩空 srcFs/dstFs。
///
/// `hint` 是 GUI 自己发起时登记的标签（见 [`OpHints`]）：同一 root 下的改名与换目录移动
/// 在记账里同形，只有发起方知道到底是哪一种，所以 bytes == 0 时优先照登记来。
fn task_op(it: &TItem, hint: Option<&str>) -> String {
    let src_remote = remote_name_of(&it.src);
    let dst_remote = remote_name_of(&it.dst);
    if it.what == "deleting" {
        return rcgui_i18n::t("删除").to_string();
    }
    if it.bytes == 0.0 {
        if let Some(kind) = hint {
            return kind.to_string();
        }
    }
    if it.bytes == 0.0 && it.size > 0.0 && !it.src.is_empty() && !it.dst.is_empty() {
        if it.src == it.dst {
            return rcgui_i18n::t("重命名").to_string();
        }
        if src_remote.is_some() && dst_remote.is_some() {
            return rcgui_i18n::t("移动").to_string();
        }
    }
    if dst_remote.is_some() && src_remote.is_none() {
        return rcgui_i18n::t("上传").to_string();
    }
    if src_remote.is_some() && dst_remote.is_none() {
        return rcgui_i18n::t("下载").to_string();
    }
    String::new()
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
    let mut tmap: std::collections::HashMap<String, TItem> = std::collections::HashMap::new();
    if let Ok(v) = client.core_transferred().await {
        let arr = v
            .get("transferred")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        for it in arr {
            let group = it
                .get("group")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let name = it
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let src = it
                .get("srcFs")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let dst = it
                .get("dstFs")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let bytes = it.get("bytes").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let size = it.get("size").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let what = it
                .get("what")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            tmap.insert(
                group,
                TItem {
                    name,
                    src,
                    dst,
                    bytes,
                    size,
                    what,
                },
            );
        }
    }

    // core/transferred 只在传输「完成后」才追加条目，进行中的传输只出现在 core/stats.transferring。
    // 两处都得读：transferring 建运行中的行，transferred 建已完成的行。
    let transferring = client
        .core_stats()
        .await
        .map(|s| s.transferring)
        .unwrap_or_default();
    publish_speeds(ui, &transferring);

    let serves = ctx.serves_cache.lock().unwrap().clone();
    // GUI 自己发起的移动/改名登记：记账分辨不出这两者，能对上就照登记贴标签
    let hints = ctx.op_hints.lock().unwrap().clone();

    // 进行中的文件：一个文件一行（同一个 job 并发传多个文件时各占一行）
    let (mut rows, live) = running_task_rows(&transferring, &serves, &hints);

    // 已完成的任务；`live` 里还在传的条目跳过，免得同一个文件在列表里出现两次
    let done_rows = done_task_rows(&tmap, &live, &serves, &hints);
    // 快路径（`refresh_tasks_fast`）复用这两份快照：已完成行原样写回，运行集合用来判断
    // 「还是同一批任务在跑」还是「有文件开始/传完，必须重取 core/transferred」。
    ctx.tasks_done.lock().unwrap().clone_from(&done_rows);
    *ctx.tasks_live.lock().unwrap() = Some(live);
    rows.extend(done_rows);
    publish_task_rows(ui, hide, &filter, rows);
}

/// 运行中的传输行：一个文件一行（同一个 job 并发传多个文件时各占一行）。
///
/// 第二项返回这些行的 `(job, 文件名)`。刚传完的文件会短暂同时出现在
/// core/stats.transferring 与 core/transferred 里，补已完成行时靠它跳过同一条。
fn running_task_rows(
    transferring: &[rcgui_rc::Transferring],
    serves: &[ServeRow],
    hints: &OpHints,
) -> (Vec<TaskRow>, std::collections::HashSet<(String, String)>) {
    let mut rows: Vec<TaskRow> = Vec::new();
    let mut live: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for t in transferring {
        if t.name.is_empty() {
            continue;
        }
        live.insert((t.group.clone(), t.name.clone()));
        // bytes 非 0 表示任务正在搬数据，task_op 据此走上传/下载判断，
        // 不会被误认成「移动」或「重命名」。
        let probe = TItem {
            name: t.name.clone(),
            src: t.src_fs.clone(),
            dst: t.dst_fs.clone(),
            bytes: 1.0,
            size: t.size,
            what: String::new(),
        };
        let op = task_op(&probe, hints.lookup(&probe.src, &probe.name));
        let remote = remote_name_of(&probe.dst)
            .or_else(|| remote_name_of(&probe.src))
            .unwrap_or_default();
        let svc = match_serve_for_task(serves, &probe.src, &probe.dst);
        rows.push(TaskRow {
            id: job_id_of(&t.group),
            op: op.into(),
            remote: remote.into(),
            path: t.name.clone().into(),
            svc: svc.into(),
            status: rcgui_i18n::t("运行中").into(),
            duration: "—".into(),
            speed: fmt_speed(t.speed).into(),
            progress: fmt_percent(t.size, t.percentage).into(),
            done: false,
        });
    }
    (rows, live)
}

/// 已完成的行：core/transferred 只含真正传过的文件，不含 about/list 等内部操作 job。
/// 它是按 group 建的表，同一个 job 传过多个文件时只剩最后一条，正好「一个 job 一行」。
///
/// 这里不看「隐藏已完成」开关：开关只决定显示，快路径要能从缓存里补回这些行。
fn done_task_rows(
    tmap: &std::collections::HashMap<String, TItem>,
    live: &std::collections::HashSet<(String, String)>,
    serves: &[ServeRow],
    hints: &OpHints,
) -> Vec<TaskRow> {
    let mut rows: Vec<TaskRow> = Vec::new();
    for (group, it) in tmap {
        if live.contains(&(group.clone(), it.name.clone())) {
            continue;
        }
        let op = task_op(it, hints.lookup(&it.src, &it.name));
        let remote = remote_name_of(&it.dst)
            .or_else(|| remote_name_of(&it.src))
            .unwrap_or_default();

        let svc = match_serve_for_task(serves, &it.src, &it.dst);

        rows.push(TaskRow {
            id: job_id_of(group),
            op: op.into(),
            remote: remote.into(),
            path: it.name.clone().into(),
            svc: svc.into(),
            status: rcgui_i18n::t("已完成").into(),
            duration: "—".into(),
            speed: "—".into(),
            progress: "100%".into(),
            done: true,
        });
    }
    rows
}

/// 顶部那两个总速（上行/下行）。两档刷新都要更新，抽出来免得各写一遍。
fn publish_speeds(ui: &Weak<MainWindow>, transferring: &[rcgui_rc::Transferring]) {
    let (mut up_total, mut down_total) = (0.0f64, 0.0f64);
    for t in transferring {
        // 上传：目标在远程；下载：来源在远程
        if remote_name_of(&t.dst_fs).is_some() {
            up_total += t.speed;
        }
        if remote_name_of(&t.src_fs).is_some() {
            down_total += t.speed;
        }
    }
    let (up, down) = (fmt_speed(up_total), fmt_speed(down_total));
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let g = ui.global::<TransfersG>();
        g.set_up_speed(up.into());
        g.set_down_speed(down.into());
    });
}

/// 排序 + 搜索过滤 + 写进界面，两档刷新共用的最后一步。
fn publish_task_rows(ui: &Weak<MainWindow>, hide_done: bool, filter: &str, mut rows: Vec<TaskRow>) {
    if hide_done {
        rows.retain(|t| !t.done);
    }
    // 运行中的排在前面；同一状态里新任务在前（id 大 = 后提交）
    rows.sort_by(|a, b| a.done.cmp(&b.done).then(b.id.cmp(&a.id)));
    // 任务搜索过滤（匹配 标签/远程/路径/服务/状态）
    if !filter.is_empty() {
        rows.retain(|t| {
            t.op.as_str().contains(filter)
                || t.remote.as_str().to_lowercase().contains(filter)
                || t.path.as_str().to_lowercase().contains(filter)
                || t.svc.as_str().to_lowercase().contains(filter)
                || t.status.as_str().contains(filter)
        });
    }

    let _ = ui.upgrade_in_event_loop(move |ui| {
        if crate::lightweight::suppress_rows(rows.len()) {
            return;
        }
        ui.global::<TransfersG>()
            .set_tasks(ModelRc::from(Rc::new(VecModel::from(rows))));
    });
}

/// 传输页每秒走的快路径：只读 `core/stats`（响应小），把运行中行的速度/进度就地更新，
/// 已完成的行直接用上一次完整重建的快照。
///
/// `core/transferred` 的响应随历史文件数增长（大批量任务可达数 MB），所以它留在 10 s 那一档。
/// 但运行集合一变（有文件开始、有文件传完、任务全部结束）就立刻转一次完整重建：
/// 否则刚传完的文件要等到下一次 10 s 重建才出现，界面上看着像「卡住不动」。
async fn refresh_tasks_fast(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let client = ctx.rc.client().await;
    let Ok(stats) = client.core_stats().await else {
        return;
    };
    let live_now: std::collections::HashSet<(String, String)> = stats
        .transferring
        .iter()
        .filter(|t| !t.name.is_empty())
        .map(|t| (t.group.clone(), t.name.clone()))
        .collect();
    // None = 还没有完整快照（轮询第一跳就会走到这里），此时必须先重建，否则会把列表清空
    let same_batch = {
        let cached = ctx.tasks_live.lock().unwrap();
        cached.as_ref().map(|c| c == &live_now).unwrap_or(false)
    };
    if !same_batch {
        build_tasks(ctx, ui).await;
        return;
    }

    let (hide, filter) = read_ui(ui, |u| {
        let g = u.global::<TransfersG>();
        (
            g.get_hide_done(),
            g.get_task_search().to_string().to_lowercase(),
        )
    })
    .await
    .unwrap_or((false, String::new()));
    publish_speeds(ui, &stats.transferring);
    let serves = ctx.serves_cache.lock().unwrap().clone();
    let hints = ctx.op_hints.lock().unwrap().clone();
    let (mut rows, _live) = running_task_rows(&stats.transferring, &serves, &hints);
    rows.extend(ctx.tasks_done.lock().unwrap().iter().cloned());
    publish_task_rows(ui, hide, &filter, rows);
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
        rcgui_i18n::f(
            "上传 {} · 下载 {}",
            &[
                &(up.map(fmt_bytes)
                    .unwrap_or_else(|| rcgui_i18n::t("不限").into())),
                &(down
                    .map(fmt_bytes)
                    .unwrap_or_else(|| rcgui_i18n::t("不限").into())),
            ],
        )
    };
    set_effective(ui, disp);
}

/// 把 rclone `Duration` 的纳秒数格式化成带单位文本（"10ms"、"1h30m"、"off"）
///
/// 必须带单位：`fs.ParseDuration` 把无单位数字当**秒**，所以裸数字
/// `10000000` 会被读成 10⁷ 秒（115 天），webdav 的 pacer 直接饿死。
fn fmt_duration_ns(ns: i64) -> String {
    // DurationOff：rclone 用 math.MaxInt64 表示 "off"
    if ns == i64::MAX {
        return "off".to_string();
    }
    if ns == 0 {
        return "0s".to_string();
    }
    let neg = ns < 0;
    let u = ns.unsigned_abs();
    let mut out = String::new();
    if u < 1_000 {
        out.push_str(&format!("{u}ns"));
    } else {
        // 从大到小拼接，Go 的 time.Duration.String() 同款顺序
        let units: [(&str, u64); 5] = [
            ("h", 3_600_000_000_000),
            ("m", 60_000_000_000),
            ("s", 1_000_000_000),
            ("ms", 1_000_000),
            ("us", 1_000),
        ];
        let mut rest = u;
        for (suffix, scale) in units {
            if rest >= scale {
                let n = rest / scale;
                rest %= scale;
                out.push_str(&format!("{n}{suffix}"));
            }
        }
        if rest > 0 {
            out.push_str(&format!("{rest}ns"));
        }
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// 把 rclone `SizeSuffix` 的字节数格式化成带单位文本（"10Mi"、"512Mi"、"10B"）
///
/// 必须带单位：`SizeSuffix.Set` 把无单位数字当 **KiB**，所以裸数字
/// `10485760` 会被读成 10 GiB 而不是 10 MiB。
fn fmt_size_bytes(bytes: i64) -> String {
    // 负数是 rclone 的 "off"
    if bytes < 0 {
        return "off".to_string();
    }
    if bytes == 0 {
        return "0".to_string();
    }
    const UNITS: [(&str, i64); 6] = [
        ("Ei", 1 << 60),
        ("Pi", 1 << 50),
        ("Ti", 1 << 40),
        ("Gi", 1 << 30),
        ("Mi", 1 << 20),
        ("Ki", 1 << 10),
    ];
    for (suffix, scale) in UNITS {
        if bytes >= scale {
            let whole = bytes / scale;
            let rem = bytes % scale;
            if rem == 0 {
                return format!("{whole}{suffix}");
            }
            // 非整除时保留三位小数，与 rclone 的 %.3f 对齐
            let val = bytes as f64 / scale as f64;
            let s = format!("{val:.3}");
            let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
            return format!("{s}{suffix}");
        }
    }
    // 小于 1 KiB：裸数字会被当 KiB 放大，必须带 B
    format!("{bytes}B")
}

/// 把字段里的「无单位裸数字」按 rclone `config/providers` 的基础单位换算
///
/// 注意这里刻意**不**沿用 rclone.conf 的文本约定（Duration 无单位=秒、
/// SizeSuffix 无单位=KiB）：字段的初值来自 provider 元数据，那里的 Duration
/// 是纳秒、SizeSuffix 是字节，用户看到的也是 `10ms` / `10Mi` 这种带单位文本。
/// 裸数字按基础单位解释，既与初值一致，也能把历史遗留的 `1e+07` 这类被写坏的
/// 值顺手修回 `10ms`。
fn parse_bare_duration_ns(s: &str) -> Option<i64> {
    // 能直接当浮点数读 = 无单位裸数字（含历史遗留的 `1e+07`）。
    // "10ms" / "1h30m" / "off" 都读不成浮点数，原样交给 rclone。
    let ns: f64 = s.parse().ok()?;
    if !ns.is_finite() {
        return None;
    }
    Some(ns as i64)
}

/// 把字段里的「无单位裸数字」按字节解释，见 [`parse_bare_duration_ns`]
fn parse_bare_size_bytes(s: &str) -> Option<i64> {
    let n: f64 = s.parse().ok()?;
    if !n.is_finite() {
        return None;
    }
    Some(n as i64)
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
        let name = opt
            .get("Name")
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let help = opt
            .get("Help")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        let ty = opt.get("Type").and_then(|t| t.as_str()).unwrap_or("string");
        let required = opt
            .get("Required")
            .and_then(|r| r.as_bool())
            .unwrap_or(false);
        let advanced = opt
            .get("Advanced")
            .and_then(|a| a.as_bool())
            .unwrap_or(false);
        // 是否遮住输入只看 IsPassword。
        //
        // rclone 里两个标记互不蕴含：
        // - IsPassword：这个值是密码，交互式输入时要遮住（fs/registry.go）；
        // - Sensitive：导出 / `rclone config redacted` 时要抹掉。
        // 之前用 `sensitive || ty == "password"` 判断，于是 webdav 的 `user`
        // （Sensitive=true、IsPassword=false）被当成密码遮住，而真正的 `pass`
        // （IsPassword=true、Sensitive=false）落成 "text" 明文显示 —— 掩码反了。
        let is_password = opt
            .get("IsPassword")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        // rclone 的 Default 对 Duration 是**纳秒数**、对 SizeSuffix 是**字节数**，
        // 直接 to_string 会得到 "10000000" / "10485760" 这种无单位裸数字，回写时
        // 被 rclone 按秒 / KiB 解析。这里先格式化成带单位的规范文本。
        let default = match opt.get("Default") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Number(n)) => {
                let raw = n.as_i64().or_else(|| n.as_f64().map(|f| f as i64));
                match (ty, raw) {
                    ("Duration", Some(ns)) => fmt_duration_ns(ns),
                    ("SizeSuffix", Some(bytes)) => fmt_size_bytes(bytes),
                    // Bits（位图）默认是 0，直接 to_string 得到 "0"；用户看到
                    // 一个填 0 的数字框，不知道该填什么。rclone 自己的 Help 和
                    // 配置文件里都用 `off` 表示「一个都不选」（`Use Off to
                    // disable`），且 `Bits.Set` 接受 `off` 并原样落盘，所以默认
                    // 值统一显示成 off。典型例子：s3 的 sdk_log_mode。
                    ("Bits", Some(0)) => "off".to_string(),
                    _ => n.to_string(),
                }
            }
            _ => String::new(),
        };
        // Duration / SizeSuffix 单独成一类，不能混进 "number"。
        //
        // rclone 的 `updateRemote` 对 JSON 值一律 `fmt.Sprint(v)` 落盘，数字会被
        // 打印成 `1e+07` 这种科学计数法；而 `fs.ParseDuration` 对无单位数字按
        // **秒**解析、`SizeSuffix.Set` 对无单位数字按 **KiB** 解析。于是默认值
        // 10000000ns（10ms）会变成 10⁷ 秒（115 天），webdav 的 pacer 第二次调用
        // 就永久阻塞 —— 表现为「打开根目录后断连」。所以这两类必须输出带单位的
        // 规范文本，见 fields_to_params 里的格式化分支。
        let kind = if ty == "bool" {
            "bool"
        } else if is_password {
            "password"
        } else if ty == "Duration" {
            "duration"
        } else if ty == "SizeSuffix" {
            "size"
        } else if ty == "int" || ty == "float" {
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
            // 新建远程时还没有已存密钥；编辑已有远程时由
            // fill_fields_with_existing 按实际情况置位。
            secret_set: false,
            secret_editing: false,
        });
    }
    (desc, fields)
}

/// 把 FieldRow 列表转为 rclone config 参数 JSON
///
/// 密码字段（kind == "password"，即 rclone 的 IsPassword）有一条额外规则：
/// **空值永远不发送**。已存密钥没被重填时发空串，rclone 会把空密码写进配置
/// （实测：`config/update {"pass": ""}` 会存下一个 reveal 为空串的密钥），
/// 这正是「保存后访问失败」的成因之一。跳过空密码 = 保持原值不变。
fn fields_to_params(fields: &[FieldRow]) -> Value {
    let mut map = serde_json::Map::new();
    for f in fields {
        let val = f.value.to_string();
        let val = val.trim();
        // 密码留空表示「不修改」，无论是否必填都不写空串。
        if f.kind.as_str() == "password" && val.is_empty() {
            continue;
        }
        match f.kind.as_str() {
            // 布尔必须显式写出 true/false：只在 true 时插入会导致
            // 用户取消勾选后旧值仍保留在 rclone 配置里，永远无法改回 false。
            "bool" => {
                map.insert(f.name.to_string(), Value::Bool(val == "true"));
            }
            // Duration / SizeSuffix：一律按字符串传，并且补上单位。
            //
            // 这两类绝不能走数字分支：rclone 落盘时 `fmt.Sprint(float64)` 会写出
            // `1e+07` 这样的科学计数法，而回读时无单位数字按秒 / KiB 解析，
            // 数值被放大若干个数量级（webdav 的 pacer_min_sleep 就是这样把
            // 第二次列目录永久卡死的）。裸数字在这里补齐单位再发。
            "duration" => {
                if val.is_empty() {
                    if f.required {
                        map.insert(f.name.to_string(), Value::String(String::new()));
                    }
                } else if let Some(ns) = parse_bare_duration_ns(val) {
                    map.insert(f.name.to_string(), Value::String(fmt_duration_ns(ns)));
                } else {
                    map.insert(f.name.to_string(), Value::String(val.to_string()));
                }
            }
            "size" => {
                if val.is_empty() {
                    if f.required {
                        map.insert(f.name.to_string(), Value::String(String::new()));
                    }
                } else if let Some(bytes) = parse_bare_size_bytes(val) {
                    map.insert(f.name.to_string(), Value::String(fmt_size_bytes(bytes)));
                } else {
                    map.insert(f.name.to_string(), Value::String(val.to_string()));
                }
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
                    // int/float 选项：能解析为数字就用数字（fmt.Sprint 落盘无损）
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

/// 组装真正发给 rclone 的参数字典：先铺插件声明的默认值，再用用户填的覆盖。
///
/// 为什么需要这层：编辑器的字段表是按**内核的 provider 模板**渲染的，插件独有的
/// 键（K-Vault 的 `kvault` / `kvault_storage` / `kvault_dedup` / `vendor`）不在
/// 模板里，只按字段表组装时它们会被静默丢掉——远程建出来了，插件却认不出来。
///
/// 三条规则：
/// - 只有**新建**的派生类型才铺默认值。改已有的远程时，用户删掉的键不该被复活。
/// - `type` 不从默认值里带出来：调用方按选中的类型单独传 `rtype`，两边不打架。
/// - 用户填的值优先；但**用户留空的键不覆盖默认值**，否则「没填」会把插件的默认
///   参数擦成空串。
fn params_for_save(
    choice: &TypeChoice,
    derived: &[remote_types::Derived],
    user: serde_json::Map<String, Value>,
    is_new: bool,
) -> Value {
    let mut out = serde_json::Map::new();
    if is_new && choice.derived {
        if let Some(d) = remote_types::find(derived, &choice.id) {
            for (k, v) in d.initial_params() {
                if k == "type" || v.trim().is_empty() {
                    continue;
                }
                out.insert(k, Value::String(v));
            }
        }
    }
    for (k, v) in user {
        if k == "type" {
            continue;
        }
        // 用户把这个键留空 = 没填。默认值已经在里面了，就别用空串盖掉它。
        if is_blank_param(&v) && out.contains_key(&k) {
            continue;
        }
        out.insert(k, v);
    }
    Value::Object(out)
}

/// 参数空值判断：空串和 null 都算「用户没填这个键」。
fn is_blank_param(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

/// 把派生类型的默认参数补进「每行一个 key=value」的原始文本，已有同名键的不动。
///
/// 返回补好之后的整段文本；没有任何一项可补时返回 `None`（调用方就别写回界面，
/// 免得把用户正在编辑的光标位置搅乱）。
fn seed_derived_defaults(
    choice: &TypeChoice,
    derived: &[remote_types::Derived],
    current: &str,
) -> Option<String> {
    if !choice.derived {
        return None;
    }
    let d = remote_types::find(derived, &choice.id)?;
    let existing = parse_kv_lines(current);
    let missing: Vec<String> = d
        .initial_params()
        .into_iter()
        .filter(|(k, _)| k != "type")
        .filter(|(_, v)| !v.trim().is_empty())
        .filter(|(k, _)| !existing.contains_key(k.as_str()))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(
        format!("{current}\n{}", missing.join("\n"))
            .trim()
            .to_string(),
    )
}

/// 保存时要发给 rclone 的那一次请求。
///
/// 单独拎出来是为了让「新建远程究竟带哪些参数、发到哪个接口」能被测试盯住。
/// 这段逻辑原先埋在 UI 回调里，接错线（比如直接用字段表拼参数）只有真的
/// 保存一次才看得出来，插件默认参数就是这么丢的。
enum SaveRequest {
    Create {
        name: String,
        rtype: String,
        params: Value,
    },
    Update {
        name: String,
        params: Value,
    },
}

/// 按「新建还是改已有的」决定接口，并把插件默认值铺进参数。
///
/// `rtype` 一律取选中项的 `backend`：插件派生类型落盘的是它的 `base_type`，
/// 不从参数里读 `type`，免得两个来源打架。
fn build_save_request(
    name: &str,
    choice: &TypeChoice,
    derived: &[remote_types::Derived],
    user: serde_json::Map<String, Value>,
    is_new: bool,
) -> SaveRequest {
    let params = params_for_save(choice, derived, user, is_new);
    if is_new {
        SaveRequest::Create {
            name: name.to_string(),
            rtype: choice.backend.clone(),
            params,
        }
    } else {
        SaveRequest::Update {
            name: name.to_string(),
            params,
        }
    }
}

/// 用现有远程参数值填充 FieldRow 列表
///
/// 密码字段（IsPassword）特殊处理：rclone 在配置里存的是**混淆后的密文**，
/// 直接塞进可编辑输入框，用户就会看到一串哈希，还以为密码被改坏了。这里
/// 不把密文交给 UI，只把 `secret_set` 置位，让界面显示「已设置」并等用户
/// 主动点「更改密码」再输入新值。
fn fill_fields_with_existing(fields: &mut [FieldRow], existing: &serde_json::Map<String, Value>) {
    for f in fields.iter_mut() {
        if let Some(v) = existing.get(f.name.as_str()) {
            if f.kind.as_str() == "password" {
                // 只要 rclone 里有这个键（哪怕空串），就说明「已设置」，
                // 不在界面上回显密文。
                f.value = "".into();
                f.secret_set = true;
                f.secret_editing = false;
                continue;
            }
            f.value = match v {
                Value::String(s) => s.clone().into(),
                Value::Bool(b) => b.to_string().into(),
                Value::Number(n) => n.to_string().into(),
                _ => v.to_string().into(),
            };
            // Duration / SizeSuffix：把历史遗留的裸数字（含被写坏的 `1e+07`）
            // 就地换成带单位的规范文本，用户打开编辑器就能看到并顺手修好。
            match f.kind.as_str() {
                "duration" => {
                    let cur = f.value.to_string();
                    if let Some(ns) = parse_bare_duration_ns(&cur) {
                        f.value = fmt_duration_ns(ns).into();
                    }
                }
                "size" => {
                    let cur = f.value.to_string();
                    if let Some(bytes) = parse_bare_size_bytes(&cur) {
                        f.value = fmt_size_bytes(bytes).into();
                    }
                }
                _ => {}
            }
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
    if ui
        .upgrade_in_event_loop(move |u| {
            let _ = tx.send(f(u));
        })
        .is_err()
    {
        return None;
    }
    rx.await.ok()
}

/// 从插件列表里就地摘掉一行（乐观更新，必须在 UI 线程内调用）。
///
/// 卸载的磁盘动作成功之后，列表里那张卡片立刻就不该存在了。这里不等重扫，
/// 直接按 id 过滤掉。
fn remove_plugin_row(ui: &MainWindow, id: &str) {
    let g = ui.global::<PluginsG>();
    let model = g.get_rows();
    let kept: Vec<PluginRow> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    let kept = drop_row_by_id(kept, id);
    g.set_rows(ModelRc::from(Rc::new(VecModel::from(kept))));
}

/// 按 id 摘掉一行。抽成纯函数以便单测：卸载后列表不刷新是「静默」缺陷，
/// 一旦回归不会报错，只会让用户看到已经卸载掉的卡片。
pub(crate) fn drop_row_by_id(rows: Vec<PluginRow>, id: &str) -> Vec<PluginRow> {
    rows.into_iter().filter(|r| r.id.as_str() != id).collect()
}

/// 在顶部「已卸载」提示里补一条（幂等，必须在 UI 线程内调用）。
fn add_removed_builtin(ui: &MainWindow, id: &str, name: &str, version: &str) {
    let g = ui.global::<PluginsG>();
    let model = g.get_removed_builtins();
    let mut out: Vec<crate::RemovedRow> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    if out.iter().any(|r| r.id.as_str() == id) {
        return;
    }
    out.push(crate::RemovedRow {
        id: id.into(),
        name: name.into(),
        version: version.into(),
    });
    g.set_removed_builtins(ModelRc::from(Rc::new(VecModel::from(out))));
}

/// 恢复内置后，把顶部那条「已卸载」提示去掉（必须在 UI 线程内调用）。
fn remove_removed_builtin(ui: &MainWindow, id: &str) {
    let g = ui.global::<PluginsG>();
    let model = g.get_removed_builtins();
    let out: Vec<crate::RemovedRow> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .filter(|r| r.id.as_str() != id)
        .collect();
    g.set_removed_builtins(ModelRc::from(Rc::new(VecModel::from(out))));
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
        Some(i) => (
            fs[..i].to_string(),
            fs[i + 1..].trim_matches('/').to_string(),
        ),
        None => (fs.trim_end_matches(':').to_string(), String::new()),
    }
}

/// rclone fs（"name:sub"）→ 显示路径（"root://name/sub"）
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
    if b.len() == 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') && b[0].is_ascii_alphabetic()
    {
        return format!("{}:\\", b[0].to_ascii_uppercase() as char);
    }
    t.to_string()
}

/// 列出显示路径的内容；dirs_only=true 只保留文件夹
/// 根路径 "root://" 列出所有仓库名
///
/// 第二个返回值是给界面显示的路径，`Err` 是失败原因。分成两支是因为「这个目录是
/// 空的」和「连不上这个远程」在界面上长得一模一样，而用户该做的事完全不同：前者
/// 换个目录，后者先查网络或凭据。挑目录的弹窗要靠这个区别决定开不开。
async fn list_display_checked(
    ctx: &Arc<Ctx>,
    display: &str,
    dirs_only: bool,
) -> (Result<Vec<(String, bool)>, String>, String) {
    let d = display.trim();
    let rest = d
        .trim_end_matches('/')
        .strip_prefix("root://")
        .unwrap_or("");
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
            // 缓存空就现问一次；问不到说明内核没应答，不能当成「一个远程都没配」
            match client.list_remotes().await {
                Ok(v) => v,
                Err(e) => {
                    return (
                        Err(rcgui_i18n::f("读不到远程列表：{}", &[&format!("{:#}", e)])),
                        "root://".into(),
                    )
                }
            }
        } else {
            names
        };
        return (
            Ok(names.into_iter().map(|n| (n, true)).collect()),
            "root://".into(),
        );
    }
    let Some((fs, remote)) = split_display(d) else {
        return (
            Err(rcgui_i18n::f("看不懂这个路径：{}", &[&display])),
            "root://".into(),
        );
    };
    let client = ctx.rc.client().await;
    let r = if remote.is_empty() {
        "".to_string()
    } else {
        remote.clone()
    };
    // 挑目录的弹窗也走这里。插件不在时被标记的远程不能借它翻目录树，
    // 更不能被选成同步目标：选上了同步只会一路 403。
    let name = fs.trim_end_matches(':');
    if let Some(msg) = kv_access_blocked(ctx, name).await {
        return (Err(msg), format!("root://{}", rest));
    }
    match client.list(&fs, &r, None).await {
        Ok(entries) => {
            let items: Vec<(String, bool)> = entries
                .into_iter()
                .filter(|e| !dirs_only || e.is_dir)
                .map(|e| (e.name, e.is_dir))
                .collect();
            (Ok(items), format!("root://{}", rest))
        }
        Err(e) => (
            Err(rcgui_i18n::f("列不出来：{}", &[&format!("{:#}", e)])),
            format!("root://{}", rest),
        ),
    }
}

/// 云端目录弹窗点了「确定」：把选中的显示路径落成配置里的 `远程名:子路径`。
///
/// 写之前真去列一次目录：列表能拿到才算这个位置现在可用。列不到就一个字都不改，
/// 并把原因写到同步卡片上——用户手填的旧值比一个打不开的新值有用。
async fn browse_ok_sync(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, final_disp: &str) -> bool {
    let Some((fs, sub)) = split_display(final_disp) else {
        // 停在根上点了确定：没选远程，什么都不动
        maintenance::set_sync_status(ui, rcgui_i18n::t("没选远程，云端目录没有改动"));
        let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<BrowseG>().set_show(false));
        return false;
    };
    let value = compose_fs(&fs.trim_end_matches(':'), &sub);
    let (listed, _) = list_display_checked(ctx, final_disp, true).await;
    match listed {
        Ok(_) => {
            let shown = value.clone();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                ui.global::<MaintenanceG>().set_sync_remote(shown.into());
                ui.global::<BrowseG>().set_show(false);
            });
            maintenance::set_sync_status(ui, rcgui_i18n::f("已选择云端目录：{}", &[&value]));
        }
        Err(why) => {
            let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<BrowseG>().set_show(false));
            maintenance::set_sync_status(
                ui,
                rcgui_i18n::f("这个位置现在打不开，云端目录保持原值：{}", &[&why]),
            );
        }
    }
    false
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

/// 获取 providers（优先内存缓存 → 网络 → 磁盘缓存）。
///
/// **失败不能静默成空**：`config/providers` 拿不到时，类型下拉会是空的，
/// 用户看到「新建远程」里一个类型都选不了，却完全不知道原因（实测反馈）。
/// 因此这里：
/// 1. 把失败原因记进 `providers_err`，供 UI 提示；
/// 2. 回落到上次成功时的磁盘缓存，离线也能继续建远程。
async fn get_providers(ctx: &Arc<Ctx>) -> Value {
    {
        let cache = ctx.providers_cache.lock().unwrap();
        if let Some(v) = cache.as_ref() {
            return v.clone();
        }
    }
    let client = ctx.rc.client().await;
    match client.config_providers().await {
        Ok(v) => {
            *ctx.providers_cache.lock().unwrap() = Some(v.clone());
            *ctx.providers_err.lock().unwrap() = None;
            save_providers_cache(&v);
            v
        }
        Err(e) => {
            let msg = e.to_string();
            *ctx.providers_err.lock().unwrap() = Some(msg);
            match load_providers_cache() {
                Some(v) => {
                    *ctx.providers_cache.lock().unwrap() = Some(v.clone());
                    v
                }
                None => json!({}),
            }
        }
    }
}

/// 上次成功拿到的 `config/providers` 落盘位置。
fn providers_cache_path() -> std::path::PathBuf {
    config::data_dir().join("providers_cache.json")
}

/// 把 provider 列表存盘（仅用于离线兜底；失败不影响主流程）。
fn save_providers_cache(v: &Value) {
    if v.get("providers")
        .and_then(|p| p.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true)
    {
        return; // 空结果不值得覆盖掉一份好的缓存
    }
    let _ = std::fs::write(providers_cache_path(), v.to_string());
}

fn load_providers_cache() -> Option<Value> {
    let text = std::fs::read_to_string(providers_cache_path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    if v.get("providers")
        .and_then(|p| p.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true)
    {
        return None;
    }
    Some(v)
}

// ---------------- 新建远程的类型下拉 ----------------
//
// 下拉框里只有一列字符串（`RemotesG.types`），但背后是两类东西：rclone 的
// 真实 backend 名，和插件声明的派生类型（例如 kvault 基于 webdav）。派生类型
// 写进配置时必须是 `base_type`，否则 rclone 会当成未知后端拒收；而界面要显示
// 插件给的名字（可能带中文）。所以这里维护一张平行表：`types` 只放显示名，
// 真实 id 和落盘用的 backend 都只在这张表里，绝不用显示名当 id。

/// 类型下拉里的一项。
#[derive(Clone, Debug)]
struct TypeChoice {
    /// 界面上显示的名字；插件可以给中文名
    display: String,
    /// 派生类型在插件清单里的 id；普通类型就是 backend 名
    id: String,
    /// 落进 rclone 配置的 `type` 字段。普通类型等于 `id`，派生类型等于 base_type
    backend: String,
    /// 是插件声明的派生类型时为真，决定要不要预铺插件给的 defaults
    derived: bool,
}

impl TypeChoice {
    /// 关键字是否命中这一项。大小写不敏感地比对显示名、id、backend：
    /// 用户可能记得插件名、也可能记得 rclone 的后端名。
    ///
    /// 两头都先去掉空格、连字符、下划线：显示名是「K-Vault」时，用户按习惯敲
    /// `kvault` 也得能搜到——这条搜索框存在的意义就是省得去猜连字符。
    fn matches(&self, query_lower: &str) -> bool {
        squeeze(&self.display.to_lowercase()).contains(query_lower)
            || squeeze(&self.id.to_lowercase()).contains(query_lower)
            || squeeze(&self.backend.to_lowercase()).contains(query_lower)
    }
}

/// 一次过滤之后，界面该怎么摆。
///
/// `idx` 是 `RemotesG.type-idx` 该写成的值；`None` 表示过滤后一项都没有，界面用
/// `-1` 表示「没有选中项」，此时保存会被拒。
///
/// `switched` 为真表示用户原来选中的那个类型被搜索词筛掉了，`idx` 指向的是另一个
/// 类型。调用方必须把参数表一起换成新类型的，否则选中的是 A、填的是 B 的字段，
/// 一保存就把参数写到别的后端上。
/// 把搜索词和候选名字压成同一种形状：去掉空格、连字符、下划线。
///
/// 「K-Vault」「k_vault」「kvault」应当是同一次搜索；只压低大小写不够，
/// 用户不会替我们记住连字符长在哪里。
fn squeeze(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .collect()
}

/// 一次过滤的结果：`idx` 是界面该写的 `type-idx`，`switched` 表示选中项被换掉了。
struct FilterResult {
    idx: Option<usize>,
    switched: bool,
}

/// 类型下拉的当前状态。
#[derive(Default)]
struct TypeChoices {
    /// 全部可选类型。顺序在搜索时不重排，这样清空搜索词能回到原样
    all: Vec<TypeChoice>,
    /// 按当前搜索词过滤后的那份；`RemotesG.type-idx` 指的是这一份的下标
    visible: Vec<TypeChoice>,
}

impl TypeChoices {
    /// 用一份新清单替换，返回 `select` 指定的那一项的下标（找不到或为空为 0）。
    fn reset(&mut self, all: Vec<TypeChoice>, select: Option<&str>) -> usize {
        self.all = all;
        self.visible = self.all.clone();
        select
            .and_then(|id| self.visible.iter().position(|c| c.id == id))
            .unwrap_or(0)
    }

    /// 按关键字重新过滤。
    ///
    /// 返回的这一份同时告诉调用方「选中项还成立吗」：搜索把用户已选中的类型筛掉时，
    /// 界面上的下标会落到别的类型上，下面那张参数表必须跟着换——否则选中的是新类型、
    /// 填的是旧类型的字段，一保存就写错后端（搜 `kvault` 只剩 K-Vault 却仍显示 Alias
    /// 表单就是这个）。
    ///
    /// 选中项仍在时一律不动它：用户先选好类型、再回头改搜索词，不该被悄悄换成第一项，
    /// 已经填了一半的参数也不能重铺。
    fn refilter(&mut self, query: &str, keep_id: Option<&str>) -> FilterResult {
        let q = squeeze(query.trim().to_lowercase().as_str());
        self.visible = if q.is_empty() {
            self.all.clone()
        } else {
            self.all.iter().filter(|c| c.matches(&q)).cloned().collect()
        };
        if self.visible.is_empty() {
            // 一条都不剩：没有「默认第一项」可退，界面下标置 -1，保存时会被拒。
            return FilterResult {
                idx: None,
                switched: false,
            };
        }
        match keep_id.and_then(|id| self.visible.iter().position(|c| c.id == id)) {
            Some(i) => FilterResult {
                idx: Some(i),
                switched: false,
            },
            // 没有旧选中项（刚打开编辑器）时下标 0 就是表单已经对应的那一项，不算切换。
            None => FilterResult {
                idx: Some(0),
                switched: keep_id.is_some(),
            },
        }
    }

    /// 取 `visible[idx]` 的副本。越界或表空返回 None，调用方必须处理。
    fn visible_at(&self, idx: usize) -> Option<TypeChoice> {
        self.visible.get(idx).cloned()
    }

    /// 当前选中项，按 `type-idx` 取。
    ///
    /// `-1` 表示「当前搜索词下一项都没有」：此时没有合法类型，返回 None 让保存被拒，
    /// 绝不能退回第 0 项——那会把另一套参数写到别的后端上。
    fn selected(&self, idx: i32) -> Option<TypeChoice> {
        if idx < 0 {
            return None;
        }
        self.visible_at(idx as usize)
    }
}

/// 组装一张完整的类型清单：内核报的后端 + 启用插件声明的派生类型。
///
/// 清单里只放真实存在的东西：`providers` 为空（内核没连上、也没有上次成功的缓存）
/// 就一项都不列，界面据此显示「连接内核后再选类型」。以前这里塞过一份内置类型表
/// 和内置 `k-vault`，结果是界面上同时出现 `K-Vault` 与 `k-vault` 两项、还能选中
/// 一个内核根本不认识的后端——用户明确要求类型只来自 rclone。
fn build_type_choices(providers: &Value, derived: &[remote_types::Derived]) -> Vec<TypeChoice> {
    let mut out: Vec<TypeChoice> = Vec::new();
    let names: Vec<String> = providers
        .get("providers")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("Name").and_then(|n| n.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    for n in names {
        out.push(TypeChoice {
            display: n.clone(),
            id: n.clone(),
            backend: n,
            derived: false,
        });
    }
    for d in derived {
        // 插件给的 id 若与已有后端同名，说明它想覆盖那个后端：保留插件项，
        // 因为插件项带着 defaults，用户点了能拿到可用的初始参数。
        out.retain(|c| c.id != d.id);
        out.push(TypeChoice {
            display: d.name.clone(),
            id: d.id.clone(),
            backend: d.base_type.clone(),
            derived: true,
        });
    }
    // 按显示名排序：中英文混排时 ASCII 在前，顺序稳定即可，不比拼音。
    out.sort_by(|a, b| a.display.to_lowercase().cmp(&b.display.to_lowercase()));
    out
}

/// 编辑一个**已经存在**的远程时，下拉里唯一那一项。
///
/// 分两种情况，都与「新建」共用同一套类型表：
/// - `ty` 是某个已启用插件声明的 id：用插件的显示名与 base_type。
/// - 其余（内核自带后端）：原名原样往返。
///
/// 这里不再为 K-Vault 合成专属项：那个类型现在由插件声明，插件没装它就不存在，
/// 硬合成一项等于在界面上承诺一个没人负责的类型。
fn edit_single_choice(ty: &str, derived: &[remote_types::Derived]) -> TypeChoice {
    match remote_types::find(derived, ty) {
        Some(d) => TypeChoice {
            display: d.name.clone(),
            id: d.id.clone(),
            backend: d.base_type.clone(),
            derived: true,
        },
        None => TypeChoice {
            display: ty.to_string(),
            id: ty.to_string(),
            backend: ty.to_string(),
            derived: false,
        },
    }
}

/// 一个既有远程在界面上该显示成哪个类型。
///
/// 存进 rclone 的 `type` 永远只是后端，所以「这是 K-Vault 而不是普通 WebDAV」这条
/// 信息只能来自参数里的标记键。而标记键是不是还真有实现，取决于**当前有没有插件
/// 声明那个类型 id**：插件停用或卸载后，同一个远程就该显示成它真实的样子（webdav），
/// 让用户看见「写入功能不在」而不是继续看到那个名字。
///
/// 返回类型 id：命中插件时是插件的 id，否则是 `stored` 原样。
///
/// 收整条条目（`Value::Object`，来自 `rclone config dump`）而不是拆出来的参数表：
/// 调用方手里本来就有一条条目，标记键在两种形状里的读法一样。
fn display_type_of(entry: &Value, stored: &str, derived: &[remote_types::Derived]) -> String {
    if !kvault::KvCfg::is_marked_entry(entry) {
        return stored.to_string();
    }
    match remote_types::marker_owner(derived, kvault::MARKER) {
        Some(d) => d.id.clone(),
        None => stored.to_string(),
    }
}

/// 把一张清单推给界面：`types` 放显示名，`type-idx` 指向选中的下标。
fn push_type_choices(ui: &MainWindow, choices: &[TypeChoice], idx: usize) {
    push_type_selection(ui, choices, Some(idx));
}

/// 同上，但允许「没有选中项」。
///
/// 过滤后一项都不剩时必须传 `None`：界面写成 `-1`，保存路径拿到 `None` 就会拒绝，
/// 不会拿另一套参数去建别的后端。
fn push_type_selection(ui: &MainWindow, choices: &[TypeChoice], idx: Option<usize>) {
    let g = ui.global::<RemotesG>();
    let names: Vec<slint::SharedString> = choices
        .iter()
        .map(|c| slint::SharedString::from(c.display.as_str()))
        .collect();
    g.set_types(ModelRc::from(Rc::new(VecModel::from(names))));
    g.set_type_idx(match idx {
        Some(i) => i as i32,
        None => -1,
    });
}

/// 这个类型该用哪张参数表：一律按 rclone 的后端名取。
///
/// 派生类型落盘的就是 `base_type`（例如 `kvault` 存成 `webdav`），所以它的参数表
/// 与后端自己的那张是同一张；插件独有的键由 `seed_derived_defaults` 铺进原始参数框。
fn form_fields_type(choice: &TypeChoice) -> String {
    choice.backend.clone()
}

/// 派生类型的初始参数，写成「原始参数」框里的那种一行一个 `k=v`。
///
/// `type` 一律不写：真正落盘的类型是 `base_type`（例如 kvault 存成 webdav），
/// 由保存路径按 parallel 表决定，用户在这里写反而会被覆盖或写错。
fn derived_params_text(d: &remote_types::Derived) -> String {
    d.initial_params()
        .into_iter()
        .filter(|(k, _)| k != "type")
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
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

// ---------------- K-Vault：只剩远程操作桥 ----------------
//
// K-Vault 这个**类型**由 kvault 插件声明（`remote_types`），编辑表单也在插件面板里。
// 主程序这边只剩一条操作桥：把文件字节搬到站点 API、列站点文件、删站点文件。
// 需要它是因为站点的 DAV 端点只放行 OPTIONS/PROPFIND/GET/HEAD，写方法一律 403，
// 而插件的 `http.request` 只能发字符串，搬不动文件。
//
// 桥认的参数形状没变：rclone 里就是一个 `webdav` 远程（`url=<站点>/dav`），
// 靠参数里的 `kvault=1` 标记认出来。插件没装/没启用时没人声明这个类型，
// 这个远程在本程序里就用不了（[`kv_access_blocked`] 会拒掉浏览、下载、挂载和
// 改配置，[`kv_write_kind`] 拒掉写），而不是降级成普通 webdav 远程。

/// 读一个远程的参数，判断它是不是 K-Vault 远程；是则返回配置（含明文令牌）。
///
/// 用 `config/get` 而不是 `config/dump`：只读这一个远程，不去拉全量凭据。
/// 不缓存：用户可能刚在原始编辑器里改过参数，缓存会让上传用上过期配置。
///
/// 拆成两步是因为插件宿主（`caps.rs`）手里只有 `RcClient` + `KvStore`，没有 `Ctx`：
/// 它执行插件的 `rclone.upload_file` 时也要判断目标是不是 K-Vault 远程。
pub async fn kv_cfg_lookup(
    client: &rclone_client::RcClient,
    store: &crate::kv::KvStore,
    remote: &str,
) -> Option<kvault::KvCfg> {
    let remote = remote.trim();
    if remote.is_empty() {
        return None;
    }
    let params = client.config_get(remote).await.ok()?;
    if !kvault::KvCfg::is_marked(&params) {
        return None;
    }
    let mut cfg = kvault::KvCfg::from_params(&params);
    cfg.token = kv_token_resolve(client, store, remote, &params).await;
    Some(cfg)
}

/// 插件的 id。它和 [`kvault::STORE_NS`] 现在同一个字符串，但含义不同：
/// 这里是「插件目录名 / KV 命名空间」，那边是「旧版主程序存令牌的键空间」。
/// 分开写是为了以后改名时不会顺手改错另一边。
const KVAULT_PLUGIN_ID: &str = "kvault";

/// 找出一枚能直接发 HTTP 的明文令牌。
///
/// 历史原因同一枚令牌有三处来源，优先级从旧到新：
/// 1. 主程序按远程名记下的明文（见 [`kv_token_in`]）。旧版把令牌写在
///    `kvault::STORE_NS` 命名空间里，那个命名空间与插件目录同名，卸载插件会连令牌
///    一起删掉，所以现在写进 `kvault::TOKEN_NS`，读的时候两个都看；
/// 2. rclone 远程参数里的 `bearer_token`——kvault 插件新建远程时写这里；
/// 3. kvault 插件自己存的 `site.token`（整个站点一枚，不区分远程）。
///
/// 三处都空才返回空串；调用方据此判定「没令牌，只能浏览」，不要在这里猜一个。
async fn kv_token_resolve(
    client: &rclone_client::RcClient,
    store: &crate::kv::KvStore,
    remote: &str,
    params: &serde_json::Map<String, Value>,
) -> String {
    let legacy = kv_token_in(store, remote);
    if !legacy.is_empty() {
        return legacy;
    }
    if let Some(raw) = params.get(kvault::TOKEN_KEY).and_then(|v| v.as_str()) {
        let token = kv_reveal(client, raw).await;
        if !token.is_empty() {
            return token;
        }
    }
    store
        .get_all(KVAULT_PLUGIN_ID)
        .get("site")
        .and_then(|v| v.get("token"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 把 rclone 存下的 `bearer_token` 还原成明文。
///
/// rclone 对 webdav 的 `bearer_token` 做 obscure 后存储——那是可逆编码，不是加密，
/// 但直接拿去发请求会被站点拒绝。手工写进配置文件的明文令牌不是合法 obscure 串，
/// `config/reveal` 会失败，这时原文就是明文，原样使用。
///
/// 失败一律当明文：宁可发出一个错的令牌让用户看见 401，也不要静默变成「没有凭据」。
async fn kv_reveal(client: &rclone_client::RcClient, raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    match client.call("config/reveal", &json!({ "value": raw })).await {
        Ok(v) => {
            let plain = v.get("value").and_then(|v| v.as_str()).unwrap_or("").trim();
            if plain.is_empty() {
                raw.to_string()
            } else {
                plain.to_string()
            }
        }
        Err(_) => raw.to_string(),
    }
}

/// 这个远程现在归不归主程序的 K-Vault 操作桥管。
///
/// 判据是「有没有启用中的插件声明了这个标记键」：插件把**自己的类型 id** 当作远程
/// 参数里的标记键（`config.json` 的 `remote_types[].defaults` 里写 `kvault: "1"`），
/// 所以没人声明就等于这个类型现在没有实现，桥必须下线 —— 否则用户卸了插件，
/// 主程序还在替他往站点传文件。
///
/// 注意它**不等于**「插件停用后这个远程退化成普通 webdav 远程」：远程还带着标记，
/// 说明它本来就不是普通 WebDAV，停用后它在应用里用不了，见 [`kv_access_blocked`]。
fn kv_bridge_owns(ctx: &Arc<Ctx>) -> bool {
    remote_types::marker_owner(&remote_types::collect(ctx), kvault::MARKER).is_some()
}

async fn kv_cfg_for(ctx: &Arc<Ctx>, remote: &str) -> Option<kvault::KvCfg> {
    if !kv_bridge_owns(ctx) {
        return None;
    }
    let client = ctx.rc.client().await;
    kv_cfg_lookup(&client, &ctx.store, remote).await
}

/// 一条远程在「写」这件事上归谁管。
///
/// 三种情况必须分开，不能像原来那样只用 `Option<cfg>` 表示：
/// - `NotKv`：与 K-Vault 无关，照旧交给 rclone（普通 WebDAV 永远落在这里）；
/// - `Bridge`：带标记且插件在，走站点 API；
/// - `Orphan`：带标记但插件被停用/卸载了。此时**不能**悄悄回落到 rclone：
///   站点 DAV 的写方法一律 403，用户只会看到「method not allowed」，既不知道
///   发生了什么，也不知道装回插件就能解决。要显式拒绝并说明。
enum KvWrite {
    NotKv,
    Bridge(kvault::KvCfg),
    Orphan(String),
}

/// 判一条远程的写路径。插件不在时的结论与 [`kv_access_blocked`] 一致（拒绝），
/// 那里管的是读、挂载和配置，两边共用 [`kv_write_kind`] 这一处判定。
async fn kv_write_target(ctx: &Arc<Ctx>, remote: &str) -> KvWrite {
    let client = ctx.rc.client().await;
    let cfg = kv_cfg_lookup(&client, &ctx.store, remote).await;
    kv_write_kind(cfg, kv_bridge_owns(ctx), remote)
}

/// [`kv_write_target`] 的判定部分，单独拎出来是为了能测：
/// 真正那一步要去问 rclone，而三态的分岔点在拿到配置之后。
///
/// 先看标记：没有 `kvault` 键的远程是普通远程，插件装没装都与它无关。
fn kv_write_kind(cfg: Option<kvault::KvCfg>, bridge: bool, remote: &str) -> KvWrite {
    match cfg {
        None => KvWrite::NotKv,
        Some(cfg) if bridge => KvWrite::Bridge(cfg),
        Some(_) => KvWrite::Orphan(caps::kv_plugin_gone(remote)),
    }
}

/// 插件不在时，这条远程还能不能被用：浏览、下载、挂载、改配置都先问这里。
///
/// 判据和 [`kv_write_kind`] 是同一道闸门（带标记 + 插件不在 = 拒绝），覆盖面更大：
/// 一个「类型没有实现」的远程在本程序里就不该有能用的入口。浏览尤其不能放过 ——
/// 只读也能列能看，用户自然会以为插件可有可无，直到上传时才撞上另一句报错。
///
/// 返回 `Some(原因)` 就是拒绝，原因可以直接给用户看；`None` 表示放行：普通远程
/// （没有标记键，插件装没装都与它无关），或者插件在。
async fn kv_access_blocked(ctx: &Arc<Ctx>, remote: &str) -> Option<String> {
    // 这里必须用不带闸的 `kv_cfg_lookup`：先拿到「是不是 K-Vault」，再判插件。
    // 反过来的话插件不在时只能得到 `None`，看起来就像普通远程，闸门形同虚设。
    let client = ctx.rc.client().await;
    let cfg = kv_cfg_lookup(&client, &ctx.store, remote).await;
    match kv_write_kind(cfg, kv_bridge_owns(ctx), remote) {
        KvWrite::Orphan(msg) => Some(msg),
        _ => None,
    }
}

/// 给 [`reconcile::reconcile`] 用的「哪些远程现在不能用」表。
///
/// 对账模块只拿得到 `RcClient`，判断不了插件启没启用，所以结论在这里先算好传进去。
/// 只看 `cfg.auto_mounts` 和 `cfg.auto_serves` 里出现过的远程：那正是对账会去
/// 重建挂载和服务的范围，别的远程不用判。
async fn kv_blocked_map(
    ctx: &Arc<Ctx>,
    cfg: &config::AppConfig,
) -> std::collections::HashMap<String, String> {
    let mut names: Vec<String> = Vec::new();
    for m in &cfg.auto_mounts {
        names.push(m.remote.clone());
    }
    for s in &cfg.auto_serves {
        names.push(s.remote.clone());
    }
    names.sort();
    names.dedup();

    let mut out = std::collections::HashMap::new();
    for name in names {
        if let Some(msg) = kv_access_blocked(ctx, &name).await {
            out.insert(name, msg);
        }
    }
    out
}

/// 明文令牌（rclone 里那份是 obscured 的，API 用不了）。
///
/// 和上传/删除走同一套回落顺序（[`kv_token_resolve`]）：否则「界面说有令牌、
/// 上传时却说没有凭据」，用户只能看到一句莫名其妙的 401。
///
/// 先看 [`kvault::TOKEN_NS`]，再退回 [`kvault::STORE_NS`]：后者与插件目录同名，
/// 卸载插件时整个 `kv.json` 会被删掉，旧版写在那里的令牌也就跟着没了。启动时的
/// [`kvault::migrate_kvault_tokens`] 把旧记录复制到新命名空间，这里两个都查是为了
/// 迁移还没跑过（或迁移失败）时用户至少还能用一次。
fn kv_token_in(store: &crate::kv::KvStore, remote: &str) -> String {
    let read = |ns: &str| {
        store
            .get_all(ns)
            .get(remote)
            .and_then(|v| v.get("token"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let fresh = read(kvault::TOKEN_NS);
    if fresh.is_empty() {
        read(kvault::STORE_NS)
    } else {
        fresh
    }
}

/// 本次上传落到站点哪个目录。
///
/// 文件页当前路径就是用户看到的目录：非空时按它传（用户明确选了一个子目录），
/// 否则用远程保存的上传目录（通常等于令牌的目录前缀）。
///
/// 站点按令牌策略要求目录落在前缀下，传到前缀之外一律 `POLICY_FOLDER_DENIED`；
/// 所以当前路径不在上传目录里时退回上传目录，而不是让 rclone/站点回一个看不懂的错。
/// 站点目录：用户在界面里选的路径优先，留空才用配置里的上传目录。
///
/// 不把「不在策略前缀下」的路径偷偷换成上传目录——那会让文件跑到用户没有指定的地方。
/// 站点自己会用 `POLICY_FOLDER_DENIED` 明确拒绝，错误信息里也写了允许的前缀。
fn kv_upload_folder(cfg: &kvault::KvCfg, gui_path: &str) -> String {
    let p = gui_path.trim().trim_matches('/');
    if p.is_empty() {
        return cfg.folder.trim().trim_matches('/').to_string();
    }
    p.to_string()
}

/// 一次 K-Vault 上传的汇总（给 toast 用）。
#[derive(Default)]
struct KvUpload {
    total: usize,
    ok: usize,
    failed: Vec<String>,
    /// 最后一个成功的直链；只传了一个文件时复制到剪贴板
    link: String,
    /// 与 `link` 同一次上传的文件名（提示语里指名道姓，免得用户猜是哪个）
    name: String,
    /// 是否只有一个文件（决定要不要把直链丢进剪贴板）
    single: bool,
    /// 站点判定「同一份内容已存在」直接复用的个数（开了去重才会出现）
    dedup: usize,
}

/// 递归列出目录下的文件：`(绝对路径, 相对目录的路径)`。
///
/// 跳过符号链接（避免绕圈），并对深度/数量设上限：这里只是给上传排个队，
/// 真正的错误留给逐文件上传去报。
fn kv_walk(dir: &Path) -> Vec<(PathBuf, String)> {
    fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(PathBuf, String)>) {
        if depth > 32 || out.len() > 20000 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = e.path();
            if ft.is_dir() {
                walk(root, &path, depth + 1, out);
            } else if ft.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push((path.clone(), rel.to_string_lossy().to_string()));
                }
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, dir, 0, &mut v);
    v
}

/// 把本机路径（文件或目录）上传到站点目录 `folder`。
///
/// **不做同名改名**：站点里文件名不是身份（记录靠 `file.id`），同名不会被覆盖，
/// 而 `deduplicate` 正是按内容去重 —— 客户端抢先改名只会让去重失效。
async fn kv_upload_paths(cfg: &kvault::KvCfg, folder: &str, paths: &[PathBuf]) -> KvUpload {
    // 先排好队：目录展开成文件，这样 total 是「文件数」而不是「拖进来的项数」。
    let mut jobs: Vec<(PathBuf, String, String)> = Vec::new();
    let mut out = KvUpload::default();
    for p in paths {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if p.is_dir() {
            let files = kv_walk(p);
            if files.is_empty() {
                out.failed
                    .push(rcgui_i18n::f("{}: 目录里没有文件", &[&(name)]));
                continue;
            }
            let base_dir = join_path(folder, &name);
            for (abs, rel) in files {
                let rel = rel.replace('\\', "/");
                let (sub, fname) = match rel.rsplit_once('/') {
                    Some((d, n)) => (d.to_string(), n.to_string()),
                    None => (String::new(), rel.clone()),
                };
                let dst = if sub.is_empty() {
                    base_dir.clone()
                } else {
                    join_path(&base_dir, &sub)
                };
                jobs.push((abs, dst, fname));
            }
        } else {
            jobs.push((p.clone(), folder.to_string(), name));
        }
    }
    out.total = jobs.len();
    out.single = jobs.len() == 1;
    for (abs, dst, name) in jobs {
        match kvault::upload(cfg, &abs, &name, &dst).await {
            Ok(u) => {
                out.ok += 1;
                if u.dedup {
                    out.dedup += 1;
                }
                if !u.link.is_empty() {
                    out.name = u.name;
                    out.link = u.link;
                }
            }
            Err(e) => out.failed.push(format!("{name}: {e}")),
        }
    }
    out
}

/// 上传结果 → 一句 toast（成功 1 / 失败 2）。
fn kv_upload_report(up: &KvUpload, where_: &str) -> (i32, String) {
    let place = if where_.is_empty() {
        rcgui_i18n::t("站点根目录")
    } else {
        where_
    };
    if up.failed.is_empty() {
        let mut msg = rcgui_i18n::f("已上传 {} 个文件到 {}", &[&(up.ok), &(place)]);
        if up.dedup > 0 {
            msg.push_str(&rcgui_i18n::f(
                "；其中 {} 个站点已有同一份内容，直接复用了直链",
                &[&(up.dedup)],
            ));
        }
        return (1, msg);
    }
    // 失败项只列前 3 条：toast 是一行字，全塞进去就看不清了。
    let shown: Vec<String> = up.failed.iter().take(3).cloned().collect();
    let mut msg = rcgui_i18n::f(
        "上传完成 {}/{}，失败 {}：{}",
        &[
            &(up.ok),
            &(up.total),
            &(up.failed.len()),
            &(shown.join("；")),
        ],
    );
    if up.failed.len() > shown.len() {
        msg.push_str(&rcgui_i18n::f(
            "（另有 {} 条未列出）",
            &[&(up.failed.len() - shown.len())],
        ));
    }
    (2, msg)
}

/// 改名/移动/新建目录在 K-Vault 远程上没得做：站点的 WebDAV 端点只读，HTTP API
/// 也只开放上传、列表、删除和文本粘贴，没有移动或建目录的端点。这里给一句明确的
/// 原因，而不是让 rclone 回一个干巴巴的 403。
async fn kv_write_blocked(ctx: &Arc<Ctx>, remote: &str) -> Option<String> {
    match kv_write_target(ctx, remote).await {
        KvWrite::NotKv => None,
        KvWrite::Bridge(_) => Some(
            rcgui_i18n::t(
                "K-Vault 远程不支持改名/移动/新建目录：站点只读（WebDAV 写方法一律 403），\
API 也只开放上传、列表、删除。要在站点上调整目录，请到站点网页端操作。",
            )
            .to_string(),
        ),
        KvWrite::Orphan(msg) => Some(msg),
    }
}

/// K-Vault 远程的列目录：直接读站点索引（`GET /api/v1/files`）。
///
/// 为什么不用 rclone 的 DAV：站点的目录树有一层 5 分钟 KV 缓存（`dav:tree:v2`，
/// 上传和删除都不会主动失效），刚上传的文件那边要等好几分钟才出现；而站点索引
/// 是直接读 KV 的，立刻可见。顺带也绕开了 Cloudflare 上偶发的 HTTP/2 建连抖动。
///
/// 返回 `None` 表示「这条远程不归我管」（不是 k-vault，或者没填令牌只能走 DAV）。
async fn kv_list_rows(
    ctx: &Arc<Ctx>,
    remote: &str,
    path: &str,
    sort_key: &str,
    desc: bool,
) -> Option<Result<Vec<FileRow>, String>> {
    let cfg = kv_cfg_for(ctx, remote).await?;
    if cfg.token.trim().is_empty() {
        return None;
    }
    match kvault::list_files(&cfg.base, &cfg.token).await {
        Ok(files) => Some(Ok(kvault::list_folder(&files, path, sort_key, desc)
            .into_iter()
            .map(|i| FileRow {
                name: i.name.into(),
                is_dir: i.is_dir,
                size: fmt_bytes(i.size as f64).into(),
                mtime: fmt_mtime(&i.uploaded).into(),
            })
            .collect())),
        Err(e) => Some(Err(e)),
    }
}

/// 删除成功后本地摘行（必须在 UI 线程内调用）。
///
/// 不刷新列表的原因：站点目录树有一层 5 分钟 KV 缓存（`dav:tree:v2`，上传和
/// 删除都不会主动失效），删完立刻刷新会把条目又读回来，看起来像「删了没效果」。
/// 先本地摘掉，缓存过期后自然一致。
fn prune_file_rows(ui: &MainWindow, names: &[String]) {
    let g = ui.global::<FilesG>();
    let model = g.get_entries();
    let kept: Vec<FileRow> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .filter(|r| !names.iter().any(|n| n == r.name.as_str()))
        .collect();
    set_file_entries(ui, kept);
}

/// K-Vault 远程的删除：走站点 API（`DELETE /api/v1/file/<id>`，要 delete 权限），
/// 按站点索引把「目录 + 名字」翻成文件 id 再删。
///
/// 不用 DAV 路径直接删的原因：站点的 DAV 显示名可能被改过（同目录重名会补
/// `~<主键末 6 位>`），而且目录树有 5 分钟缓存；站点索引
/// （`GET /api/v1/files`）里的 `folderPath` / 文件名才是原始值。
async fn kv_delete_into(
    ui: Weak<MainWindow>,
    cfg: kvault::KvCfg,
    dir: String,
    items: Vec<(String, bool)>,
) -> bool {
    if cfg.token.trim().is_empty() {
        toast(
            &ui,
            2,
            rcgui_i18n::t("删除站点文件需要 API 令牌（WebDAV 通道只读）。请编辑该远程填入令牌。"),
        );
        return false;
    }
    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_busy(true));
    let listed = kvault::list_files(&cfg.base, &cfg.token).await;
    let files = match listed {
        Ok(f) => f,
        Err(e) => {
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_busy(false));
            toast(&ui, 2, rcgui_i18n::f("读取站点索引失败: {}", &[&(e)]));
            return false;
        }
    };
    let mut gone: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for (name, is_dir) in &items {
        let ids = kvault::pick_targets(&files, &dir, name, *is_dir);
        if ids.is_empty() {
            missing.push(name.clone());
            continue;
        }
        // 站点允许同一目录里同名文件共存（靠主键区分）。删哪一条都不是用户在
        // GUI 里看到的「那一条」，所以宁可不删，把话说明白。
        if !*is_dir && ids.len() > 1 {
            failed.push(rcgui_i18n::f(
                "{}: 站点有 {} 个同名文件，请到站点网页端处理",
                &[&(name), &(ids.len())],
            ));
            continue;
        }
        let mut err: Option<String> = None;
        for id in &ids {
            if let Err(e) = kvault::delete_file(&cfg.base, &cfg.token, id).await {
                err = Some(e);
                break;
            }
        }
        match err {
            Some(e) => failed.push(format!("{name}: {e}")),
            None => gone.push(name.clone()),
        }
    }
    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_busy(false));
    if !gone.is_empty() {
        let names = gone.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| prune_file_rows(&ui, &names));
    }
    let mut msg = rcgui_i18n::f("已删除 {} 项", &[&(gone.len())]);
    if !gone.is_empty() {
        msg.push_str(rcgui_i18n::t(
            "（站点目录树有 5 分钟缓存，其它视图可能还会显示一会儿）",
        ));
    }
    if !missing.is_empty() {
        msg.push_str(&rcgui_i18n::f(
            "；{} 项在站点索引里找不到（可能已被删或改过名）",
            &[&(missing.len())],
        ));
    }
    if !failed.is_empty() {
        let shown: Vec<String> = failed.iter().take(2).cloned().collect();
        msg.push_str(&rcgui_i18n::f(
            "；失败 {}：{}",
            &[&(failed.len()), &(shown.join("；"))],
        ));
    }
    let level = if failed.is_empty() && missing.is_empty() {
        1
    } else {
        2
    };
    toast(&ui, level, msg);
    false
}

/// 这类错误重试一次就好：HTTP/2 建连抖动、连接被重置、TLS 握手中断。
///
/// 站点挂在 Cloudflare 后面，实测偶发 `Propfind …: http2: client conn could not be
/// established`，紧接着再请求就正常；把它当致命错误报给用户太吵。
fn looks_like_transient_net(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "http2",
        "could not be established",
        "connection reset",
        "eof",
        "broken pipe",
        "tls handshake",
        "timeout awaiting",
        rcgui_i18n::t("请求超时"),
        "i/o timeout",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// 上传入口的 K-Vault 分支：三个「上传到本机→远程」的入口共用。
///
/// 先判令牌（没有令牌时 rclone 那条路根本走不通），再逐个文件发站点 API，
/// 最后刷新列表 + 报一句结果。单个文件成功时把签名直链放进剪贴板 ——
/// 「传完就能发出去」是这个站点最主要的用法。
async fn kv_upload_into(
    ctx: Arc<Ctx>,
    ui: Weak<MainWindow>,
    cfg: kvault::KvCfg,
    paths: Vec<PathBuf>,
    gui_path: &str,
) -> bool {
    if let Err(why) = cfg.upload_ready() {
        toast(&ui, 2, why);
        return false;
    }
    let folder = kv_upload_folder(&cfg, gui_path);
    let _ = ui.upgrade_in_event_loop(|ui| {
        ui.global::<FilesG>().set_busy(true);
        ui.global::<LocalG>().set_busy(true);
    });
    let up = kv_upload_paths(&cfg, &folder, &paths).await;
    let _ = ui.upgrade_in_event_loop(|ui| {
        ui.global::<FilesG>().set_busy(false);
        ui.global::<LocalG>().set_busy(false);
    });
    if up.ok > 0 {
        let _ = Box::pin(handle_cmd(&ctx, &ui, Cmd::FilesRefresh { seq: -1 })).await;
    }
    let copied = up.single && !up.link.is_empty();
    if copied {
        clipboard_copy(&up.link);
    }
    let (level, mut msg) = kv_upload_report(&up, &folder);
    if copied {
        msg.push_str(&rcgui_i18n::f("；{} 的直链已复制到剪贴板", &[&(up.name)]));
    }
    toast(&ui, level, msg);
    false
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

/// 后台 tokio runtime 的 worker 栈大小。
///
/// tokio 默认 2 MB，而这台机器上最深的调用链（命令 → 插件 16 轮能力协商 →
/// 能力实现）会把它撑爆，进程直接 abort 且没有回溯。16 MB 是虚拟内存预留，
/// 不占物理页。详见 `main()` 里使用处的注释。
const BACKEND_THREAD_STACK: usize = 16 * 1024 * 1024;

// ---------------- 主入口 ----------------

/// 系统界面语言（简体 / 繁体中文都算中文，其余按英文）。
///
/// 用 `GetUserDefaultUILanguage` 而不是 `sys_locale`：前者就是 Windows 自己
/// 判定的「用户界面语言」，与资源管理器显示的一致；返回值低 10 位是主语言 ID，
/// `0x04` 即 LANG_CHINESE（不区分简繁）。
fn system_lang() -> rcgui_i18n::Lang {
    use windows::Win32::Globalization::GetUserDefaultUILanguage;
    // SAFETY: 无参数、无副作用的查询；返回值是 LANGID，失败时同样是合法数字。
    let id = unsafe { GetUserDefaultUILanguage() } as u32;
    if id & 0x3ff == 0x04 {
        rcgui_i18n::Lang::Zh
    } else {
        rcgui_i18n::Lang::En
    }
}

/// 设置里的语言标签 → 实际生效的语言（`system` / 空串 / 脏值都走系统检测）。
fn resolve_lang(tag: &str) -> rcgui_i18n::Lang {
    rcgui_i18n::Lang::from_tag(tag).unwrap_or_else(system_lang)
}

/// 设置页「界面语言」下拉的索引 ↔ 配置标签：0 跟随系统 / 1 简体中文 / 2 English。
fn lang_idx_to_tag(idx: i32) -> &'static str {
    match idx {
        1 => "zh",
        2 => "en",
        _ => rcgui_i18n::TAG_SYSTEM,
    }
}

fn tag_to_lang_idx(tag: &str) -> i32 {
    match rcgui_i18n::Lang::from_tag(tag) {
        Some(rcgui_i18n::Lang::Zh) => 1,
        Some(rcgui_i18n::Lang::En) => 2,
        None => 0,
    }
}

/// 切换 Slint 侧 `@tr` 的译文。
///
/// 必须在第一个组件（`MainWindow::new()`）之后调用：打包进去的语言是随组件
/// 创建一起登记的，之前调用会返回 `NoTranslationsBundled`。
///
/// `select_bundled_translation` 只给 `translations_dirty` 赋语言下标，窗口里
/// **已经求值过**的 `@tr` 属性未必跟着重算（实测：下拉里选中 English，Rust 侧
/// 文案已是英文，导航栏与标题栏仍停在中文）。所以再显式把下标标脏一次，强制
/// 所有 `@tr` 重新求值——Slint 文档说打包译文不需要这一步，实测需要。
fn apply_slint_language(lang: rcgui_i18n::Lang) {
    if let Err(e) = slint::select_bundled_translation(lang.slint_language()) {
        // 只影响界面语言，不该拦住启动；日志按全局约定不翻译。
        eprintln!("select_bundled_translation({:?}) 失败: {e}", lang);
    }
    slint::update_all_translations();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let silent_flag = args.iter().any(|a| a == "--silent");

    // 装上 panic 钩子：崩了至少留一行日志，否则用户只会看到窗口消失。
    // 这里装（而不是更早）是因为它要解析日志路径；用日志的都不在它之前。
    logging::install_panic_hook();

    // 无界面命令（--version / --diagnostics / --validate-config / --list-plugins /
    // --export-accessibility-tree）必须在任何窗口相关初始化之前处理：这些命令要能在
    // 没有 display 的环境里跑，也不能因为单实例互斥量或 UI 编译产物退出去。
    //
    // 带参数时先依附父进程的控制台：发布版是 GUI 子系统，从 cmd/PowerShell 启动时
    // 进程本身没有 stdout，不依附的话这些命令的输出没人收得到。
    if args.len() > 1 {
        attach_parent_console();
    }
    if let Some(code) = cli::run() {
        std::process::exit(code);
    }

    // 单实例：命名互斥量；重复启动时通过本地端口通知已有实例弹出窗口
    if !acquire_single_instance() {
        notify_show();
        return;
    }

    // 让任务栏把本进程当成一个独立的 App 身份。
    //
    // 不设这个，Windows 会按 exe 路径给任务栏按钮归组，而每个版本的 exe 路径都是
    // 新目录（portable 解压到不同文件夹、安装目录升级），于是同一程序的任务栏按钮
    // 与固定到开始菜单的快捷方式对不上，表现为「图标丢失」或显示默认白纸图标。
    set_app_user_model_id();

    // 配置
    // 必须在任何档案写入之前注册：程序若曾以管理员身份运行过，数据目录的 ACL
    // 会让普通权限进程写入报「拒绝访问」，这里给它一次自愈的机会。
    profiles::install_permission_healer();
    let cfg = config::load();

    // 界面语言：先定 Rust 侧的语言（`MainWindow::new()` 失败时的提示、以及后面
    // 写进界面的所有文案都要用它），建出窗口之后再切 Slint 侧的 `@tr` 译文。
    let lang = resolve_lang(&cfg.language);
    rcgui_i18n::set_lang(lang);

    // Slint UI
    //
    // 在这之前定下「隐藏即销毁原生窗口」的策略：隐藏到托盘、隐藏进轻量模式时，
    // 图形资源（software renderer 的 surface/context、原生窗口）真的会被释放，
    // 而不是只从屏幕上消失。见 init_destroy_on_hide 的注释。
    init_destroy_on_hide();
    let ui = MainWindow::new().expect(rcgui_i18n::t("UI 初始化失败"));
    // 窗口标题和左上角的自绘文字都读这个属性。带上版本号是为了让人一眼分清
    // 当前跑的是哪一版：排查时出现过「看到旧窗口，以为新构建没生效」的情况
    // （旧 exe 还在跑，唤起的其实是旧实例）。
    ui.set_window_label(format!("RCGUI {}", env!("CARGO_PKG_VERSION")).into());
    apply_slint_language(lang);
    apply_theme(&ui, cfg.theme_mode);
    ui.global::<SettingsG>()
        .set_theme_idx(cfg.theme_mode as i32);
    ui.global::<SettingsG>()
        .set_language_idx(tag_to_lang_idx(&cfg.language));
    ui.global::<ThemeG>().set_accent_idx(cfg.accent_idx as i32);
    ui.global::<SettingsG>()
        .set_accent_idx(cfg.accent_idx as i32);
    ui.global::<SettingsG>().set_autostart(get_autostart());
    ui.global::<SettingsG>().set_silent(cfg.silent_start);
    ui.global::<SettingsG>()
        .set_close_to_tray(cfg.close_to_tray);
    ui.global::<SettingsG>().set_http_enabled(cfg.http.enabled);
    ui.global::<SettingsG>()
        .set_http_port(cfg.http.port.to_string().into());
    ui.global::<SettingsG>()
        .set_winget_mirror_source(cfg.winget_mirror_source.clone().into());
    ui.global::<SettingsG>()
        .set_cfg_path(config::config_path().display().to_string().into());
    // 复选框反映**当前进程真实权限**，而不是记忆的偏好：重启后状态自然对齐
    ui.global::<SettingsG>().set_run_as_admin(is_elevated());
    ui.global::<MountsG>().set_winfsp_ok(winfsp_installed().0);
    ui.global::<MountsG>()
        .set_winfsp_msg(winfsp_installed().1.into());
    // 远程路径历史
    set_history(&ui.as_weak(), &cfg.path_history);

    // 数据目录迁移 / 档案初始化的一次性提示（在 UI 起来之后再吐，避免丢失）
    for msg in profiles::take_notices() {
        toast(&ui.as_weak(), 1, msg);
    }

    // 插件宿主
    let host =
        Arc::new(plugin_host::PluginHost::new().expect(rcgui_i18n::t("wasm 引擎初始化失败")));
    let store = Arc::new(kv::KvStore::new(config::data_dir().join("plugins")));

    // 命令通道
    let (tx, mut rx) = mpsc::unbounded_channel::<Cmd>();

    // 后台 runtime
    let cfg_arc = Arc::new(std::sync::Mutex::new(cfg.clone()));
    // 连接状态与进程管理分开持有：`rc` 是 RC 调用入口，`mgr` 只负责进程生命周期。
    let rc = Arc::new(RcState::new());
    // 插件长任务进度中心：宿主写入，UI 定时器读出并绘制
    let progress = Arc::new(caps::ProgressHub::default());
    progress.set_ui(ui.as_weak());
    let ctx = Arc::new(Ctx {
        rc: rc.clone(),
        mgr: Arc::new(RcloneManager::new(rc)),
        host,
        store,
        progress: progress.clone(),
        cfg: cfg_arc.clone(),
        http_restart: Arc::new(tokio::sync::Notify::new()),
        remotes_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
        providers_cache: Arc::new(std::sync::Mutex::new(None)),
        providers_err: Arc::new(std::sync::Mutex::new(None)),
        mounts_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
        pending_mounts: Arc::new(std::sync::Mutex::new(Vec::new())),
        cancelled_mounts: Arc::new(std::sync::Mutex::new(Vec::new())),
        serves_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
        last_bwlimit: Arc::new(std::sync::Mutex::new(String::new())),
        speed_meter: Arc::new(std::sync::Mutex::new(None)),
        visible: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        conn_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pending_import: Arc::new(std::sync::Mutex::new(None)),
        http_server: Arc::new(tokio::sync::Mutex::new(None)),
        hub: Arc::new(state::Hub::new()),
        usage_at: Arc::new(std::sync::Mutex::new(None)),
        drives: Arc::new(state::DriveProbe::default()),
        op_hints: Arc::new(std::sync::Mutex::new(OpHints::default())),
        tasks_done: Arc::new(std::sync::Mutex::new(Vec::new())),
        tasks_live: Arc::new(std::sync::Mutex::new(None)),
        maint: Arc::new(maintenance::Maintenance::new()),
        // 类型清单在第一次打开「新建远程」时才填满；没有它时保存会提示重开编辑器。
        type_choices: Arc::new(std::sync::Mutex::new(TypeChoices::default())),
    });

    // 轻量模式的唤醒通道：隐藏够久之后要能主动把界面清空，而清理动作只能由
    // 事件循环线程执行，所以后台循环需要一条通往 `Cmd` 队列的入口。
    lightweight::install_tx(tx.clone());
    maintenance::install_tx(tx.clone());

    // 维护页的初值必须在 `wire_callbacks` 之前灌：那一步会把 `ctx` move 进回调闭包。
    maintenance::load_into_ui(&ui, &cfg);

    // 调试开关：`RCGUI_PANEL_DUMP=<插件 id>` 只打印该插件面板的行然后退出。
    //
    // 面板外观对不上时，先用它确认宿主到底往界面发了什么，免得在 UI 和插件
    // 之间来回猜。
    if let Ok(id) = std::env::var("RCGUI_PANEL_DUMP") {
        if !id.is_empty() {
            // 内置插件平时是在 UI setup 阶段（下面 1.35 段 `ensure_builtin`）才释放到
            // 用户插件目录的，而本分支在此之前就 dump 并退出 —— 于是「全新安装下内置
            // 插件长什么样」永远看不到（只会得到「找不到 config.json」）。这里先释放一次，
            // 让调试开关看到的面板与真实首启一致。
            {
                let added = plugins::ensure_builtin(&ctx);
                if !added.is_empty() {
                    eprintln!("[plugin] 已释放内置插件: {}", added.join(", "));
                }
            }
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(BACKEND_THREAD_STACK)
                .enable_all()
                .build()
                .expect(rcgui_i18n::t("tokio runtime 启动失败"));
            match rt.block_on(plugins::build_panel(&ctx, &id)) {
                Ok(rows) => {
                    // 打印前统一脱敏：这个分支的原始输出经常被整段贴进 issue，
                    // 而表单里就有密码框和 token 字段（脱敏规则在 dump_rows_text 里）。
                    print!("{}", plugins::dump_rows_text(&rows));
                }
                Err(e) => println!("PANEL ERROR: {e:#}"),
            }
            std::process::exit(0);
        }
    }

    {
        let ctx = ctx.clone();
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                // 后台线程栈必须显式放大。
                //
                // tokio 默认只给 worker **2 MB**，而后台线程要跑的是最深的调用链：
                // `handle_cmd` → 插件动作 → `invoke_with`（16 轮能力协商循环）
                // → `cap_call`（30 多个分支，每个分支的局部变量都在同一帧里）
                // → 具体能力实现。
                //
                // 代价是这条链的**单帧**合并后相当大；实测插件里调用一次
                // `fs.search_exe` 就会把 2 MB 的 worker 栈撑爆，进程直接 abort，
                // 日志只有 `thread 'tokio-rt-worker' has overflowed its stack`，
                // 连 Rust 回溯都没有（`RUST_MIN_STACK=64M` 跑同一操作则正常，
                // 说明是「深但有限」而不是死循环）。
                //
                // 16 MB 对这条链足够，且只是虚拟内存预留，不占物理页。
                .thread_stack_size(BACKEND_THREAD_STACK)
                .enable_all()
                .build()
                .expect(rcgui_i18n::t("tokio runtime 启动失败"));
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
    //
    // 从「不可见」回到「可见」的那一拍**立刻**发一条 Foregrounded 命令：
    // 轮询间隔是 1s，等下一拍再对齐最多要 1.5s，用户会觉得「切回来还是旧数字」。
    {
        let vis = visible_flag;
        let ui_weak = ui.as_weak();
        let tx_fg = tx.clone();
        let mut was_shown = true;
        let vis_timer = Box::leak(Box::new(slint::Timer::default()));
        vis_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(500),
            move || {
                use slint::winit_030::WinitWindowAccessor;
                // 可见性以「用户要它显示吗」为准（`WINDOW_SHOWN`），只有窗口确实
                // 存在时才顺带问一句是否被最小化——最小化的窗口虽然还占着图形资源，
                // 但用户看不见，按后台处理。
                //
                // 反过来读 `is_visible()` 是不行的：隐藏会销毁原生窗口，句柄为 None，
                // 而 None 在启动早期同样成立，两者语义相反（详见 WINDOW_SHOWN 的注释）。
                let asked = WINDOW_SHOWN.load(std::sync::atomic::Ordering::Relaxed);
                let shown = asked
                    && ui_weak
                        .upgrade()
                        .and_then(|u| {
                            u.window()
                                .with_winit_window(|w| !w.is_minimized().unwrap_or(false))
                        })
                        // 刚 show 完、窗口正在重建：按可见算，宁可多轮询一拍
                        .unwrap_or(true);
                vis.store(shown, std::sync::atomic::Ordering::Relaxed);
                // 隐藏计时也在这里记：轻量模式看的是「窗口藏了多久」，
                // 后台线程不能自己问 winit，只能读这个时刻。
                lightweight::note_visible(shown);
                if shown && !was_shown {
                    // 回到前台：马上对齐一次（轮询会在之后继续，不重复发）
                    let _ = tx_fg.send(Cmd::Foregrounded);
                }
                was_shown = shown;
            },
        );
    }

    // 窗口关闭：交给命令循环按「关闭到托盘」设置决定（隐藏 / 弹首次提示 / 退出）。
    //
    // 这里**必须返回 KeepWindowShown**：返回 HideWindow 会让窗口在这一刻就被藏起来，
    // 首次关闭的提示框根本没机会显示（实测：点关闭后窗口直接消失、配置也没写）。
    // 真正的隐藏/退出都由命令处理。
    {
        let tx_c = tx.clone();
        ui.window().on_close_requested(move || {
            let _ = tx_c.send(Cmd::WindowClose);
            slint::CloseRequestResponse::KeepWindowShown
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
                        let _ = tx_db.send(Cmd::SearchApply {
                            scope: scope as i32,
                        });
                    }
                }
            },
        );
    }

    // 托盘。菜单在托盘线程里创建（右键由系统弹出），
    // 左键双击也由托盘线程自己处理，因此不需要主窗口 HWND。
    spawn_tray(ui.as_weak(), tx.clone());

    if !silent_flag && !cfg.silent_start {
        WINDOW_SHOWN.store(true, std::sync::atomic::Ordering::Relaxed);
        ui.show().expect(rcgui_i18n::t("窗口显示失败"));
    } else {
        // 静默启动（`--silent` 或设置里勾了「启动时不显示窗口」）：窗口从来没有
        // 建出来过，而 `WINDOW_SHOWN` 的初值是 true。不翻过来的话，后台会以为
        // 用户正看着界面，一直按前台节奏取数，轻量模式也永远进不去。
        WINDOW_SHOWN.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    // 窗口 / 任务栏图标。
    //
    // 必须**延后**到事件循环真正跑起来之后再设置：`ui.show()` 只是发出显示请求，
    // winit 的原生窗口要等事件循环处理后才存在。启动早期取 HWND 会得到 None，
    // 图标设置就静默失败了（实测：show() 之后立即取仍是 None）。
    // 因此先尝试一次，再用一次性定时器在事件循环里补一次。
    set_window_icon(&ui, &cfg);
    {
        let ui_weak = ui.as_weak();
        let cfg_for_icon = cfg.clone();
        let timer = Box::leak(Box::new(slint::Timer::default()));
        timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(300),
            move || {
                if let Some(u) = ui_weak.upgrade() {
                    set_window_icon(&u, &cfg_for_icon);
                }
            },
        );
    }
    slint::run_event_loop_until_quit().expect(rcgui_i18n::t("事件循环启动失败"));
}

/// 取主窗口的 HWND。
///
/// 首选 `with_winit_window`（权威来源）；失败时回退到「按窗口标题查找」。
///
/// **为什么需要回退**：Slint 的 `ui.show()` 只是发出显示请求，
/// 真正的 win32 窗口可能在事件循环开始后才由 winit 创建。启动早期调用
/// `with_winit_window` 会拿到 `None`（实测在 show() 之后仍然如此），
/// 于是图标设置静默失败。按标题查找则不依赖 winit 的内部时机。
fn main_window_hwnd(ui: &MainWindow) -> Option<isize> {
    #[cfg(windows)]
    {
        use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use slint::winit_030::WinitWindowAccessor;

        let via_winit = ui
            .window()
            .with_winit_window(|w| {
                w.window_handle().ok().and_then(|h| match h.as_raw() {
                    RawWindowHandle::Win32(h) => Some(h.hwnd.get() as isize),
                    _ => None,
                })
            })
            .flatten();
        if via_winit.is_some() {
            return via_winit;
        }
        // 回退：按标题找（标题在 main.slint 里固定为 "RCGUI"）
        find_window_by_title("RCGUI")
    }
    #[cfg(not(windows))]
    {
        let _ = ui;
        None
    }
}

/// 按窗口标题查找顶层窗口 HWND（仅回退路径使用）。
#[cfg(windows)]
fn find_window_by_title(title: &str) -> Option<isize> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
    };

    struct Ctx<'a> {
        want: &'a str,
        found: Option<isize>,
    }

    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        if ctx.found.is_some() {
            return BOOL(0); // 已找到，停止枚举
        }
        // 只看可见窗口，避免命中隐藏的同名窗口
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return BOOL(1);
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let got = GetWindowTextW(hwnd, &mut buf);
        if got <= 0 {
            return BOOL(1);
        }
        let s = String::from_utf16_lossy(&buf[..got as usize]);
        if s == ctx.want {
            ctx.found = Some(hwnd.0 as isize);
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut ctx = Ctx {
        want: title,
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.found
}
///
/// 走 win32 的 `WM_SETICON` 而不是 Slint 的 `Window::set_icon`：
/// Slint 的 `set_icon` 在某些后端（winit）对 Windows 任务栏图标
/// 不总是生效，而 `WM_SETICON` 是任务栏取图标的权威来源。
fn set_window_icon(ui: &MainWindow, cfg: &crate::config::AppConfig) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            SendMessageW, ICON_BIG, ICON_SMALL, WM_SETICON,
        };

        // 拿不到 HWND 就静默返回：启动早期（事件循环之前）本来就可能还没有原生窗口，
        // 调用方会在事件循环里用定时器再试一次。
        let Some(v) = main_window_hwnd(ui) else {
            write_icon_log("set_window_icon: hwnd not ready\n");
            return;
        };
        let h = windows::Win32::Foundation::HWND(v as *mut ::core::ffi::c_void);

        let accent = accent_rgb(cfg);
        // 同时设置大小两个尺寸：SMALL 用于标题栏/任务栏小图标，
        // BIG 用于 Alt-Tab 与任务视图。
        //
        // 尺寸按**系统当前 DPI**下期望的值生成，而不是硬编码：
        // 在高 DPI（如 175%）时 SM_CXICON 会是 56 而不是 32，
        // 给错尺寸虽然会被系统缩放，但会糊。
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXICON, SM_CXSMICON};
        for (default, metric, which) in [
            (32usize, SM_CXICON, ICON_BIG),
            (16usize, SM_CXSMICON, ICON_SMALL),
        ] {
            let m = unsafe { GetSystemMetrics(metric) };
            let size = if m > 0 { m as usize } else { default };

            if let Some(hicon) = rgba_to_hicon(&make_icon_rgba(size, accent), size) {
                let prev = unsafe {
                    SendMessageW(
                        h,
                        WM_SETICON,
                        windows::Win32::Foundation::WPARAM(which as usize),
                        windows::Win32::Foundation::LPARAM(hicon.0 as isize),
                    )
                };
                write_icon_log(&format!(
                    "set_window_icon: size={size} which={which} prev={:?}\n",
                    prev.0
                ));
                // 不 DestroyIcon：HICON 已被窗口接管（WM_SETICON 之后由系统持有），
                // 提前销毁会让图标变成空白。
            } else {
                write_icon_log(&format!("set_window_icon: hicon FAILED size={size}\n"));
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (ui, cfg);
    }
}

/// 设置进程的显式 AppUserModelID。
///
/// 任务栏按这个 ID 归组、并据此去取「快捷方式上写的图标」和「exe 里的图标资源」。
/// 用一个固定的字符串而不是默认的路径推断值，固定到开始菜单/任务栏的快捷方式才会
/// 一直指向同一个身份，升级换了安装目录也不会掉图标。
///
/// 失败只记日志：这只影响任务栏归组，不该拦住程序启动。
#[cfg(windows)]
fn set_app_user_model_id() {
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    let id = windows::core::w!("RCGUI.RcloneGui");
    unsafe {
        if let Err(e) = SetCurrentProcessExplicitAppUserModelID(id) {
            write_icon_log(&format!("set_app_user_model_id failed: {e}\n"));
        }
    }
}

#[cfg(not(windows))]
fn set_app_user_model_id() {}

/// 托盘诊断日志（同一文件，便于与图标日志对照）。
fn tray_log(msg: &str) {
    write_icon_log(&format!("[tray] {msg}\n"));
}

/// 图标/托盘诊断日志（保留给后续排查）。
///
/// 写 **exe 同目录**下的 `rcgui-icon-debug.log`。仅在需要排查
/// 「图标/托盘看不到效果」时启用（设置 `RCGUI_DEBUG_ICON=1`）。
/// 不用 `%TEMP%`：本机 `%TEMP%` 在沙箱下表现为可写、实际写入会被拒
/// （项目里其它测试也踩过这个坑，统一把临时文件放在可执行文件旁）。
fn write_icon_log(msg: &str) {
    if std::env::var("RCGUI_DEBUG_ICON").is_err() {
        return;
    }
    let mut p = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    p.push("rcgui-icon-debug.log");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = f.write_all(msg.as_bytes());
        let _ = f.flush();
    }
}

/// RGBA 像素 → Windows HICON。
///
/// 走 `CreateIconFromResourceEx` 吃一段图标格式的 DIB，而不是先 `CreateBitmap` 再
/// `CreateIconIndirect`。原因：`CreateBitmap` 给的是设备相关位图（DDB），
/// 32 位 alpha 在很多显卡驱动上会被丢掉，任务栏于是画出一个空方块——
/// 托盘那边用 RGBA 直接喂 `tray-icon`，所以看起来「托盘正常、任务栏没图标」。
/// 图标格式的 DIB 是显式的 32bpp BGRA + AND 掩码，alpha 原样保留。
///
/// 失败返回 `None`，调用方记日志并保持窗口原图标，不 panic。
#[cfg(windows)]
fn rgba_to_hicon(
    rgba: &[u8],
    size: usize,
) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconFromResourceEx, HICON, IMAGE_FLAGS};

    let dib = icon_asset::dib_image(size, rgba);
    unsafe {
        // dwVer = 0x00030000 表示这是标准图标资源（Vista 起的老规矩，不能写 0）
        match CreateIconFromResourceEx(
            &dib,
            windows::Win32::Foundation::BOOL(1),
            0x0003_0000,
            size as i32,
            size as i32,
            IMAGE_FLAGS(0),
        ) {
            Ok(icon) if !icon.0.is_null() => Some(icon),
            _ => {
                write_icon_log(&format!("rgba_to_hicon: CreateIconFromResourceEx failed\n"));
                None::<HICON>
            }
        }
    }
}
// ---------------- 单实例 ----------------

/// 把本进程挂到父进程的控制台上，让命令行开关的输出真的能被终端收到。
///
/// 发布版二进制是 GUI 子系统（见文件头的 `windows_subsystem`），从 cmd/PowerShell
/// 启动时进程没有 stdout 句柄：`--diagnostics` 的 JSON 会写进一个无效句柄，
/// 直接被丢掉。AttachConsole 会在句柄无效时补上标准句柄；调试版本来就带着控制台，
/// 这一次调用会以 ERROR_ACCESS_DENIED 失败，忽略即可。
fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // 没有可依附的父控制台（从资源管理器双击、计划任务）时同样失败：没有输出目的地，
    // 这时候安静放弃是对的——不能为了"打印一句话"凭空开一个控制台窗口。
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// 唤醒通道（第二个实例 → 已有实例「把窗口拿出来」）用的回环端口。
///
/// 由数据目录派生，所以便携版与 `RCGUI_DATA_DIR` 的验收实例各有各的端口，
/// 不会把对方当成自己。范围取 52800..=54799：**52799 留给老版本**，
/// 新实例不要去占那个号，也不要试图唤醒老实例——它用的是用户真实配置，
/// 被一个隔离实例弹到前台只会干扰验收。
fn ipc_port() -> u16 {
    let scope = config::data_dir_scope();
    let head = scope.get(0..4).unwrap_or("0");
    let v = u16::from_str_radix(head, 16).unwrap_or(0);
    52800 + (v % 2000)
}

/// 单实例互斥量句柄。
///
/// 必须保存下来：用户切换「以管理员身份运行」时本进程要先释放互斥量，
/// 否则新拉起的进程会看到 ERROR_ALREADY_EXISTS 而直接退出。
static SINGLETON: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

fn acquire_single_instance() -> bool {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    // 互斥量名字跟着数据目录走（见 config::data_dir_scope），允许用 RCGUI_SINGLETON 覆盖。
    //
    // 早先这里是个固定名字 "rclone-gui-singleton"，于是「另一个数据目录的实例」会被
    // 当成同一个程序：第二个进程直接退出，把 show 请求发给了配置完全不同的那个实例。
    // 覆盖变量保留给需要两个同目录实例的调试场景（此时两个实例会互相覆盖 config.json，
    // 要对比就各自用独立数据目录）。
    let name = std::env::var("RCGUI_SINGLETON")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("rclone-gui-singleton-{}", config::data_dir_scope()));
    unsafe {
        match CreateMutexW(None, false, &HSTRING::from(name.as_str())) {
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
            let _ = CloseHandle(HANDLE(v as *mut ::core::ffi::c_void));
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
            Some(&mut elevation as *mut _ as *mut ::core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 当前进程的完整性级别 RID（`0x1000`=Low、`0x2000`=Medium、`0x3000`=High）。
///
/// 托盘图标由 explorer.exe（Medium）托管：进程若是 Low，
/// `Shell_NotifyIconW(NIM_ADD)` 会返回 `拒绝访问 (5)`。诊断日志里带上这个值，
/// 下次再遇到「托盘图标不见了」就能一眼看出是不是完整性问题。
fn integrity_rid() -> Option<u32> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed);
        let mut buf = vec![0u8; needed as usize];
        let ok = needed > 0
            && GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(buf.as_mut_ptr() as *mut ::core::ffi::c_void),
                needed,
                &mut needed,
            )
            .is_ok();
        let _ = CloseHandle(token);
        if !ok {
            return None;
        }
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_invalid() {
            return None;
        }
        let count = *GetSidSubAuthorityCount(sid) as u32;
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, count - 1))
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
            anyhow::bail!(
                "{}",
                rcgui_i18n::f(
                    "以管理员身份重启失败（错误码 {}，可能是取消了 UAC 提示）",
                    &[&(code)]
                )
            );
        }
    } else {
        std::process::Command::new("explorer.exe")
            .arg(&exe)
            .args(&args)
            .spawn()
            .map_err(|e| anyhow!("{}", rcgui_i18n::f("以普通权限重启失败: {}", &[&(e)])))?;
    }
    Ok(())
}

fn notify_show() {
    if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", ipc_port())) {
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
    /// 当前端点是不是我们自己拉起的托管内核（仪表盘据此显示「托管 / 外部」）
    managed: bool,
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

/// 「没连上」这一支要显示的 RC 地址。
///
/// 未连接时 `RcState` 里的端点是占位默认值（5572），而旧配置里存的可能是别的地址；
/// 仪表盘必须显示**将要连的**那个，否则用户会看到一个自己没配过的端口。
/// 空/空白配置视为「没配」，回落到占位值。
fn startup_endpoint_url(configured: &str, fallback: &str) -> String {
    let t = configured.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
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
        // 连接身份（地址/版本/托管）以前只在启动时写一次，换内核后就一直显示旧端口。
        // 这里每次投影都重写：谁最后知道真实端点，顶栏和连接卡就跟着谁走。
        d.set_url(v.endpoint_url.as_str().into());
        d.set_managed(v.managed);
        if !v.version.is_empty() {
            // 离线/失败快照里版本是空的，别用它清掉上次握手拿到的版本号
            d.set_version(v.version.as_str().into());
        }
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
        managed: ctx.maint.core.is_running(),
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
            // 关键：重连后必须清掉 provider 缓存。
            // 对端可能换了实例/版本，也可能上一次是因为 rclone 侧没配 RC 认证
            // 才失败的。不清的话，用户即使修好认证、点「重新握手」，
            // 类型列表也永远停在旧结果（实测反馈：握手成功但 type 不更新）。
            *ctx.providers_cache.lock().unwrap() = None;
            *ctx.providers_err.lock().unwrap() = None;
            // 顺手预取一次：让「拿不到类型」这件事在连接时就暴露，而不是等建远程
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(8), get_providers(ctx)).await;
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
            if hs.auth_unset {
                // 连上了但对端没配 RC 鉴权：除 core/version 外全是 403，
                // 挂载 / 服务 / 参数读取都做不了。状态说得比「能力受限」更明确；
                // 具体文案由握手自己塞进 warnings（下面统一并入 notes），
                // 这里不要再补一条，否则同一件事在界面上会被说两遍。
                view.status = rcgui_i18n::t("已连接（对端未配置鉴权）").to_string();
            } else if !hs.is_compatible() {
                view.status = rcgui_i18n::t("已连接（能力受限）").to_string();
                notes.push(rcgui_i18n::f("缺少必需能力：{}", &[&(hs.missing_text())]));
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
    // 自己管理的内核优先：它的端口与凭据每次启动都是新的，只存在于内存里，
    // 配置文件里的外部地址此时是过期的。
    let managed = ctx.maint.core.endpoint();
    if let Some((url, user, pass)) = managed {
        let attempt = ctx.mgr.attach(&url, Some(user), Some(pass)).await;
        return match attempt {
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
        };
    }
    // 只要设置了用户名就带上 Authorization（密码可以为空）。
    //
    // 为什么不等密码非空才带：`rclone rcd --rc-user gui`（不带 --rc-pass）确实会
    // 开启鉴权，此时客户端必须发 `Basic gui:`（空密码）。原来的
    // `(!pass.is_empty()).then(...)` 会让 auth=None，握手直接 401
    // —— 内置插件「一键 rclone」只填用户名时就会踩到这个坑（已实测）。
    let has_user = !cfg.rclone.user.is_empty();
    let pass = if has_user {
        Some(cfg.rclone.pass.clone())
    } else {
        None
    };
    let attempt = ctx
        .mgr
        .attach(
            &cfg.rclone.url,
            Some(cfg.rclone.user.clone()).filter(|u| !u.is_empty()),
            pass,
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

/// 用户触发的重新握手：置忙 → 握手 → 复位 → 应用到 UI → 分类提示。
///
/// 只会握手，不会拉起或停止进程——进程的生命周期归托管值守所有，
/// 界面上也没有手动连接/断开的入口了。
async fn rehandshake(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    if ctx
        .conn_busy
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        toast(ui, 0, rcgui_i18n::t("正在连接，请稍候"));
        return;
    }
    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<SettingsG>().set_conn_busy(true));
    let out = handshake_only(ctx).await;
    ctx.conn_busy
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let live = out.view.live;
    let err = out.view.error.clone();
    apply_conn_view(ui, out.view);
    if live {
        if err.is_empty() {
            toast(ui, 1, rcgui_i18n::t("连接成功"));
        } else {
            toast(ui, 2, err);
        }
        // 连上后自动恢复上次的挂载/服务（rclone 自己不持久化 mount/serve）。
        // 代理的「待应用」也在同一处补下发，且必须先于 replay 完成 ——
        // 否则刚重建的挂载/服务会用到旧代理。
        spawn_restore(ctx, ui);
    } else {
        toast(ui, 2, rcgui_i18n::f("连接失败：{}", &[&(err)]));
    }
}

/// 连接成功后自动恢复上次的挂载/服务（含「待应用」的代理绑定）。
///
/// 必须放到独立线程 + 独立 runtime：调用方可能是被主循环 await 的 Send future，
/// 而 replay 内部要拿 std Mutex 与 Slint Weak，直接 `tokio::spawn` 会破坏 Send 约束。
///
/// 调用点：
/// - **托管内核值守自动重启成功之后**（`maintenance::attempt_core_start`）；
/// - **插件拉起 rclone 之后的自动连接**（`plugins::apply_rc`）。这一条是会话 16 补的：
///   冷启动时 rclone 还没起来，启动流程里 `ready == false` 会走「让插件去启动」的分支，
///   那条分支原本不 replay；插件把 rclone 拉起来并回传 rc 凭据后，挂载/服务就再也没人
///   恢复，用户看到的是「一键 rclone 启动成功，但挂载列表空的」。
///   （`main` 的启动流程在 `ready == true` 时自己内联了同样的动作，只多一步 `startup_flow`。）
pub(crate) fn spawn_restore(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let ctx2 = ctx.clone();
    let ui2 = ui.clone();
    std::thread::spawn(move || {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(async move {
                replay_pending_proxies(&ctx2, &ui2).await;
                persist::replay(&ctx2, &ui2).await;
            }),
            Err(_) => {}
        }
    });
}

/// 连一次、只回答"通没通"。值守判断内核是不是真的可用，需要握手结果；
/// 但 `ConnOutcome` 只在界面层用，没必要为它开可见性。
pub(crate) async fn handshake_live(ctx: &Arc<Ctx>) -> bool {
    connect_report(ctx).await.view.live
}

/// 插件刚拉起 rclone 之后的重连：按 2 秒间隔最多试 40 秒，连上就补挂载/服务恢复。
///
/// 为什么需要：插件回传 rc 端点时 rclone 可能还没 bind 上端口。会话 16 QEMU 客机
/// 实测（TCG 慢机器）：宿主只连一次，rclone 二十秒后才开始监听，界面就永远停在
/// 「未连接」，`apply_rc` 里那次 `spawn_restore` 也不会执行 —— 其实 `serve/start`
/// 不需要 WinFsp，本来能成功。宿主（快机器）碰不到这个竞态。
///
/// 重试期间界面保持上一次的结果（不刷错误事件），连上后自动补恢复。
pub(crate) fn spawn_connect_retry(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let ctx2 = ctx.clone();
    let ui2 = ui.clone();
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async move {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(40);
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let out = connect_report(&ctx2).await;
                if out.view.live {
                    apply_conn_view(&ui2, out.view);
                    spawn_restore(&ctx2, &ui2);
                    return;
                }
            }
        });
    });
}

/// 连接成功后补下发「待应用」的代理绑定。
///
/// 与 `persist::replay` 分开的原因：代理下发**不需要**重建挂载/服务，
/// 而挂载/服务重放需要用户确认（用户明确要求首版不自动）。两者时序上：
/// 先补代理，再重放挂载/服务，这样重建出来的实例才会用上新代理。
async fn replay_pending_proxies(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    // 先取待办清单（拿锁、clone、立刻放锁），再做 await
    let pending: Vec<(String, rcgui_profile::ProxyProfile)> = {
        let cfg = ctx.cfg.lock().unwrap();
        let reg = rcgui_profile::ProxyRegistry::new(&cfg.proxies, &cfg.proxy_bindings);
        cfg.proxy_bindings
            .iter()
            .filter(|b| b.needs_apply())
            .filter_map(|b| {
                reg.for_remote(&b.remote)
                    .map(|p| (b.remote.clone(), p.clone()))
            })
            .collect()
    };
    if pending.is_empty() {
        return;
    }
    let client = ctx.rc.client().await;
    let mut changed = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for (remote, profile) in pending {
        let mut binding = {
            let cfg = ctx.cfg.lock().unwrap();
            cfg.proxy_bindings
                .iter()
                .find(|b| b.remote.eq_ignore_ascii_case(&remote))
                .cloned()
                .unwrap_or_else(|| rcgui_profile::RemoteProxyBinding {
                    remote: remote.clone(),
                    ..Default::default()
                })
        };
        let result = proxy::apply_to_remote(&client, &remote, Some(&profile), &mut binding).await;
        // 不论成功失败都写回：换键时旧键可能已经成功还原，丢掉这次更新会让
        // 下一次比对把「已还原」误判成「被外部改过」。
        {
            let mut cfg = ctx.cfg.lock().unwrap();
            if let Some(slot) = cfg
                .proxy_bindings
                .iter_mut()
                .find(|b| b.remote.eq_ignore_ascii_case(&remote))
            {
                *slot = binding;
            } else {
                cfg.proxy_bindings.push(binding);
            }
            let _ = profiles::save_from_cfg(&cfg);
        }
        match result {
            Ok(out) => {
                if out.is_failure() {
                    errs.push(out.describe(&remote));
                } else if out.is_skipped() {
                    // 用户自己在 rclone 里设了原生代理：只补空缺，不覆盖
                    skipped.push(out.describe(&remote));
                } else if out.is_changed() {
                    changed += 1;
                }
            }
            Err(e) => errs.push(e),
        }
    }
    if changed > 0 {
        toast(
            ui,
            1,
            rcgui_i18n::f(
                "已应用 {} 条代理配置。{}",
                &[&(changed), &(proxy::restart_hint())],
            ),
        );
    }
    if !skipped.is_empty() {
        toast(ui, 0, skipped.join("；"));
    }
    if !errs.is_empty() {
        toast(ui, 2, errs.join("；"));
    }
}

/// 删除一条代理配置：先落盘，再把受影响的 remote 在 rclone 侧恢复成原状。
///
/// 顺序很重要：**先写档案**，这样即使后面的 RC 调用失败，界面与磁盘也不会互相矛盾；
/// 失败的那几个 remote 会保留「待应用」标记，下次连接自动重试。
///
/// 恢复动作走的是 `proxy::apply_to_remote(None)`，它会按记录还原用户原有代理，
/// 而不是无脑 unset —— 这是「切回不代理也不误删原有设置」的落地。
async fn delete_proxy_now(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, id: &str) {
    let (affected, name, bindings_before) = {
        let mut cfg = ctx.cfg.lock().unwrap();
        let affected = proxy::remotes_using(&cfg, id);
        let name = cfg
            .proxies
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        // 关键：回滚需要用 `original_override` / `applied_value`，而下面就会把
        // 这些绑定从档案里删掉。必须先克隆出来，否则回滚时拿到的是空绑定，
        // `plan_apply` 会判定「不是本程序管的」→ 一个字节都不写，
        // rclone 里仍然留着那个已经删掉的代理（实测就是这样漏掉的）。
        let bindings_before: Vec<rcgui_profile::RemoteProxyBinding> = cfg
            .proxy_bindings
            .iter()
            .filter(|b| b.proxy_id == id)
            .cloned()
            .collect();
        cfg.proxies.retain(|p| p.id != id);
        // 绑定也要清掉：留着就是悬空引用
        cfg.proxy_bindings.retain(|b| b.proxy_id != id);
        if let Err(e) = profiles::save_from_cfg(&cfg) {
            toast(ui, 2, rcgui_i18n::f("删除代理配置失败: {}", &[&(e)]));
            return;
        }
        (affected, name, bindings_before)
    };

    // 再在 rclone 侧回滚到「不代理」/ 用户原值（用上面克隆出来的绑定记录）
    let mut errs: Vec<String> = Vec::new();
    if ctx.rc.is_live() && !affected.is_empty() {
        let client = ctx.rc.client().await;
        for remote in &affected {
            let mut binding = bindings_before
                .iter()
                .find(|b| b.remote.eq_ignore_ascii_case(remote))
                .cloned()
                .unwrap_or_else(|| rcgui_profile::RemoteProxyBinding {
                    remote: remote.clone(),
                    ..Default::default()
                });
            match proxy::apply_to_remote(&client, remote, None, &mut binding).await {
                Ok(out) => {
                    if out.is_failure() {
                        errs.push(out.describe(remote));
                    }
                }
                Err(e) => errs.push(e),
            }
        }
    }
    if !errs.is_empty() {
        toast(ui, 2, errs.join("；"));
    }
    let msg = if affected.is_empty() {
        rcgui_i18n::f("已删除代理配置「{}」", &[&(name)])
    } else {
        rcgui_i18n::f(
            "已删除代理配置「{}」，{} 个远程已回落到「不代理」（{}）",
            &[&(name), &(affected.len()), &(affected.join("、"))],
        )
    };
    toast(ui, 1, msg);
    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ProxyListRefresh)).await;
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
    // 插件身份/版本/来源/启用/设置一并带出去，但不带二进制（导入端按 source+url 重装）
    let plugin_refs = plugins::bundle_refs(ctx);
    let conf = conf_io::build_bundle(
        &cfg,
        dump.as_ref(),
        include_secrets,
        Some(&providers),
        &live_serves,
        &live_mounts,
        &plugin_refs,
    );
    let summary = conf.summary();
    match conf_io::write_bundle(&path, &conf) {
        Ok(()) => {
            let mut msg = rcgui_i18n::f(
                "已导出 {}（远程 {} · 挂载 {} · 服务 {}）",
                &[
                    &(path.display()),
                    &(summary.remote_count),
                    &(summary.mount_count),
                    &(summary.serve_count),
                ],
            );
            if summary.sensitive {
                msg.push_str(&rcgui_i18n::f(
                    "；⚠ 含 {} 个远程的凭据，请妥善保管",
                    &[&(summary.sensitives)],
                ));
            }
            if summary.needs_auth > 0 {
                msg.push_str(&rcgui_i18n::f(
                    "；{} 个远程的凭据未随包导出（可能需要重新认证）",
                    &[&(summary.needs_auth)],
                ));
            }
            toast(ui, if summary.sensitive { 2 } else { 1 }, msg);
        }
        Err(e) => toast(ui, 2, rcgui_i18n::f("导出失败: {}", &[&(e)])),
    }
}

// ---------------- 后台主循环 ----------------

async fn backend_main(ctx: Arc<Ctx>, ui: Weak<MainWindow>, rx: &mut mpsc::UnboundedReceiver<Cmd>) {
    // 0) 一次性的旧配置迁移：内置「一键 rclone」插件退出历史舞台，它记下的
    //    自定义内核路径要接着用（用户可能专门换成了带 teldrive 等后端的 fork）。
    for note in maintenance::migrate_oneclick(&ctx) {
        toast(&ui, 1, note);
    }
    // 0.1) 旧的「外部 rclone」连法改成托管内核。只翻一个开关，用户填过的
    //      地址与口令原样留在配置里，不删。
    for note in maintenance::migrate_external_to_managed(&ctx) {
        toast(&ui, 1, note);
    }
    // 0.2) K-Vault 明文令牌原来与插件目录同处一个 kv.json，卸载插件会连令牌
    //      一起删掉。复制一份到独立命名空间；旧数据原样保留，只复制不搬走。
    let moved_tokens = kvault::migrate_kvault_tokens(&ctx.store);
    if !moved_tokens.is_empty() {
        toast(
            &ui,
            1,
            rcgui_i18n::f(
                "已把 {} 个 K-Vault 远程的令牌复制到独立存储（卸载插件不会再连带删掉）",
                &[&moved_tokens.len()],
            ),
        );
    }

    // 0.5) 托管内核：random 端口 + random 凭据，只活在本进程里。
    //      失败不拦启动：维护页会写明原因，值守循环会接着重试。
    if let Some(note) = maintenance::start_managed_core(&ctx).await {
        toast(&ui, 1, note);
    }

    // 后台的两个常驻循环：同步任务调度（轻量模式也照跑）与轻量模式自身的进出。
    tokio::spawn(maintenance::watch(ctx.clone(), ui.clone()));
    tokio::spawn(lightweight::watch(ctx.clone(), ui.clone()));

    // 1) 启动时连一次。托管内核起来了就连它（凭据在内存里）；
    //    没起来而旧配置里存着外部地址、以前又勾过「启动时连接」，就照旧**只连**那个地址
    //    兜底一次——内核这一轮起不来，不该把本来能用的外部 rclone 一起挡在门外。
    //    连接失败 → 离线模式；**任何情况下都不会**在这里启动 rclone 进程。
    let conn = if ctx.maint.core.is_running() || ctx.cfg.lock().unwrap().rclone.connect_on_start {
        connect_report(&ctx).await
    } else {
        let mut v = snapshot_view(&ctx).await;
        // 没连过时 `RcState` 里的端点是 `Endpoint::loopback_default()`（5572），
        // 而用户配的可能是别的地址。仪表盘的「RC 地址」必须显示**将要连的**那个，
        // 否则关掉「启动时自动连接」后会看到一个自己没配过的端口（会话 11 实测踩到）。
        let saved = ctx.cfg.lock().unwrap().rclone.url.clone();
        v.endpoint_url = startup_endpoint_url(&saved, &v.endpoint_url);
        v.error =
            rcgui_i18n::t("尚未连接到内核：程序会自动重试；若一直不行，设置页的状态里会写明原因。")
                .to_string();
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
        toast(&ui, 2, rcgui_i18n::f("未连接：{}", &[&(conn.view.error)]));
    }
    {
        // 托管与否由内核实际状态决定：自己起的那个进程在跑才算托管，
        // 连的是别人已有的实例就还是外部。
        let managed = ctx.maint.core.is_running();
        let vi = conn.version.unwrap_or_default();
        let version = vi.version;
        let os_info = format!("{} / {}", vi.os, vi.arch);
        let url = conn.view.endpoint_url;
        let _ = ui.upgrade_in_event_loop(move |ui| {
            let g = ui.global::<DashboardG>();
            g.set_version(version.into());
            g.set_os_info(os_info.into());
            g.set_managed(managed);
            g.set_url(url.into());
        });
    }

    // 1.35) 释放内置插件：它们在 assets/plugins 里随程序发布，但装到用户数据目录后
    //       与用户自己装的插件完全同构，因此可以卸载。卸载过的（`plugin_removed`
    //       墓碑）不再释放，直到用户点「恢复内置」。
    {
        let added = plugins::ensure_builtin(&ctx);
        if !added.is_empty() {
            eprintln!("[plugin] 已释放内置插件: {}", added.join(", "));
        }
    }

    // 1.4) 插件自启动：按插件声明的 `plugin_setting.on_start` 让它自己决定做什么。
    //
    // 默认配置下没有任何插件会被释放到 enabled=true 之外的状态，因此这里通常什么都不做；
    // 具体「要不要拉起 rclone」由插件在 wasm 里判断并操作宿主能力，宿主不解释语义。
    if !ready {
        plugins::autostart(&ctx, &ui).await;
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
    //
    // **不要再往 `DashboardG.url` 里写 HTTP 端口**：那个属性在界面上是
    // 「RC 地址」（仪表盘 + 底部状态栏两处），语义是**rclone RC 端点**。
    // 早先这里写过 `http://127.0.0.1:{http_port}`，只要开了内嵌 HTTP
    // （默认开、端口 5273），就会把上面刚设好的 RC 地址覆盖掉 ——
    // 表现为「连上 rclone 了但 RC 地址显示 5273」，会话 11 实测踩到。
    let http_cfg = ctx.cfg.lock().unwrap().http.clone();
    if http_cfg.enabled {
        spawn_http(&ctx, http_cfg.port, ui.clone()).await;
    }

    // 3) 单实例 IPC（收到 show → 显示窗口）
    {
        let ui = ui.clone();
        tokio::spawn(async move {
            let port = ipc_port();
            if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                loop {
                    if let Ok((mut sock, _)) = listener.accept().await {
                        use tokio::io::AsyncReadExt as _;
                        let mut buf = [0u8; 16];
                        if sock.read(&mut buf).await.is_ok() {
                            // 走同一个「显示并置前」入口：它会记账 WINDOW_SHOWN、
                            // 取消最小化、并在窗口刚重建时补一次置前。
                            let _ = ui.upgrade_in_event_loop(|ui| {
                                show_main_window(&ui);
                            });
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
    //
    // 更新只有两条线路（见 state.rs）：
    //   ① 轮询：前台、无用户操作、上一轮取数已完成 → 每秒取一份完整快照；
    //   ② 用户输入：命令进来时暂停轮询、动手前后各对齐一次（见 handle_cmd 包装）。
    // 窗口最小化 / 隐藏时两条线路都停，回到前台先立即对齐一次。
    let mut tick: u64 = 0;
    let mut last_dark: Option<bool> = None;
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(1000));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                tick += 1;
                // 后台（最小化 / 隐藏到托盘）：**停止一切轮询与 UI 写入**。
                // 这里连本地进度泵都不跑 —— 界面看不见，没有必要合成帧。
                // 回到前台那一下由 UI 线程的可见性采样直接发 `Cmd::Foregrounded`，
                // 不走这条 1s 的节拍（否则最多要等 1.5s 才对齐）。
                if !ctx.visible.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                // 本地进程状态（不产生任何 RC 请求，也不受「未连接」影响）：
                // 插件长任务进度（下载 / 解压 / 安装）+ 插件子进程存活灯
                caps::pump_progress(&ctx.progress);
                // 插件要求的面板定时重跑（例如扫码登录在等用户在手机上确认）。
                // 只在那个插件的面板确实开着、且没有别的动作在跑时触发；也不设
                // busy —— 每秒闪一下「处理中…」比不动更难看。
                if let Some((rid, raction)) = plugins::panel_repeat_due() {
                    let open = ui
                        .upgrade()
                        .map(|u| {
                            let g = u.global::<PluginsG>();
                            g.get_panel_plugin().as_str() == rid.as_str()
                                && g.get_panel_busy_action().is_empty()
                        })
                        .unwrap_or(false);
                    if open {
                        let ctx2 = ctx.clone();
                        let ui2 = ui.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                plugins::run_action(&ctx2, &rid, &raction, &ui2).await
                            {
                                eprintln!("面板定时动作 {raction} 失败: {e:#}");
                                return;
                            }
                            match plugins::build_panel(&ctx2, &rid).await {
                                Ok(rows) => {
                                    let _ = ui2.upgrade_in_event_loop(move |u| {
                                        plugins::apply_panel(&u, &rows);
                                    });
                                }
                                Err(e) => eprintln!("面板定时刷新失败: {e:#}"),
                            }
                        });
                    }
                }
                // 周期任务门控：用户操作进行中 → 轮询让位；上一轮取数未完成 → 不叠加。
                let live = ctx.rc.is_live();
                if ctx.hub.status(true, live) == crate::state::PollStatus::Running
                    && !ctx.hub.fetch_in_flight()
                {
                    let ctx2 = ctx.clone();
                    let ui2 = ui.clone();
                    let t = tick;
                    tokio::spawn(async move {
                        // 线路①：一份完整快照 → 计数与三个列表同一帧更新
                        refresh::refresh_state(&ctx2, &ui2, false).await;
                        // 传输页：每 10s 全量重建（core_transferred 响应大，不宜频繁），
                        // 其余各跳走快路径，只更新运行中行的速度/进度
                        if t % 10 == 0 {
                            build_tasks(&ctx2, &ui2).await;
                        } else {
                            refresh_tasks_fast(&ctx2, &ui2).await;
                        }
                        // 限速只在配置值变化时下发（apply_global_limit 内部已判重）
                        if t % 2 == 0 {
                            apply_global_limit(&ctx2, &ui2).await;
                        }
                        // 已连接时更新「上次刷新」时间戳
                        if t % 5 == 0 {
                            let ts = now_hms();
                            let _ = ui2.upgrade_in_event_loop(move |ui| {
                                ui.global::<SettingsG>().set_conn_last_refresh(ts.into());
                            });
                        }
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
            // 类型没有实现的远程不去预热：DAV 的 about/list 会成功，状态栏随即显示
            // 「正常」，而用户点进去只会看到拒绝。宁可不做这一步。
            if kv_access_blocked(&ctx2, &name).await.is_some() {
                return;
            }
            // 1) 连接校验（失败 → 该预设流程终止）。8s 超时，防止慢后端把整个启动流程拖死
            let fs = format!("{name}:");
            let ok = tokio::time::timeout(std::time::Duration::from_secs(8), client.about(&fs))
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
/// `wait_mount_ready_rc` 允许「rclone 说还挂着」时额外延长的上限。
/// 没有它的话，盘符永远不出现时该函数会无限等待，界面卡在 spinner 且收不到任何提示。
const MOUNT_READY_EXTEND_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// 该挂载点是否已被用户主动取消（卸载）。
///
/// 一次 `MountAdd` 的等待循环可能比用户的操作更慢：用户挂上后马上卸载，
/// 循环醒来会发现 rclone 不再列出该挂载点。此时必须区分两种情况：
/// - 挂载真的失败 → 应该报错；
/// - 用户自己撤掉了 → 不应报错（否则就是「卸载后弹出未生效」的假错误）。
fn mount_was_cancelled(ctx: &Arc<Ctx>, point: &str) -> bool {
    let key = mount_key(point);
    ctx.cancelled_mounts
        .lock()
        .map(|c| c.iter().any(|k| k == &key))
        .unwrap_or(false)
}

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
///
/// **必须有基于绝对时间的硬上限**：早期实现用 `waited += step` / `waited -= step`
/// 的净增长控制预算，一旦每轮都被延长，净增长恒为 0，函数永不返回 ——
/// 调用方后面的成功/失败提示与 UI 刷新全都不会执行，界面就停在 spinner 上。
/// 这里改用 `deadline`（挂钟时间），延长只影响「是否继续等」，不可能越过上限。
async fn wait_mount_ready_rc(
    client: &rclone_client::RcClient,
    point: &str,
    timeout: std::time::Duration,
    cancelled_mounts: &Arc<std::sync::Mutex<Vec<String>>>,
) -> bool {
    let key = mount_key(point);
    let start = tokio::time::Instant::now();
    // 延长后的绝对截止时间：无论 rclone 怎么说，到这里必须返回
    let hard_deadline = start + timeout + MOUNT_READY_EXTEND_CAP;
    let step = std::time::Duration::from_secs(3);
    // 连续多少次「rclone 不认该挂载点」才认为已被撤掉。
    // 不能只判一次：挂载刚发起时 listmounts 可能还没收录新条目（实测有抖动），
    // 只判一次会把「正在挂载」误判成「已撤销」→ 立刻报「盘符未出现」。
    let mut missing_streak = 0u32;
    const MISSING_STREAK_LIMIT: u32 = 3;
    loop {
        // 用户已经主动卸载了这个挂载点 → 静默退出，不要当成「挂载失败」报错
        if cancelled_mounts
            .lock()
            .map(|c| c.iter().any(|k| k == &key))
            .unwrap_or(false)
        {
            return false;
        }
        let now = tokio::time::Instant::now();
        if now >= hard_deadline {
            return false;
        }
        // 每轮做一次短等待，避免频繁轮询 RC
        let slice = step.min(hard_deadline.saturating_duration_since(now));
        if wait_mount_ready(point, slice).await {
            return true;
        }
        if tokio::time::Instant::now() >= hard_deadline {
            return false;
        }
        // rclone 是否仍然认为该挂载点挂着（用于决定「继续等」还是「已被撤掉」）
        match client.mounts().await {
            Ok(ms) => {
                if ms.iter().any(|m| mount_key(&m.mount_point) == key) {
                    missing_streak = 0; // 仍在 → 继续等盘符出现
                } else {
                    missing_streak += 1;
                    // 连续多次都不在列表里，才认为该挂载已被撤销（用户点了卸载，
                    // 或 rclone 侧已放弃）。此前的单次判断会在卸载/抖动时误报失败。
                    if missing_streak >= MISSING_STREAK_LIMIT {
                        return false;
                    }
                }
            }
            // 查询失败（RC 抖动）：不要据此下结论，继续按 deadline 等
            Err(_) => {}
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
        toast(
            ui,
            2,
            rcgui_i18n::f("挂载 {} 失败：{}", &[&(point), &(msg)]),
        );
        return;
    }
    let hint = if mount_point_ready(point) {
        rcgui_i18n::t("该盘符已被占用，请换一个盘符")
    } else {
        rcgui_i18n::t("WinFsp 正常，但盘符未出现（网盘首次挂载可能较慢；也可能是盘符被占用或隐藏）")
    };
    toast(
        ui,
        2,
        rcgui_i18n::f("挂载 {} 到 {} 未生效：{}", &[&(fs), &(point), &(hint)]),
    );
}

/// 启动（或重启）内嵌 HTTP 服务。
///
/// 端口冲突（os error 10048）的根因是：旧实例的 listener 在重启时尚未释放。
/// 因此这里先向旧实例发送关闭信号并等待其任务真正结束，再绑定新端口。
async fn spawn_http(ctx: &Arc<Ctx>, port: u16, ui: Weak<MainWindow>) {
    // 1) 优雅关闭并等待旧实例退出（释放监听套接字）
    stop_http(ctx).await;

    let st = http::HttpState { ctx: ctx.clone() };

    // 2) 先绑定：失败时给出明确原因，且不会留下"半启动"的服务
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            let hint = if e.kind() == std::io::ErrorKind::AddrInUse {
                rcgui_i18n::t("端口已被其他程序占用，请在设置中改用其他端口")
            } else {
                rcgui_i18n::t("请检查端口设置")
            };
            toast(
                &ui,
                2,
                rcgui_i18n::f(
                    "HTTP 服务启动失败（端口 {}）：{}；{}",
                    &[&(port), &(e), &(hint)],
                ),
            );
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
    *ctx.http_server.lock().await = Some(HttpServer {
        shutdown: tx_shutdown,
        handle,
    });
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
            Err(e) => errs.push(rcgui_i18n::f("远程 {}: {}", &[&(r.name), &(e)])),
        }
    }

    // 凭据缺失的远程集合。
    //
    // 为什么需要这个：导出时默认会剥离凭据（`needs_auth = true`），
    // 导入后 rclone 侧的该远程是「参数不全」的状态。此时对它做任何操作
    // （挂载、serve/start）rclone 都会在构造后端时报错，例如 teldrive 会报
    // `missing api_key`、drive 会报 token 相关错误。
    //
    // 那个错误看起来像「服务启动参数不对」，实际根因是**凭据没随包导出**。
    // 因此这里提前跳过并给出真正的原因，而不是把 rclone 的底层报错当结论。
    let needs_auth: std::collections::HashSet<String> = conf
        .remotes
        .iter()
        .filter(|r| r.needs_auth)
        .map(|r| r.name.to_ascii_lowercase())
        .collect();
    let mut skipped_auth: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

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
                errs.push(rcgui_i18n::f("记录挂载 {}: {}", &[&(point), &(e)]));
            }
        }
        if needs_auth.contains(&m.remote.to_ascii_lowercase()) {
            // 记进档案（下次有凭据时会自动重建），但这次不尝试挂载
            skipped_auth.insert(m.remote.clone());
            continue;
        }
        // 导入的档案里也可能有被标记的远程：它的类型插件不在时挂上去只会立刻
        // 403。档案条目已经记下了，插件装回来下次启动会自动重建。
        if let Some(msg) = kv_access_blocked(ctx, &m.remote).await {
            errs.push(rcgui_i18n::f(
                "挂载 {} → {}: {}",
                &[&(m.remote), &(point), &(msg)],
            ));
            continue;
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
            Ok(Err(e)) => errs.push(rcgui_i18n::f("挂载 {} → {}: {}", &[&(fs), &(point), &(e)])),
            Err(_) => errs.push(rcgui_i18n::f("挂载 {} → {}: 超时", &[&(fs), &(point)])),
        }
    }

    // 3) 服务：先查运行态，已在监听同一地址的直接跳过。
    //
    // 这里必须和挂载一样做「已存在则跳过」检查，否则导入时会对已在运行的
    // 8080/8081 再调一次 serve/start，rclone 报
    // `bind: Only one usage of each socket address is normally permitted`。
    // 那个报错看起来像导入失败，其实服务本来就好好跑着。
    let live_addrs: Vec<String> = match client.serve_list().await {
        Ok(v) => v
            .get("list")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|it| {
                        it.get("params")
                            .and_then(|p| p.get("addr"))
                            .or_else(|| it.get("addr"))
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    for s in &conf.profile.serves {
        if s.remote.trim().is_empty() {
            continue;
        }
        // 同样记进档案，供下次连接自动恢复
        {
            let mut cfg = ctx.cfg.lock().unwrap();
            if let Err(e) = persist::remember_serve(
                &mut cfg,
                &s.stype,
                &s.remote,
                &s.sub_path,
                &s.addr,
                &s.user,
                &s.pass,
            ) {
                errs.push(rcgui_i18n::f("记录服务 {}: {}", &[&(s.addr), &(e)]));
            }
        }
        // 已在监听该地址 → 视为成功，不重复启动
        if live_addrs.iter().any(|a| a == &s.addr) {
            ok_serves += 1;
            continue;
        }
        // 凭据缺失 → 不尝试启动。
        //
        // 否则 rclone 会在构造后端时失败并报出底层错误（如 teldrive 的
        // `missing api_key`），看起来像「服务参数不对」，实际是凭据没随包导出。
        // 那个错误信息会严重误导排查方向，因此这里提前跳过。
        if needs_auth.contains(&s.remote.to_ascii_lowercase()) {
            skipped_auth.insert(s.remote.clone());
            continue;
        }
        // 同上：被标记的远程在本程序里用不了，起服务等于把它重新暴露成一个
        // 打不开的地址。档案条目已记下，插件装回来会自动重建。
        if let Some(msg) = kv_access_blocked(ctx, &s.remote).await {
            errs.push(rcgui_i18n::f(
                "服务 {} {}: {}",
                &[&(s.stype), &(s.addr), &(msg)],
            ));
            continue;
        }
        let fs = compose_fs(&s.remote, &s.sub_path);
        match client
            .serve_start(&s.stype, &fs, &s.addr, &s.user, &s.pass)
            .await
        {
            Ok(_) => ok_serves += 1,
            Err(e) => errs.push(rcgui_i18n::f(
                "服务 {} {}: {}",
                &[&(s.stype), &(s.addr), &(e)],
            )),
        }
    }

    // 把「因缺凭据而跳过」的原因单独汇总：这不是失败，而是需要用户补凭据。
    // 归到 errs 会让「失败 N 项」看起来像程序缺陷，所以单独一条说明。
    if !skipped_auth.is_empty() {
        let names: Vec<String> = skipped_auth.into_iter().collect();
        errs.push(rcgui_i18n::f("以下远程的凭据未随包导出，已跳过其挂载/服务的启动（导入后在「远程」页补全凭据即可启用）：{}", &[&(names.join("、"))]));
    }

    (ok_remotes, ok_mounts, ok_serves, errs)
}

/// 返回 true 表示退出
/// 文件页导航的统一实现：列出 `target`，**成功后才**更新路径、面包屑与列表。
///
/// 「进入子目录 / 返回上级 / 点面包屑」都走这里，避免三份几乎相同的代码各自出错；
/// 过期请求（seq 不一致）只清 loading，绝不覆盖界面。
async fn files_navigate(
    ctx: &Arc<Ctx>,
    ui: &Weak<MainWindow>,
    target: String,
    seq: i32,
    fail_prefix: String,
) {
    let (remote, _) = ui_files_pos(ui).await;
    if remote.is_empty() {
        return;
    }
    // 进入子目录 / 返回上级 / 点面包屑都从这里过：插件不在就别往下走了
    if let Some(msg) = kv_access_blocked(ctx, &remote).await {
        let _ = ui.upgrade_in_event_loop(move |ui| {
            ui.global::<FilesG>().set_loading(false);
            toast_ui(&ui, 2, msg);
        });
        return;
    }
    let proxy_ep = remote_proxy_endpoint(ctx, &remote);
    let client = ctx.rc.client().await;
    let fs = format!("{remote}:");
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client.list(&fs, &target, None),
    )
    .await
    .map_err(|_| anyhow!("{}", rcgui_i18n::t("请求超时")))
    .and_then(|r| r.map_err(anyhow::Error::from));

    let cur_seq = read_ui(ui, |u| u.global::<FilesG>().get_seq())
        .await
        .unwrap_or(-2);
    if seq >= 0 && cur_seq != seq {
        let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
        return;
    }
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let g = ui.global::<FilesG>();
        g.set_loading(false);
        match res {
            Ok(entries) => {
                // 成功后才更新路径，避免网络慢时拼出不存在的路径
                g.set_path(target.clone().into());
                apply_crumbs(&ui, file_crumbs(&remote, &target));
                let key = g.get_sort_key().to_string();
                let desc = g.get_sort_desc();
                // 走 set_file_entries：新建/切换目录后勾选态必须清空且长度对齐
                let rows = file_rows(&entries, &key, desc);
                set_file_entries(&ui, rows);
            }
            Err(e) => {
                let msg = crate::proxy::annotate_error(
                    &format!("{fail_prefix}: {e}"),
                    proxy_ep.as_deref(),
                );
                toast_ui(&ui, 2, msg);
            }
        }
        g.set_sel(-1);
    });
}

/// 命令入口：**用户操作**按「暂停轮询 → 动手前对齐 → 执行 → 动手后再对齐」走。
///
/// 为什么动手前必须对齐一次：用户看到的列表可能已经过期（比如 T: 刚被别的工具挂上、
/// 或服务已被外部停掉），基于旧状态做的判断（盘符占用、名称冲突）会做错事。
/// rclone 才是真相，命令发布前先读一次。
///
/// 只读命令（切页、搜索、浏览文件……）不进这条路径：它们本来就只是「读」。
async fn handle_cmd(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, cmd: Cmd) -> bool {
    if !cmd.mutates_rclone_state() {
        return handle_cmd_inner(ctx, ui, cmd).await;
    }
    // 守卫：期间轮询让位（Drop 时恢复，任何提前 return / panic 都不会卡住轮询）
    let _op = state::UserOpGuard::begin(&ctx.hub);
    // ① 动手前对齐：用 rclone 的真实状态覆盖界面里的旧状态
    refresh::refresh_state(ctx, ui, true).await;
    let quit = handle_cmd_inner(ctx, ui, cmd).await;
    // ② 动手后对齐：结果异步落地（挂载盘符出现、服务端口监听）也在这一步收敛到界面
    refresh::refresh_state(ctx, ui, true).await;
    quit
}

impl Cmd {
    /// 这条命令会不会改变 rclone 的运行状态（挂载 / 服务 / 远程 / 代理 / 连接）。
    ///
    /// 只有这类命令需要「暂停轮询 + 动手前后各对齐一次」；纯读命令直接执行。
    fn mutates_rclone_state(&self) -> bool {
        matches!(
            self,
            Cmd::MountAdd
                | Cmd::MountRemove { .. }
                | Cmd::ServeStart
                | Cmd::ServeStop { .. }
                | Cmd::RemoteSave
                | Cmd::RemoteDelete { .. }
                | Cmd::ProxyBind { .. }
                | Cmd::ProxySave
                | Cmd::ProxyDelete { .. }
                | Cmd::ConfApply
        )
    }
}

async fn handle_cmd_inner(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>, cmd: Cmd) -> bool {
    match cmd {
        Cmd::Maint { action, id } => {
            // 维护页的按钮全部落到 `maintenance::handle`。返回值表示"要不要退出"，
            // 这里的动作都不会退出程序。
            let _ = crate::maintenance::handle(ctx, ui, &action, &id).await;
            false
        }
        Cmd::Quit => {
            // 关闭 GUI 只做三件事：停止轮询、释放连接、退出事件循环。
            // **不**调用 core/quit、**不**杀 rclone、**不**卸载挂载、**不**停止 serve：
            // rclone 的生命周期与运行中的挂载/服务都由用户自己掌握。
            //
            // 唯一例外：插件通过 `proc.start` 拉起的子进程，只要它没把
            // `keep_on_exit` 置真，就在这里结束掉（见 `caps::stop_plugin_procs_on_exit`）。
            // 是否「退出时停止」由插件自己决定并告诉宿主，宿主不需要理解插件语义。
            //
            // ---- 退出路径的健壮性（本轮踩坑）----
            // 实测退出处理里只要有一步卡住，关窗后进程就变成没有窗口的僵尸：
            // 命令循环卡在退出处理里的某个 await 上，`quit_event_loop()` 永远等不到。
            // 根因没能定位（RwLock / http_server 锁都已排除，线程全在 Wait），因此这里
            // 不再依赖「清理一定完成」：
            //   1) 先挂 4s 看门狗，任何一步卡住都强制退出；
            //   2) 只有「停子进程」是必须同步做完的（快、且涉及真实进程）；
            //   3) 其余清理 + 请求退出放到独立线程 + 独立 runtime，绝不阻塞命令循环。
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(4));
                std::process::exit(0);
            });
            caps::stop_plugin_procs_on_exit();
            // 还有没发完的上传也一并撤掉：进程马上就没了，留着授权只会让它
            // 在退出清理的那几秒里被某个远程动作捡起来用。
            plugin_host::upload::cancel_all();
            // 自己起的 rclone 内核跟着退出。只可能结束本进程亲手 spawn 的那个 child
            // （`core::Core` 里只保存自己启动的句柄）；连外部实例时这里是空操作，
            // 用户的 rclone 一个字都不会被动到。
            ctx.maint.core.stop();
            let ctx2 = ctx.clone();
            let ui2 = ui.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt {
                    rt.block_on(async move {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            stop_http(&ctx2),
                        )
                        .await;
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            ctx2.rc.set_disconnected(None),
                        )
                        .await;
                    });
                }
                // 让 UI 线程自己退出事件循环（跨线程直接调 quit_event_loop 有几率不生效）
                let posted = ui2
                    .upgrade_in_event_loop(|_| {
                        let _ = slint::quit_event_loop();
                    })
                    .is_ok();
                if !posted {
                    let _ = slint::quit_event_loop();
                }
            });
            true
        }
        Cmd::RefreshDashboard => {
            // 仪表盘不再单独取数：所有数字都来自同一份状态快照
            refresh::refresh_state(ctx, ui, true).await;
            false
        }
        Cmd::Foregrounded => {
            // 窗口从隐藏回到前台：先退出轻量模式，再刷新。
            //
            // 顺序不能反。轻量模式里 `suppress_rows` 会挡住列表回填，而 `ENTERED`
            // 要等 `lightweight::watch` 下一个 30 秒的 tick 才会自己清掉——先刷新的话，
            // 这一次刷回来的数据会被整片丢掉，用户看到的是空白页，最长等到半分钟。
            // 这里直接调 `leave`，等于把「窗口回来了」当成立刻可用的信号。
            crate::lightweight::leave(ctx, ui);
            // 回到前台的第二件事：用 rclone 的真实状态覆盖界面（后台期间它可能变了）
            refresh::refresh_state(ctx, ui, true).await;
            false
        }
        Cmd::WindowClose => {
            let (close_to_tray, prompt_done) = {
                let c = ctx.cfg.lock().unwrap();
                (c.close_to_tray, c.close_prompt_done)
            };
            if !prompt_done {
                // 首次关闭：弹三选一提示，由用户决定。此时候选框还没被用户动过，
                // 所以**不改任何配置**，只把界面里的复选框同步成当前真实值。
                let _ = ui.upgrade_in_event_loop(|ui| {
                    ui.global::<SettingsG>().set_close_prompt_show(true);
                });
                return false;
            }
            if close_to_tray {
                hide_to_tray(ui);
            } else {
                // 这次是真的退出：先记账再隐藏，免得那半秒里后台还以为窗口在前台。
                WINDOW_SHOWN.store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = ui.upgrade_in_event_loop(|ui| {
                    ui.hide().ok();
                });
                return Box::pin(handle_cmd(ctx, ui, Cmd::Quit)).await;
            }
            false
        }
        Cmd::CloseToTrayConfirm => {
            // 勾选 + 记住「问过了」→ 隐藏到托盘
            {
                let mut cfg = ctx.cfg.lock().unwrap();
                cfg.close_to_tray = true;
                cfg.close_prompt_done = true;
                let _ = config::save(&cfg);
            }
            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.global::<SettingsG>().set_close_to_tray(true);
                ui.global::<SettingsG>().set_close_prompt_show(false);
            });
            hide_to_tray(ui);
            false
        }
        Cmd::CloseToTrayCancel => {
            // 什么都不做：不写配置、不隐藏。下次关闭还会问。
            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.global::<SettingsG>().set_close_prompt_show(false);
            });
            false
        }
        Cmd::CloseToTrayQuit => {
            // 取消勾选 + 记住「问过了」→ 退出（托盘图标随进程一起消失）
            {
                let mut cfg = ctx.cfg.lock().unwrap();
                cfg.close_to_tray = false;
                cfg.close_prompt_done = true;
                let _ = config::save(&cfg);
            }
            WINDOW_SHOWN.store(false, std::sync::atomic::Ordering::Relaxed);
            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.global::<SettingsG>().set_close_to_tray(false);
                ui.global::<SettingsG>().set_close_prompt_show(false);
                ui.hide().ok();
            });
            Box::pin(handle_cmd(ctx, ui, Cmd::Quit)).await
        }
        Cmd::FilesInit => {
            let client = ctx.rc.client().await;
            let remotes = client.list_remotes().await.unwrap_or_default();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                ui.global::<FilesG>()
                    .set_remotes(ModelRc::from(Rc::new(VecModel::from(
                        remotes
                            .iter()
                            .map(|r| r.clone().into())
                            .collect::<Vec<slint::SharedString>>(),
                    ))));
                ui.global::<FilesG>().set_remote_idx(0);
            });
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
            // 插件不在时，被标记的远程连列目录都不给：清空列表 + 说清原因。
            // 不能像以前那样悄悄落到 DAV 只读，那样用户看不出插件已经卸了。
            if let Some(msg) = kv_access_blocked(ctx, &remote).await {
                let cur_seq = read_ui(ui, |u| u.global::<FilesG>().get_seq())
                    .await
                    .unwrap_or(-2);
                if seq >= 0 && cur_seq != seq {
                    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
                    return false;
                }
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    let g = ui.global::<FilesG>();
                    set_file_entries(&ui, Vec::<FileRow>::new());
                    // 面包屑照旧重建：路径本身还是用户看到的那个，只是里面没东西可看
                    apply_crumbs(&ui, file_crumbs(&remote, &path));
                    g.set_sel(-1);
                    g.set_loading(false);
                    toast_ui(&ui, 2, msg);
                });
                return false;
            }
            // 取一次「该远程是否绑了代理」：网络类错误没有代理证据时，
            // 只给弱提示（见 proxy::annotate_error），不能断言是谁的问题。
            let proxy_ep = remote_proxy_endpoint(ctx, &remote);
            // k-vault 远程：列站点索引，比 DAV 新鲜，也不走那条抖动的 HTTP/2 链路
            let sort = read_ui(ui, |u| {
                (
                    u.global::<FilesG>().get_sort_key().to_string(),
                    u.global::<FilesG>().get_sort_desc(),
                )
            })
            .await
            .unwrap_or_else(|| ("name".to_string(), false));
            if let Some(res) = kv_list_rows(&ctx, &remote, &path, &sort.0, sort.1).await {
                // 请求期间用户可能已换目录（seq 变化），丢弃过期结果
                let cur_seq = read_ui(ui, |u| u.global::<FilesG>().get_seq())
                    .await
                    .unwrap_or(-2);
                if seq >= 0 && cur_seq != seq {
                    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
                    return false;
                }
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    let g = ui.global::<FilesG>();
                    match res {
                        Ok(rows) => set_file_entries(&ui, rows),
                        Err(e) => {
                            set_file_entries(&ui, Vec::<FileRow>::new());
                            toast_ui(&ui, 2, rcgui_i18n::f("列目录失败: {}", &[&(e)]));
                        }
                    }
                    apply_crumbs(&ui, file_crumbs(&remote, &path));
                    g.set_sel(-1);
                    g.set_loading(false);
                });
                return false;
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let mut res = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                client.list(&fs, &path, None),
            )
            .await
            .map_err(|_| anyhow!("{}", rcgui_i18n::t("请求超时")))
            .and_then(|r| r.map_err(anyhow::Error::from));
            // 站点在 Cloudflare 后面，HTTP/2 建连偶发抖动（"http2: client conn could not
            // be established"）；这种错误自己会好，重试一次远好过让用户盯着红色报错。
            if let Err(e) = &res {
                if looks_like_transient_net(&e.to_string()) {
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    res = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        client.list(&fs, &path, None),
                    )
                    .await
                    .map_err(|_| anyhow!("{}", rcgui_i18n::t("重试仍超时")))
                    .and_then(|r| r.map_err(anyhow::Error::from));
                }
            }
            // 请求期间用户可能已发起更新操作（seq 变化），丢弃过期结果
            let cur_seq = read_ui(ui, |u| u.global::<FilesG>().get_seq())
                .await
                .unwrap_or(-2);
            if seq >= 0 && cur_seq != seq {
                let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
                return false;
            }
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<FilesG>();
                match res {
                    Ok(entries) => {
                        let key = g.get_sort_key().to_string();
                        let desc = g.get_sort_desc();
                        // 走 set_file_entries：勾选数组与 entries 必须等长
                        set_file_entries(&ui, file_rows(&entries, &key, desc));
                    }
                    Err(e) => {
                        set_file_entries(&ui, Vec::<FileRow>::new());
                        let msg = crate::proxy::annotate_error(
                            &rcgui_i18n::f("列目录失败: {}", &[&(e)]),
                            proxy_ep.as_deref(),
                        );
                        toast_ui(&ui, 2, msg);
                    }
                }
                // 切换远程时 Slint 会把 path 清空，这里按当前值重建面包屑
                apply_crumbs(&ui, file_crumbs(&remote, &path));
                g.set_sel(-1);
                g.set_loading(false);
            });
            false
        }
        Cmd::FilesEnter { name, seq } => {
            let (_, path) = ui_files_pos(ui).await;
            let target = join_path(&path, &name);
            let fail = rcgui_i18n::f("无法进入 {}", &[&(name)]);
            files_navigate(ctx, ui, target, seq, fail).await;
            false
        }
        Cmd::FilesJump { path, seq } => {
            files_navigate(ctx, ui, path, seq, rcgui_i18n::t("无法打开该路径").into()).await;
            false
        }
        Cmd::FilesUp { seq } => {
            let (_, path) = ui_files_pos(ui).await;
            files_navigate(
                ctx,
                ui,
                parent_path(&path),
                seq,
                rcgui_i18n::t("返回上级失败").into(),
            )
            .await;
            false
        }
        Cmd::FilesDownload { name, local } => {
            let (remote, path) = ui_files_pos(ui).await;
            // 读同样要拦：下载拿到的是用户存在站点的真实文件，插件不在时这条通道
            // 也不该开着——否则「浏览被挡了但文件照样下得下来」才是真的怪
            if let Some(msg) = kv_access_blocked(ctx, &remote).await {
                toast(ui, 2, msg);
                return false;
            }
            // 「另存为」里用户可能改了文件名，必须按最终选择落盘
            let dst_name = local
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| name.clone());
            let client = ctx.rc.client().await;
            let fs = local_fs(local.parent().unwrap_or(Path::new(".")));
            let res = client
                .copyfile(
                    &format!("{remote}:"),
                    &join_path(&path, &name),
                    &fs,
                    &dst_name,
                )
                .await;
            match res {
                Ok(_) => toast(ui, 1, rcgui_i18n::f("已下载到 {}", &[&(local.display())])),
                Err(e) => toast(ui, 2, rcgui_i18n::f("下载失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::FilesDownloadToDir { name, dir, is_dir } => {
            let (remote, path) = ui_files_pos(ui).await;
            if remote.is_empty() {
                return false;
            }
            if let Some(msg) = kv_access_blocked(ctx, &remote).await {
                toast(ui, 2, msg);
                return false;
            }
            if is_dir {
                // 文件夹：递归复制到「所选目录/文件夹名」；同名时另起带序号的目录
                let target = unique_local_dir(&dir, &name);
                if let Err(e) = std::fs::create_dir_all(&target) {
                    toast(
                        ui,
                        2,
                        rcgui_i18n::f("无法创建本地目录 {}: {}", &[&(target.display()), &(e)]),
                    );
                    return false;
                }
                let src_fs = format!("{remote}:{}", join_path(&path, &name));
                let dst_fs = local_fs(&target);
                let ctx2 = ctx.clone();
                let ui2 = ui.clone();
                toast(ui, 0, rcgui_i18n::f("正在下载文件夹 {}…", &[&(name)]));
                // 大目录可能复制很久：后台线程执行，不阻塞命令队列
                std::thread::spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(_) => return,
                    };
                    rt.block_on(async move {
                        let client = ctx2.rc.client().await;
                        match client.sync_copy(&src_fs, &dst_fs, true).await {
                            Ok(_) => toast(
                                &ui2,
                                1,
                                rcgui_i18n::f("已下载到 {}", &[&(target.display())]),
                            ),
                            Err(e) => toast(&ui2, 2, rcgui_i18n::f("下载失败: {}", &[&(e)])),
                        }
                    });
                });
                return false;
            }
            // 文件：文件夹选择器没有原生「是否覆盖」确认，同名时自动加序号
            let local = unique_local_path(&dir, &name);
            let dst_name = local
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| name.clone());
            let client = ctx.rc.client().await;
            let fs = local_fs(local.parent().unwrap_or(Path::new(".")));
            let res = client
                .copyfile(
                    &format!("{remote}:"),
                    &join_path(&path, &name),
                    &fs,
                    &dst_name,
                )
                .await;
            match res {
                Ok(_) => toast(ui, 1, rcgui_i18n::f("已下载到 {}", &[&(local.display())])),
                Err(e) => toast(ui, 2, rcgui_i18n::f("下载失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::FilesMoveList { path, seq } => {
            let (remote, _) = ui_files_pos(ui).await;
            if remote.is_empty() {
                return false;
            }
            // 这是「移动到…」的目录选择器，走的是 DAV 列目录：不拦的话
            // 用户还能借它把整个站点目录树翻一遍。
            if let Some(msg) = kv_access_blocked(ctx, &remote).await {
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    ui.global::<FilesG>().set_move_busy(false);
                    toast_ui(&ui, 2, msg);
                });
                return false;
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                client.list(&fs, &path, None),
            )
            .await
            .map_err(|_| anyhow!("{}", rcgui_i18n::t("请求超时")))
            .and_then(|r| r.map_err(anyhow::Error::from));
            let cur_seq = read_ui(ui, |u| u.global::<FilesG>().get_move_seq())
                .await
                .unwrap_or(-2);
            if seq >= 0 && cur_seq != seq {
                let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_move_busy(false));
                return false;
            }
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<FilesG>();
                g.set_move_busy(false);
                match res {
                    Ok(entries) => {
                        let mut dirs: Vec<MoveDir> = entries
                            .iter()
                            .filter(|e| e.is_dir)
                            .map(|e| MoveDir {
                                name: e.name.clone().into(),
                            })
                            .collect();
                        dirs.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
                        g.set_move_dirs(ModelRc::from(Rc::new(VecModel::from(dirs))));
                        g.set_move_path(path.clone().into());
                    }
                    Err(e) => toast_ui(&ui, 2, rcgui_i18n::f("读取目标目录失败: {}", &[&(e)])),
                }
            });
            false
        }
        Cmd::FilesMove { name, is_dir, dest } => {
            let (remote, path) = ui_files_pos(ui).await;
            if remote.is_empty() || name.is_empty() {
                return false;
            }
            if let Some(msg) = kv_write_blocked(&ctx, &remote).await {
                toast(ui, 2, msg);
                return false;
            }
            let src = join_path(&path, &name);
            if dest == path {
                toast(ui, 2, rcgui_i18n::t("目标就是当前目录，无需移动"));
                return false;
            }
            if is_dir && is_same_or_inside(&dest, &src) {
                toast(
                    ui,
                    2,
                    rcgui_i18n::t("不能把文件夹移动到它自己或它的子目录里"),
                );
                return false;
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            // 目标目录已有同名项时拒绝，绝不悄悄覆盖用户数据
            match client.list(&fs, &dest, None).await {
                Ok(entries) => {
                    if entries.iter().any(|e| e.name.eq_ignore_ascii_case(&name)) {
                        toast(
                            ui,
                            2,
                            rcgui_i18n::f(
                                "目标目录已有同名{}，请先改名或换一个目录",
                                &[&(if is_dir {
                                    rcgui_i18n::t("文件夹")
                                } else {
                                    rcgui_i18n::t("文件")
                                })],
                            ),
                        );
                        return false;
                    }
                }
                Err(e) => {
                    toast(ui, 2, rcgui_i18n::f("无法检查目标目录: {}", &[&(e)]));
                    return false;
                }
            }
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(true));
            let dst = join_path(&dest, &name);
            let where_ = if dest.is_empty() {
                "/".to_string()
            } else {
                dest.clone()
            };
            if is_dir {
                // 目录移动可能很久（要遍历/搬大量文件）：放到后台任务，
                // 否则命令队列会被占住，连「退出」都要等它结束。
                let ctx2 = ctx.clone();
                let ui2 = ui.clone();
                let src_fs = format!("{remote}:{src}");
                let dst_fs = format!("{remote}:{dst}");
                // 先登记再动手：core/transferred 分不清目录改名与目录移动（见 OpHints）
                ctx.op_hints
                    .lock()
                    .unwrap()
                    .put(&fs, &src, rcgui_i18n::t("移动"));
                // 用「独立线程 + current_thread runtime」而不是 tokio::spawn：
                // 目录移动完成后要复用 `handle_cmd` 刷新列表，而它整体不是 Send。
                std::thread::spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(_) => return,
                    };
                    rt.block_on(async move {
                        let client = ctx2.rc.client().await;
                        let res = client.sync_move(&src_fs, &dst_fs, true, true).await;
                        let _ = ui2
                            .upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
                        match res {
                            Ok(_) => {
                                let _ = Box::pin(handle_cmd(
                                    &ctx2,
                                    &ui2,
                                    Cmd::FilesRefresh { seq: -1 },
                                ))
                                .await;
                                toast(&ui2, 1, rcgui_i18n::f("已移动到 {}", &[&(where_)]));
                            }
                            Err(e) => toast(&ui2, 2, rcgui_i18n::f("移动失败: {}", &[&(e)])),
                        }
                    });
                });
                return false;
            }
            // 先登记再动手：同一 root 下的改名与移动在记账里同形（见 OpHints）
            ctx.op_hints
                .lock()
                .unwrap()
                .put(&fs, &src, rcgui_i18n::t("移动"));
            // 文件移动通常很快，直接等待，失败可立即提示
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                client.movefile(&fs, &src, &fs, &dst),
            )
            .await
            .map_err(|_| anyhow!("{}", rcgui_i18n::t("移动超时")))
            .and_then(|r| r.map_err(anyhow::Error::from));
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
            match res {
                Ok(_) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
                    toast(ui, 1, rcgui_i18n::f("已移动到 {}", &[&(where_)]));
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("移动失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::FilesMkdir => {
            let (remote, path) = ui_files_pos(ui).await;
            if let Some(msg) = kv_write_blocked(&ctx, &remote).await {
                toast(ui, 2, msg);
                return false;
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            // 先列出当前目录，挑一个没被占用的名字。
            //
            // 旧实现固定用「新建文件夹」，目录里已存在同名文件夹时 rclone 会
            // 报 `directory already exists`，用户就只能自己先去改名再重试。
            // 现在按「新建文件夹」「新建文件夹 (1)」「新建文件夹 (2)」… 递增。
            let existing: Vec<String> = match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                client.list(&fs, &path, None),
            )
            .await
            {
                Ok(Ok(entries)) => entries.into_iter().map(|e| e.name).collect(),
                // 列目录失败不阻断创建：退回默认名，让 rclone 自己判断
                _ => Vec::new(),
            };
            let name = unique_dir_name(rcgui_i18n::t("新建文件夹"), &existing);
            let res = client.mkdir(&fs, &join_path(&path, &name)).await;
            match res {
                Ok(_) => {
                    let chosen = name.clone();
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        ui.global::<FilesG>().set_rename_name(chosen.into());
                    });
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
                    toast(
                        ui,
                        1,
                        rcgui_i18n::f("文件夹已创建：{}（选中后可重命名）", &[&(name)]),
                    );
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("创建失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::FilesDel { name, is_dir } => {
            let (remote, path) = ui_files_pos(ui).await;
            // K-Vault 远程：DAV 通道只读，但站点 API 有正经的删除端点，
            // 按站点索引翻成文件 id 再删。插件不在时这条远程不能当普通 DAV 用，
            // 所以 Orphan 分支直接拒绝，而不是让它落到下面的 `client.purge`。
            match kv_write_target(&ctx, &remote).await {
                KvWrite::Bridge(cfg) => {
                    let items = vec![(name, is_dir)];
                    return kv_delete_into(ui.clone(), cfg, path, items).await;
                }
                KvWrite::Orphan(msg) => {
                    toast(ui, 2, msg);
                    return false;
                }
                KvWrite::NotKv => {}
            }
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
                    toast(ui, 1, rcgui_i18n::f("已删除 {}", &[&(name)]));
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("删除失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::FilesRename { from, to, is_dir } => {
            if to.trim().is_empty() || to == from {
                return false;
            }
            // 只接受单层名字：带路径分隔符就不是「重命名」而是「移动」了，
            // 语义不同（移动还要检查子目录、同名冲突），这里明确拒绝。
            if to.contains('/') || to.contains('\\') {
                toast(ui, 2, rcgui_i18n::t("名称不能包含路径分隔符"));
                return false;
            }
            let (remote, path) = ui_files_pos(ui).await;
            if let Some(msg) = kv_write_blocked(&ctx, &remote).await {
                toast(ui, 2, msg);
                return false;
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            if is_dir {
                // 目录重命名：`operations/movefile` 只认文件，对目录会回
                // `is a directory not a file`，所以走目录级 `sync/move`（rclone
                // 会优先用后端的 DirMove，不行再逐个搬）。先确认新名字没被占用。
                match client.list(&fs, &path, None).await {
                    Ok(entries) => {
                        if entries.iter().any(|e| e.name.eq_ignore_ascii_case(&to)) {
                            toast(ui, 2, rcgui_i18n::f("当前目录已有同名项：{}", &[&(to)]));
                            return false;
                        }
                    }
                    Err(e) => {
                        toast(ui, 2, rcgui_i18n::f("无法检查目录内容: {}", &[&(e)]));
                        return false;
                    }
                }
                let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(true));
                let ctx2 = ctx.clone();
                let ui2 = ui.clone();
                let src_fs = format!("{remote}:{}", join_path(&path, &from));
                let dst_fs = format!("{remote}:{}", join_path(&path, &to));
                // 先登记再动手：core/transferred 分不清目录改名与目录移动（见 OpHints）
                ctx.op_hints.lock().unwrap().put(
                    &fs,
                    &join_path(&path, &from),
                    rcgui_i18n::t("重命名"),
                );
                // 与目录移动同样的理由：可能很久，放后台线程，
                // 用 current_thread runtime 复用 `handle_cmd`（它不是 Send）。
                std::thread::spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(_) => return,
                    };
                    rt.block_on(async move {
                        let client = ctx2.rc.client().await;
                        let res = client.sync_move(&src_fs, &dst_fs, true, true).await;
                        let _ = ui2
                            .upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
                        match res {
                            Ok(_) => {
                                let _ = ui2.upgrade_in_event_loop(|ui| {
                                    ui.global::<FilesG>().set_sel(-1);
                                });
                                let _ = Box::pin(handle_cmd(
                                    &ctx2,
                                    &ui2,
                                    Cmd::FilesRefresh { seq: -1 },
                                ))
                                .await;
                                toast(&ui2, 1, rcgui_i18n::t("重命名完成"));
                            }
                            Err(e) => toast(&ui2, 2, rcgui_i18n::f("重命名失败: {}", &[&(e)])),
                        }
                    });
                });
                return false;
            }
            // 先登记再动手：同一 root 下的改名与移动在记账里同形（见 OpHints）
            ctx.op_hints.lock().unwrap().put(
                &fs,
                &join_path(&path, &from),
                rcgui_i18n::t("重命名"),
            );
            let res = client
                .movefile(&fs, &join_path(&path, &from), &fs, &join_path(&path, &to))
                .await;
            match res {
                Ok(_) => {
                    let _ = ui.upgrade_in_event_loop(|ui| {
                        ui.global::<FilesG>().set_sel(-1);
                    });
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
                    toast(ui, 1, rcgui_i18n::t("重命名完成"));
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("重命名失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::FilesCopy { name, is_dir } => {
            let (remote, path) = ui_files_pos(ui).await;
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let entries = client.list(&fs, &path, None).await.unwrap_or_default();
            if is_dir {
                // 目录复制：递归 `sync/copy`，同样可能很久，放后台线程。
                let existing: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                let cand = unique_dir_name(&name, &existing);
                let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(true));
                let ctx2 = ctx.clone();
                let ui2 = ui.clone();
                let src_fs = format!("{remote}:{}", join_path(&path, &name));
                let dst_fs = format!("{remote}:{}", join_path(&path, &cand));
                let cand2 = cand.clone();
                std::thread::spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(_) => return,
                    };
                    rt.block_on(async move {
                        let client = ctx2.rc.client().await;
                        let res = client.sync_copy(&src_fs, &dst_fs, true).await;
                        let _ = ui2
                            .upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_loading(false));
                        match res {
                            Ok(_) => {
                                let _ = Box::pin(handle_cmd(
                                    &ctx2,
                                    &ui2,
                                    Cmd::FilesRefresh { seq: -1 },
                                ))
                                .await;
                                toast(&ui2, 1, rcgui_i18n::f("已复制为 {}", &[&(cand2)]));
                            }
                            Err(e) => toast(&ui2, 2, rcgui_i18n::f("复制失败: {}", &[&(e)])),
                        }
                    });
                });
                return false;
            }
            let (base, ext) = split_name(&name);
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
                    toast(ui, 1, rcgui_i18n::f("已复制为 {}", &[&(cand)]));
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("复制失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::FilesSort { key } => {
            // 排序：rclone 侧只给一次目录快照，本地不缓存，所以改完排序键重新列一次目录。
            // 先 read_ui 拿到当前状态再写，保证 set 一定先于 FilesRefresh 里的读取。
            let (same, desc) = read_ui(ui, {
                let key_probe = key.clone();
                move |ui| {
                    let g = ui.global::<FilesG>();
                    (
                        g.get_sort_key().as_str() == key_probe.as_str(),
                        g.get_sort_desc(),
                    )
                }
            })
            .await
            .unwrap_or((false, false));
            let new_desc = if same { !desc } else { false };
            let key2 = key.clone();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<FilesG>();
                g.set_sort_key(key2.into());
                g.set_sort_desc(new_desc);
            });
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
            false
        }
        Cmd::FilesUpload { paths, dest } => {
            // 上传本机文件到当前远程目录（请求 ③④ 的落点）。
            //
            // 与下载方向相反，但 copyfile 的用法完全对称：
            //   本机 fs = 选中文件的父目录 + 文件名，远程 fs = "remote:当前目录" + 目标名。
            // 同名一律改名（`a.txt` → `a (1).txt`）：copyfile 默认直接覆盖，
            // 而「把新版本传上去」是最常见的同名场景，静默覆盖不是安全的默认行为。
            let (remote, mut path) = ui_files_pos(ui).await;
            if remote.is_empty() {
                toast(ui, 2, rcgui_i18n::t("请先选择远程仓库"));
                return false;
            }
            if paths.is_empty() {
                return false;
            }
            // 「上传到…」指定了目标目录：用它的路径，但仍留在当前列表里（不跳转）
            if let Some(d) = dest {
                path = d;
            }
            // K-Vault 远程的写入不走 rclone（DAV 只读，写方法一律 403），改发站点 API。
            // 插件不在时这条远程也不能当普通 DAV 用：拒绝并说明，别让用户去猜那个 403。
            match kv_write_target(&ctx, &remote).await {
                KvWrite::Bridge(cfg) => {
                    return Box::pin(kv_upload_into(ctx.clone(), ui.clone(), cfg, paths, &path))
                        .await;
                }
                KvWrite::Orphan(msg) => {
                    toast(ui, 2, msg);
                    return false;
                }
                KvWrite::NotKv => {}
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            // 目标目录的 fs 直接写成 "remote:path"（path 为空就是远程根）
            let dst_fs = format!("{remote}:{path}");
            let entries = client.list(&fs, &path, None).await.unwrap_or_default();
            let mut existing: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
            let total = paths.len();
            let mut ok = 0usize;
            let mut renamed = 0usize;
            let mut failed: Vec<String> = Vec::new();
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_busy(true));
            for p in &paths {
                let base = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if base.is_empty() {
                    continue;
                }
                let cand = unique_file_name(&base, &existing);
                if cand != base {
                    renamed += 1;
                }
                existing.push(cand.clone());
                let src_fs = local_fs(p.parent().unwrap_or(Path::new(".")));
                // 本机拖进来/选中的可能是文件夹：copyfile 只收单个文件，传目录必失败，
                // 所以目录走 sync/copy 递归复制（拖一个文件夹上去 = 整个文件夹）。
                // sync/* 没有 srcRemote/dstRemote，目标路径要直接拼进 fs 串。
                let res = if p.is_dir() {
                    let dir_dst = format!("{remote}:{}", join_path(&path, &cand));
                    client.sync_copy(&local_fs(p), &dir_dst, true).await
                } else {
                    client.copyfile(&src_fs, &base, &dst_fs, &cand).await
                };
                match res {
                    Ok(_) => ok += 1,
                    Err(e) => failed.push(format!("{base}: {e}")),
                }
            }
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_busy(false));
            if ok > 0 {
                let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
            }
            let where_ = if path.is_empty() {
                "/".to_string()
            } else {
                path.clone()
            };
            if failed.is_empty() {
                let extra = if renamed > 0 {
                    rcgui_i18n::f("（{} 个同名文件已自动改名）", &[&(renamed)])
                } else {
                    String::new()
                };
                toast(
                    ui,
                    1,
                    rcgui_i18n::f(
                        "已上传 {}/{} 项到 {}{}",
                        &[&(ok), &(total), &(where_), &(extra)],
                    ),
                );
            } else {
                toast(
                    ui,
                    2,
                    rcgui_i18n::f(
                        "上传完成 {}/{}，失败 {}：{}",
                        &[&(ok), &(total), &(failed.len()), &(failed.join("；"))],
                    ),
                );
            }
            false
        }
        Cmd::FilesCheck { idx, shift } => {
            // 勾选 / 取消勾选一行；按住 Shift 时勾选「上次点击行 → 本行」整段。
            // 勾选态重建整个数组（行数通常只有几百，重建比 downcast 取值更稳）。
            let (n, anchor, cur) = read_ui(ui, move |ui| {
                let g = ui.global::<FilesG>();
                let cur = g
                    .get_checked()
                    .row_data(idx.max(0) as usize)
                    .unwrap_or(false);
                (g.get_entries().row_count() as i32, g.get_anchor(), cur)
            })
            .await
            .unwrap_or((0, -1, false));
            if idx < 0 || idx >= n {
                return false;
            }
            let (lo, hi, value, new_anchor) = if shift && anchor >= 0 && anchor < n {
                (anchor.min(idx), anchor.max(idx), true, anchor)
            } else {
                (idx, idx, !cur, idx)
            };
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<FilesG>();
                let mut flags: Vec<bool> = (0..n as usize)
                    .map(|i| g.get_checked().row_data(i).unwrap_or(false))
                    .collect();
                for i in lo..=hi {
                    flags[i as usize] = value;
                }
                let hits = flags.iter().filter(|v| **v).count() as i32;
                g.set_checked(ModelRc::from(Rc::new(VecModel::from(flags))));
                g.set_checked_count(hits);
                g.set_anchor(new_anchor);
            });
            false
        }
        Cmd::FilesSelect { mode } => {
            // mode: 0=清空 1=全选 2=反选 3=进入/退出多选模式
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<FilesG>();
                let n = g.get_entries().row_count();
                match mode {
                    3 => {
                        let on = !g.get_multi();
                        g.set_multi(on);
                        // 退出多选时清掉勾选：留着看不见的勾选，下次进入会莫名多选
                        if !on {
                            g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![false; n]))));
                            g.set_checked_count(0);
                            g.set_anchor(-1);
                        }
                    }
                    0 => {
                        // 长度必须与 entries 一致：Slint 侧按 checked[i] 取值
                        g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![false; n]))));
                        g.set_checked_count(0);
                        g.set_anchor(-1);
                    }
                    1 => {
                        g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![true; n]))));
                        g.set_checked_count(n as i32);
                    }
                    _ => {
                        let flags: Vec<bool> = (0..n)
                            .map(|i| !g.get_checked().row_data(i).unwrap_or(false))
                            .collect();
                        let hits = flags.iter().filter(|v| **v).count() as i32;
                        g.set_checked(ModelRc::from(Rc::new(VecModel::from(flags))));
                        g.set_checked_count(hits);
                    }
                }
            });
            false
        }
        Cmd::FilesDownloadMany { items, dir } => {
            // 批量下载（多选模式）：文件走 copyfile，文件夹走递归 sync/copy 并另起序号目录。
            // 目录可能很大 → 专用线程 + current_thread 运行时（handle_cmd 不是 Send）。
            if items.is_empty() {
                return false;
            }
            let (remote, path) = ui_files_pos(ui).await;
            if remote.is_empty() {
                return false;
            }
            if let Some(msg) = kv_access_blocked(ctx, &remote).await {
                toast(ui, 2, msg);
                return false;
            }
            let total = items.len();
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_busy(true));
            let ctx2 = ctx.clone();
            let ui2 = ui.clone();
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return,
                };
                rt.block_on(async move {
                    let client = ctx2.rc.client().await;
                    let fs = format!("{remote}:");
                    let mut ok = 0usize;
                    let mut failed: Vec<String> = Vec::new();
                    for (name, is_dir) in &items {
                        let src = join_path(&path, name);
                        let res = if *is_dir {
                            let target = unique_local_dir(&dir, name);
                            if let Err(e) = std::fs::create_dir_all(&target) {
                                failed.push(rcgui_i18n::f(
                                    "{}: 无法创建本地目录 {}",
                                    &[&(name), &(e)],
                                ));
                                continue;
                            }
                            toast(&ui2, 0, rcgui_i18n::f("正在下载文件夹 {}…", &[&(name)]));
                            client
                                .sync_copy(&format!("{fs}{src}"), &local_fs(&target), true)
                                .await
                        } else {
                            let local = unique_local_path(&dir, name);
                            let dst_name = local
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| name.clone());
                            client
                                .copyfile(
                                    &fs,
                                    &src,
                                    &local_fs(local.parent().unwrap_or(Path::new("."))),
                                    &dst_name,
                                )
                                .await
                        };
                        match res {
                            Ok(_) => ok += 1,
                            Err(e) => failed.push(format!("{name}: {e}")),
                        }
                    }
                    let _ = ui2.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_busy(false));
                    if failed.is_empty() {
                        toast(
                            &ui2,
                            1,
                            rcgui_i18n::f(
                                "已下载 {}/{} 项到 {}",
                                &[&(ok), &(total), &(dir.display())],
                            ),
                        );
                    } else {
                        toast(
                            &ui2,
                            2,
                            rcgui_i18n::f(
                                "下载完成 {}/{}，失败 {}：{}",
                                &[&(ok), &(total), &(failed.len()), &(failed.join("；"))],
                            ),
                        );
                    }
                });
            });
            false
        }
        Cmd::FilesDelMany { items } => {
            // 批量删除（多选模式）：删除前已在界面上确认过，这里逐个执行并汇总结果。
            if items.is_empty() {
                return false;
            }
            let (remote, path) = ui_files_pos(ui).await;
            if remote.is_empty() {
                return false;
            }
            // K-Vault 远程：走站点 API 删除（DAV 通道只读），逐项翻 id。
            // 插件不在时同样拒绝，不让这批删除落到 rclone 的 DAV 写方法上。
            match kv_write_target(&ctx, &remote).await {
                KvWrite::Bridge(cfg) => {
                    return kv_delete_into(ui.clone(), cfg, path, items).await;
                }
                KvWrite::Orphan(msg) => {
                    toast(ui, 2, msg);
                    return false;
                }
                KvWrite::NotKv => {}
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let total = items.len();
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_busy(true));
            let mut ok = 0usize;
            let mut failed: Vec<String> = Vec::new();
            for (name, is_dir) in &items {
                let full = join_path(&path, name);
                let res = if *is_dir {
                    client.purge(&fs, &full).await
                } else {
                    client.deletefile(&fs, &full).await
                };
                match res {
                    Ok(_) => ok += 1,
                    Err(e) => failed.push(format!("{name}: {e}")),
                }
            }
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<FilesG>().set_busy(false));
            // 刷新会重建 entries 并清空勾选（此时仍留在多选模式，方便继续操作）
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
            if failed.is_empty() {
                toast(ui, 1, rcgui_i18n::f("已删除 {} 项", &[&(ok)]));
            } else {
                toast(
                    ui,
                    2,
                    rcgui_i18n::f(
                        "删除完成 {}/{}，失败 {}：{}",
                        &[&(ok), &(total), &(failed.len()), &(failed.join("；"))],
                    ),
                );
            }
            false
        }
        Cmd::LocalRefresh { path } => {
            // 本机窗格：读一层目录。纯本机操作（std::fs），不发任何 RC 请求。
            // 排序偏好和路径一起读：一次 read_ui 拿到，避免额外往返。
            let (cur, sort_key, sort_desc) = read_ui(ui, |ui| {
                let g = ui.global::<LocalG>();
                (
                    g.get_path().to_string(),
                    g.get_sort_key().to_string(),
                    g.get_sort_desc(),
                )
            })
            .await
            .unwrap_or_default();
            let cur = match path {
                Some(p) => p,
                None => cur,
            };
            // 首次进入没有路径：从用户主目录开始（比 C:\ 更接近用户想传的东西）
            let cur = if cur.trim().is_empty() {
                std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
            } else {
                cur
            };
            let res = local_rows(Path::new(&cur), &sort_key, sort_desc);
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<LocalG>();
                match res {
                    // 轻量模式期间不写本地列表：界面不在屏幕上，写进去只是把刚清掉的
                    // 行重新装回内存；退出轻量模式时 `LocalRefresh` 会重取一遍。
                    Ok(rows) if crate::lightweight::suppress_rows(rows.len()) => {}
                    Ok(rows) => {
                        g.set_path(cur.clone().into());
                        // 与远程侧同理：勾选数组按行数重建，否则 Slint 的 checked[i] 越界
                        let n = rows.len();
                        g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![false; n]))));
                        g.set_checked_count(0);
                        g.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
                        g.set_sel(-1);
                    }
                    Err(e) => {
                        // 路径无效 / 无权限：保留旧列表，只提示，不清空（避免界面突然空掉）
                        toast_ui(&ui, 2, rcgui_i18n::f("读取本机目录失败: {}", &[&(e)]));
                    }
                }
                g.set_loading(false);
            });
            false
        }
        Cmd::LocalEnter { name } => {
            let cur = read_ui(ui, |ui| ui.global::<LocalG>().get_path().to_string())
                .await
                .unwrap_or_default();
            if cur.trim().is_empty() {
                return false;
            }
            let next = Path::new(&cur).join(&name);
            let _ = Box::pin(handle_cmd(
                ctx,
                ui,
                Cmd::LocalRefresh {
                    path: Some(next.to_string_lossy().to_string()),
                },
            ))
            .await;
            false
        }
        Cmd::LocalUp => {
            let cur = read_ui(ui, |ui| ui.global::<LocalG>().get_path().to_string())
                .await
                .unwrap_or_default();
            let up = Path::new(&cur)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(cur);
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::LocalRefresh { path: Some(up) })).await;
            false
        }
        Cmd::LocalChooseDir => {
            if let Some(dir) = rfd::FileDialog::new()
                .set_title(rcgui_i18n::t("选择本机目录"))
                .pick_folder()
            {
                let _ = Box::pin(handle_cmd(
                    ctx,
                    ui,
                    Cmd::LocalRefresh {
                        path: Some(dir.to_string_lossy().to_string()),
                    },
                ))
                .await;
            }
            false
        }
        Cmd::LocalUpload { name, is_dir } => {
            // 本机这一项 → 远程当前目录：文件用 copyfile，目录递归 sync/copy 并另起不重名目录。
            let (remote, path) = ui_files_pos(ui).await;
            if remote.is_empty() {
                toast(ui, 2, rcgui_i18n::t("请先选择远程仓库"));
                return false;
            }
            let cur = read_ui(ui, |ui| ui.global::<LocalG>().get_path().to_string())
                .await
                .unwrap_or_default();
            if cur.trim().is_empty() {
                return false;
            }
            let src = Path::new(&cur).join(&name);
            // K-Vault 远程：写入走站点 API，不经 rclone 的 DAV 通道（只读）。
            // 插件不在时同样拒绝，别让这次拷贝落到 DAV 的写方法上。
            match kv_write_target(&ctx, &remote).await {
                KvWrite::Bridge(cfg) => {
                    return Box::pin(kv_upload_into(
                        ctx.clone(),
                        ui.clone(),
                        cfg,
                        vec![src],
                        &path,
                    ))
                    .await;
                }
                KvWrite::Orphan(msg) => {
                    toast(ui, 2, msg);
                    return false;
                }
                KvWrite::NotKv => {}
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let entries = client.list(&fs, &path, None).await.unwrap_or_default();
            let existing: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
            if is_dir {
                let cand = unique_dir_name(&name, &existing);
                let dst_fs = format!("{remote}:{}", join_path(&path, &cand));
                let src_fs = local_fs(&src);
                let _ = ui.upgrade_in_event_loop(|ui| ui.global::<LocalG>().set_busy(true));
                toast(ui, 0, rcgui_i18n::f("正在上传文件夹 {}…", &[&(name)]));
                let ctx2 = ctx.clone();
                let ui2 = ui.clone();
                // 大目录可能很久：专用线程 + current_thread 运行时
                std::thread::spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(_) => return,
                    };
                    rt.block_on(async move {
                        let client = ctx2.rc.client().await;
                        let res = client.sync_copy(&src_fs, &dst_fs, true).await;
                        let _ =
                            ui2.upgrade_in_event_loop(|ui| ui.global::<LocalG>().set_busy(false));
                        match res {
                            Ok(_) => {
                                let _ = Box::pin(handle_cmd(
                                    &ctx2,
                                    &ui2,
                                    Cmd::FilesRefresh { seq: -1 },
                                ))
                                .await;
                                toast(&ui2, 1, rcgui_i18n::f("已上传文件夹 {}", &[&(cand)]));
                            }
                            Err(e) => toast(&ui2, 2, rcgui_i18n::f("上传失败: {}", &[&(e)])),
                        }
                    });
                });
                return false;
            }
            let cand = unique_file_name(&name, &existing);
            let dst_fs = format!("{remote}:{path}");
            let src_fs = local_fs(src.parent().unwrap_or(Path::new(".")));
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<LocalG>().set_busy(true));
            let res = client.copyfile(&src_fs, &name, &dst_fs, &cand).await;
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<LocalG>().set_busy(false));
            match res {
                Ok(_) => {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
                    let extra = if cand != name {
                        rcgui_i18n::t("（同名已自动改名）")
                    } else {
                        ""
                    };
                    toast(ui, 1, rcgui_i18n::f("已上传 {}{}", &[&(cand), &(extra)]));
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("上传失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::LocalCheck { idx, shift } => {
            // 本机多选勾选。逻辑与远程侧 FilesCheck 完全一致，只是换成 LocalG。
            let (n, anchor, cur) = read_ui(ui, move |ui| {
                let g = ui.global::<LocalG>();
                let cur = g
                    .get_checked()
                    .row_data(idx.max(0) as usize)
                    .unwrap_or(false);
                (g.get_entries().row_count() as i32, g.get_anchor(), cur)
            })
            .await
            .unwrap_or((0, -1, false));
            if idx < 0 || idx >= n {
                return false;
            }
            let (lo, hi, value, new_anchor) = if shift && anchor >= 0 && anchor < n {
                (anchor.min(idx), anchor.max(idx), true, anchor)
            } else {
                (idx, idx, !cur, idx)
            };
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<LocalG>();
                let mut flags: Vec<bool> = (0..n as usize)
                    .map(|i| g.get_checked().row_data(i).unwrap_or(false))
                    .collect();
                for i in lo..=hi {
                    flags[i as usize] = value;
                }
                let hits = flags.iter().filter(|v| **v).count() as i32;
                g.set_checked(ModelRc::from(Rc::new(VecModel::from(flags))));
                g.set_checked_count(hits);
                g.set_anchor(new_anchor);
            });
            false
        }
        Cmd::LocalSelect { mode } => {
            // mode: 0=清空 1=全选 2=反选 3=进入/退出本机多选模式
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<LocalG>();
                let n = g.get_entries().row_count();
                match mode {
                    3 => {
                        let on = !g.get_multi();
                        g.set_multi(on);
                        if !on {
                            g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![false; n]))));
                            g.set_checked_count(0);
                            g.set_anchor(-1);
                        }
                    }
                    0 => {
                        g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![false; n]))));
                        g.set_checked_count(0);
                        g.set_anchor(-1);
                    }
                    1 => {
                        g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![true; n]))));
                        g.set_checked_count(n as i32);
                    }
                    _ => {
                        let flags: Vec<bool> = (0..n)
                            .map(|i| !g.get_checked().row_data(i).unwrap_or(false))
                            .collect();
                        let hits = flags.iter().filter(|v| **v).count() as i32;
                        g.set_checked(ModelRc::from(Rc::new(VecModel::from(flags))));
                        g.set_checked_count(hits);
                    }
                }
            });
            false
        }
        Cmd::LocalSort { key } => {
            // 与远程侧 FilesSort 同规则：点同一个键切换升降序，换键则回到升序。
            let (same, desc) = read_ui(ui, {
                let key = key.clone();
                move |ui| {
                    let g = ui.global::<LocalG>();
                    (g.get_sort_key() == key, g.get_sort_desc())
                }
            })
            .await
            .unwrap_or((false, false));
            let new_desc = if same { !desc } else { false };
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<LocalG>();
                g.set_sort_key(key.into());
                g.set_sort_desc(new_desc);
            });
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::LocalRefresh { path: None })).await;
            false
        }
        Cmd::LocalUploadToList { path, seq } => {
            // 「上传到…」对话框：只列远程的子文件夹（和 FilesMoveList 一个套路）。
            let (remote, _) = ui_files_pos(ui).await;
            if remote.is_empty() {
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    ui.global::<LocalG>().set_up_busy(false);
                });
                return false;
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<LocalG>().set_up_busy(true));
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                client.list(&fs, &path, None),
            )
            .await;
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<LocalG>();
                // seq 竞态：用户连点几次目录时，只应用最后一次的结果
                if g.get_up_seq() != seq {
                    return;
                }
                g.set_up_busy(false);
                match res {
                    Ok(Ok(items)) => {
                        let dirs: Vec<MoveDir> = items
                            .into_iter()
                            .filter(|e| e.is_dir)
                            .map(|e| MoveDir {
                                name: e.name.into(),
                            })
                            .collect();
                        let mut dirs = dirs;
                        dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                        g.set_up_dirs(ModelRc::from(Rc::new(VecModel::from(dirs))));
                        g.set_up_path(path.clone().into());
                    }
                    Ok(Err(e)) => toast_ui(&ui, 2, rcgui_i18n::f("读取远程目录失败: {}", &[&(e)])),
                    Err(_) => toast_ui(&ui, 2, rcgui_i18n::t("读取远程目录超时")),
                }
            });
            false
        }
        Cmd::LocalUploadTo { dest } => {
            // 「上传到…」：把本机勾选项（没有勾选时是右键那一项）传到指定远程目录。
            let (remote, _) = ui_files_pos(ui).await;
            if remote.is_empty() {
                toast(ui, 2, rcgui_i18n::t("请先选择远程仓库"));
                return false;
            }
            let (cur, items) = read_ui(ui, |ui| {
                let g = ui.global::<LocalG>();
                let entries = g.get_entries();
                let checked = g.get_checked();
                // 不看 multi：右键单项时回调已经把它勾上了，勾选态就是唯一的数据来源
                let mut out: Vec<(String, bool)> = Vec::new();
                for i in 0..entries.row_count() {
                    if !checked.row_data(i).unwrap_or(false) {
                        continue;
                    }
                    if let Some(r) = entries.row_data(i) {
                        out.push((r.name.to_string(), r.is_dir));
                    }
                }
                (g.get_path().to_string(), out)
            })
            .await
            .unwrap_or_default();
            if cur.trim().is_empty() || items.is_empty() {
                return false;
            }
            // K-Vault 远程：写入走站点 API，不经 rclone 的只读 DAV 通道。
            // 插件不在时同样拒绝：这批上传不能落到 DAV 的写方法上。
            match kv_write_target(&ctx, &remote).await {
                KvWrite::Bridge(cfg) => {
                    let paths: Vec<PathBuf> =
                        items.iter().map(|(n, _)| Path::new(&cur).join(n)).collect();
                    let ret =
                        Box::pin(kv_upload_into(ctx.clone(), ui.clone(), cfg, paths, &dest)).await;
                    // 传完退出多选并清空勾选（与下面 rclone 分支的处理一致）
                    let _ = ui.upgrade_in_event_loop(|ui| {
                        let g = ui.global::<LocalG>();
                        let n = g.get_entries().row_count();
                        g.set_multi(false);
                        g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![false; n]))));
                        g.set_checked_count(0);
                        g.set_anchor(-1);
                    });
                    return ret;
                }
                KvWrite::Orphan(msg) => {
                    toast(ui, 2, msg);
                    return false;
                }
                KvWrite::NotKv => {}
            }
            let client = ctx.rc.client().await;
            let fs = format!("{remote}:");
            let entries = client.list(&fs, &dest, None).await.unwrap_or_default();
            let mut existing: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
            let total = items.len();
            let mut ok = 0usize;
            let mut failed: Vec<String> = Vec::new();
            let _ = ui.upgrade_in_event_loop(|ui| ui.global::<LocalG>().set_busy(true));
            for (name, is_dir) in &items {
                let src = Path::new(&cur).join(name);
                let cand = if *is_dir {
                    unique_dir_name(name, &existing)
                } else {
                    unique_file_name(name, &existing)
                };
                existing.push(cand.clone());
                let dst_fs = format!("{remote}:{}", join_path(&dest, &cand));
                let src_fs = if *is_dir {
                    local_fs(&src)
                } else {
                    local_fs(src.parent().unwrap_or(Path::new(".")))
                };
                let res = if *is_dir {
                    client.sync_copy(&src_fs, &dst_fs, true).await
                } else {
                    client.copyfile(&src_fs, name, &dst_fs, &cand).await
                };
                match res {
                    Ok(_) => ok += 1,
                    Err(e) => failed.push(format!("{name}: {e}")),
                }
            }
            let _ = ui.upgrade_in_event_loop(|ui| {
                let g = ui.global::<LocalG>();
                let n = g.get_entries().row_count();
                g.set_busy(false);
                // 上传完成退出多选并清空勾选（与远程侧批量删除后的处理一致）
                g.set_multi(false);
                g.set_checked(ModelRc::from(Rc::new(VecModel::from(vec![false; n]))));
                g.set_checked_count(0);
                g.set_anchor(-1);
            });
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::FilesRefresh { seq: -1 })).await;
            if failed.is_empty() {
                toast(
                    ui,
                    1,
                    rcgui_i18n::f("已上传 {}/{} 项到 {}", &[&(ok), &(total), &(dest)]),
                );
            } else {
                toast(
                    ui,
                    2,
                    rcgui_i18n::f(
                        "上传完成 {}/{}，失败 {}：{}",
                        &[&(ok), &(total), &(failed.len()), &(failed.join("；"))],
                    ),
                );
            }
            false
        }
        Cmd::MountsRefresh => {
            // 刷新 = 「现在就去 rclone 取一份状态并重画」。
            // 它和轮询共用同一条路径，所以计数（仪表盘 / 状态栏）与挂载页列表
            // 永远出自同一份快照，不会再出现「列表 1 条、计数 0」。
            refresh::refresh_state(ctx, ui, true).await;
            false
        }
        Cmd::WinFspInstall { mirror } => {
            // 源名现读配置：设置页改完立刻生效，不用重启程序
            let src = config::load().winget_mirror_source;
            install_winfsp(ui, mirror, src).await;
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
                (remote, m.get_sub_path().to_string(), point, ro, cache)
            })
            .await
            .unwrap_or_default();
            if remote.is_empty() {
                toast(ui, 2, rcgui_i18n::t("请先在「远程」页添加仓库，再选择仓库"));
                return false;
            }
            // 挂载就是「把这个远程当成本地盘用」，插件不在时挂上去只会立刻 403，
            // 而且会在资源管理器里留下一个打不开的盘符；先拒绝并说明。
            // 已有的自动挂载条目（`cfg.auto_mounts`）不动，装回插件下次启动照旧恢复。
            if let Some(msg) = kv_access_blocked(ctx, &remote).await {
                toast(ui, 2, msg);
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
                    toast(
                        ui,
                        2,
                        rcgui_i18n::f("挂载点 {} 已被占用，请修改盘符", &[&(point)]),
                    );
                    return false;
                }
            }
            let client = ctx.rc.client().await;
            let opts = json!({
                "vfs_cache_mode": cache,
                "read_only": ro,
            });
            // 立即在“活动挂载”里插入一条 pending（spinner + 高亮）
            ctx.pending_mounts
                .lock()
                .unwrap()
                .push((fs.clone(), point.clone()));
            // 每次新的挂载尝试都要清掉该盘符的「已取消」标记，
            // 否则上一次卸载留下的标记会让本次等待循环直接静默退出
            ctx.cancelled_mounts
                .lock()
                .unwrap()
                .retain(|k| k != &mount_key(&point));
            // busy 由守卫托管：函数内任何 return / panic 都会复位，避免按钮永久灰掉
            let _busy = BusyGuard::new(ui, BusyKind::Mounts);
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
            // 给足时间：RC 调用会阻塞到挂载完成（网盘首次挂载可能 15~30s 甚至更久）
            let res = tokio::time::timeout(MOUNT_RC_TIMEOUT, client.mount(&fs, &point, opts))
                .await
                .map_err(|_| {
                    anyhow!(
                        "{}",
                        rcgui_i18n::f("挂载超时（{}s）", &[&(MOUNT_RC_TIMEOUT.as_secs())])
                    )
                })
                .and_then(|r| r.map_err(anyhow::Error::from));
            // 无论成败都先摘掉 pending，并立刻重绘一次：
            // 活动挂载列表是靠 `pending` 渲染 spinner 的，只从 ctx.pending_mounts 里删掉
            // 而不刷新 UI，界面就会一直停在那行 spinner 上
            // （切到别的页面再切回来之所以正常，是因为切换本身会再发一次 MountsRefresh）。
            ctx.pending_mounts
                .lock()
                .unwrap()
                .retain(|(_, p)| p != &point);
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
            // rclone 的 mount/mount 返回成功只代表「已受理」，WinFsp 真正挂上盘符是异步的。
            // 必须确认挂载点真的出现，否则会出现「提示挂载成功但资源管理器里没有盘」。
            let res = match res {
                Ok(actual) => {
                    if wait_mount_ready_rc(
                        &client,
                        &actual,
                        MOUNT_READY_TIMEOUT,
                        &ctx.cancelled_mounts,
                    )
                    .await
                    {
                        Ok(actual)
                    } else if mount_was_cancelled(ctx, &actual) {
                        // 等待期间用户自己卸载了：不报错、不记入档案，直接安静结束
                        return false;
                    } else {
                        // 交给统一的诊断，区分「没装 WinFsp」与「盘符被占/仍在挂载」。
                        // 注意这里返回前也已经刷新过列表，不会再留下 spinner。
                        report_mount_not_visible(ui, &fs, &actual).await;
                        return false;
                    }
                }
                Err(e) => {
                    // 报错但盘符其实已经挂上：按成功处理，避免误导
                    if wait_mount_ready_rc(
                        &client,
                        &point,
                        MOUNT_READY_TIMEOUT,
                        &ctx.cancelled_mounts,
                    )
                    .await
                    {
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
                            &mut cfg, &remote, &sub_path, &actual, ro, &cache,
                        ) {
                            toast(ui, 2, format!("{e}"));
                        }
                    }
                    // 挂载已成功：清掉该盘符可能残留的「已取消」标记，
                    // 避免影响之后同一盘符的等待循环
                    ctx.cancelled_mounts
                        .lock()
                        .unwrap()
                        .retain(|k| k != &mount_key(&actual));
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    toast(ui, 1, rcgui_i18n::f("挂载成功：{}", &[&(actual)]));
                }
                Err(e) => {
                    // 失败：刷新列表（pending 已摘除，条目自动消失）
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    toast(ui, 2, rcgui_i18n::f("挂载失败：{}", &[&(e)]));
                }
            }
            false
        }
        Cmd::MountRemove { point } => {
            // 先打「已取消」标记：此刻可能还有一个 MountAdd 的等待循环在跑，
            // 它看到该盘符从 rclone 列表里消失后，必须知道这是用户主动撤掉的，
            // 而不是挂载失败，否则会在约一秒后弹出「未生效」的假错误。
            {
                let key = mount_key(&point);
                let mut c = ctx.cancelled_mounts.lock().unwrap();
                if !c.iter().any(|k| k == &key) {
                    c.push(key);
                }
                // 只保留最近的若干条，避免长期运行无限增长
                if c.len() > 32 {
                    let drain = c.len() - 32;
                    c.drain(0..drain);
                }
            }
            let client = ctx.rc.client().await;
            let _busy = BusyGuard::new(ui, BusyKind::Mounts);
            let res = client.unmount(&point).await;
            match res {
                Ok(_) => {
                    // 用户主动卸载 → 忘掉这条记忆，避免下次连接又挂回来
                    {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        persist::forget_mount(&mut cfg, &point);
                        let _ = config::save(&cfg);
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    toast(ui, 1, rcgui_i18n::f("已卸载 {}", &[&(point)]));
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("卸载失败: {}", &[&(e)])),
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
                // 由「仓库 + 子路径」拼出当前显示路径 root://remote/sub
                if scope == 2 {
                    // 同步任务的云端目录：字段里存的是 "远程名:子路径"，转成浏览用的
                    // 显示路径。空值从根开始选。
                    let fs = ui.global::<MaintenanceG>().get_sync_remote().to_string();
                    if fs.trim().is_empty() {
                        String::new()
                    } else {
                        fs_to_display(fs.trim())
                    }
                } else if scope == 0 {
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
            let start = if cur.trim().is_empty() {
                "root://".to_string()
            } else {
                cur
            };
            let (items, disp) = list_display_checked(ctx, &start, scope == 2).await;
            let items = match items {
                Ok(v) => v,
                Err(why) if scope == 2 => {
                    // 同步的云端目录打不开：把原因写回卡片，别弹一个空窗让人以为目录是空的
                    maintenance::set_sync_status(ui, rcgui_i18n::f("打不开这个位置：{}", &[&why]));
                    return false;
                }
                // 挂载和服务的浏览窗一直把失败当空目录显示，这里保持原样，别顺手改行为
                Err(_) => vec![],
            };
            let rows: Vec<PathItem> = items
                .into_iter()
                .map(|(n, d)| PathItem {
                    name: n.into(),
                    is_dir: d,
                })
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
            // scope 决定只列文件夹还是连文件一起列：同步任务的云端目录只能选目录，
            // 选了文件也没法当目标。
            let (path, scope) = read_ui(ui, |ui| {
                let g = ui.global::<BrowseG>();
                (g.get_path().to_string(), g.get_scope())
            })
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
            let (items, disp) = list_display_checked(ctx, &target, scope == 2).await;
            let items = match items {
                Ok(v) => v,
                Err(why) if scope == 2 => {
                    // 弹窗里没有地方放错误文字，就关掉它把原因写到卡片上
                    maintenance::set_sync_status(ui, rcgui_i18n::f("打不开这个位置：{}", &[&why]));
                    let _ = ui.upgrade_in_event_loop(|ui| ui.global::<BrowseG>().set_show(false));
                    return false;
                }
                Err(_) => vec![],
            };
            let rows: Vec<PathItem> = items
                .into_iter()
                .map(|(n, d)| PathItem {
                    name: n.into(),
                    is_dir: d,
                })
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
                (
                    g.get_scope(),
                    g.get_path().to_string(),
                    g.get_sel(),
                    entries,
                )
            })
            .await
            .unwrap_or_default();
            let base = path.trim().trim_end_matches('/');
            let is_root = base == "root:" || base == "root://" || base == "root" || base.is_empty();
            let picked = if sel >= 0 {
                entries.get(sel as usize)
            } else {
                None
            };
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
            if scope == 2 {
                // 同步任务的云端目录要写成 "远程名:子路径" 并存进配置，先验一次再写
                return browse_ok_sync(ctx, ui, &final_disp).await;
            }
            let _ = ui.upgrade_in_event_loop(move |ui| {
                // root://name/sub → 定位仓库下拉框 + 填子路径
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
            // 与挂载/服务页同一个入口：强制取一份快照并重画。
            // 远程页额外需要「每个远程的用量」（about 会真的连后端），由 refresh 内部按
            // 30s 节流异步补，不跟着 1s 轮询跑。
            refresh::refresh_state(ctx, ui, true).await;
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
            // 先显示编辑器，再异步加载 providers。
            // `type-idx` 先写 -1：类型表还没拿到，此刻没有任何合法类型；
            // 写 0 会让保存路径把清单第 0 项当成用户的选择。
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<RemotesG>();
                g.set_edit_is_new(true);
                g.set_edit_name("".into());
                g.set_edit_params("".into());
                g.set_type_idx(-1);
                g.set_show_advanced(false);
                g.set_show_raw(false);
                g.set_show_editor(true);
            });
            // 异步获取 providers 并更新字段
            let ui2 = ui.clone();
            let ctx2 = ctx.clone();
            tokio::spawn(async move {
                let providers = get_providers(&ctx2).await;
                // 类型清单 = 内核报的后端 + 启用插件声明的派生类型。
                // 界面只拿显示名，真实 id 与落盘用的 backend 留在平行表里。
                let derived = remote_types::collect(&ctx2);
                let choices = build_type_choices(&providers, &derived);
                // 一项都没有 = 内核没连上、也没有上次成功的缓存、还没有插件声明类型。
                // 这时下拉留空、保存被拒，并说明怎么恢复；不塞任何编造的类型。
                let empty = choices.is_empty();
                let first = choices.first().cloned();
                let idx = match ctx2.type_choices.lock() {
                    Ok(mut t) => t.reset(choices.clone(), None),
                    Err(_) => 0,
                };
                // 派生类型带着插件给的初始参数：直接铺进「原始参数」，用户
                // 不用自己敲已知的键。`type` 不写进文本，保存时按 backend 落盘。
                let seed = first
                    .as_ref()
                    .filter(|c| c.derived)
                    .and_then(|c| remote_types::find(&derived, &c.id))
                    .map(derived_params_text)
                    .unwrap_or_default();
                // 派生类型只是个名字：字段表按它落盘时的 base_type 取，
                // 插件额外要的键由插件声明的默认值补上。
                let fields_type = first.as_ref().map(form_fields_type).unwrap_or_default();
                let shown = choices.clone();
                // 空清单必须传 `None`：界面写 -1，保存路径据此拒绝，而不是
                // 悄悄用第 0 项建一个别的后端。
                let pick = if empty { None } else { Some(idx) };
                let _ = ui2.upgrade_in_event_loop(move |ui| {
                    push_type_selection(&ui, &shown, pick);
                    if !seed.is_empty() {
                        ui.global::<RemotesG>()
                            .set_edit_params(seed.as_str().into());
                    }
                });
                if empty {
                    let why = ctx2
                        .providers_err
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| rcgui_i18n::t("rclone 未返回后端类型列表").to_string());
                    // rclone 对 config/* 这类敏感接口要求配置 RC 认证；只连上
                    // core/version 并不代表能读配置（实测踩过）。托管内核由本程序
                    // 带着自己生成的凭据启动，所以真出现认证失败，通常是端口上跑的
                    // 不是本程序起的那个内核——指引就指向状态页，而不是命令行参数。
                    let auth_hint = if why
                        .to_ascii_lowercase()
                        .contains("authentication must be set up")
                    {
                        rcgui_i18n::t("端口上的 rclone 没有接受本程序的凭据：可能不是本程序启动的内核。请到「设置 → 内核与连接」查看状态。")
                    } else {
                        rcgui_i18n::t(
                            "请到「设置 → 内核与连接」查看内核状态：内核由本程序自动启动并握手。",
                        )
                    };
                    toast(
                        &ui2,
                        2,
                        rcgui_i18n::f(
                            "连接内核后再选类型（{}）：现在没有可用的远程类型。{}",
                            &[&(why), &(auth_hint)],
                        ),
                    );
                }
                update_editor_fields(&ui2, &providers, &fields_type);
            });
            false
        }
        Cmd::RemoteTypeChanged => {
            // 下拉里选中第几项 → 平行表里的真实 id 与 backend。
            // 显示名可能被插件改措辞（甚至中文化），绝不能拿它当类型名。
            let picked_idx = read_ui(ui, |ui| ui.global::<RemotesG>().get_type_idx())
                .await
                .unwrap_or(0);
            let choice = match ctx.type_choices.lock() {
                Ok(t) => t.selected(picked_idx),
                Err(_) => None,
            };
            let providers = get_providers(&ctx).await;
            let Some(choice) = choice else {
                // 平行表还没建（启动后没打开过新建对话框）或下标越界：
                // 按空类型渲染，至少不要把上一个类型的字段留在界面上。
                update_editor_fields(ui, &providers, "");
                return false;
            };
            let fields_type = form_fields_type(&choice);
            update_editor_fields(ui, &providers, &fields_type);
            // 派生类型需要的内核后端可能不存在（例如 teldrive 只在 fork 里）。
            // 这里只说事实，不去替用户启动第二个 rclone。
            let mut lack_note: Option<String> = None;
            if choice.derived {
                if let Some(false) = remote_types::backend_available(&providers, &choice.backend) {
                    lack_note = Some(rcgui_i18n::f(
                        "当前连接的内核没有 {} 后端，选这个类型建远程会失败（它需要 {} 后端）。请换用带该后端的内核（维护 → 内核 → 自定义路径），再试一次。",
                        &[&choice.backend, &choice.backend],
                    ));
                }
            }
            if let Some(note) = lack_note {
                toast(ui, 2, note);
            }
            // 派生类型的 defaults 预铺：只补用户还没写的键，不覆盖他已填的值。
            if choice.derived {
                let current = read_ui(ui, |ui| {
                    ui.global::<RemotesG>().get_edit_params().to_string()
                })
                .await
                .unwrap_or_default();
                if let Some(seeded) =
                    seed_derived_defaults(&choice, &remote_types::collect(ctx), &current)
                {
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        ui.global::<RemotesG>().set_edit_params(seeded.into());
                    });
                }
            }
            false
        }
        Cmd::RemoteTypeSearch(query) => {
            // 过滤只改可选清单，但有个必须处理的副作用：搜索把用户当前选中的类型
            // 筛掉时，界面上的选中项就落到别的类型上了（比如搜 `kvault` 只剩
            // K-Vault），这时候参数表必须一起换，否则选中的是新类型、填的是旧类型
            // 的字段，一保存就把参数写到另一个后端上。
            //
            // 反过来，选中项还在清单里就一律不动它：用户改搜索词不该被悄悄改成
            // 第一项，已经填了一半的参数也不能重铺。
            let picked = read_ui(ui, |ui| ui.global::<RemotesG>().get_type_idx())
                .await
                .unwrap_or(0);
            let keep = match ctx.type_choices.lock() {
                Ok(t) => t.selected(picked).map(|c| c.id),
                Err(_) => None,
            };
            let (shown, result) = match ctx.type_choices.lock() {
                Ok(mut t) => {
                    let result = t.refilter(&query, keep.as_deref());
                    (t.visible.clone(), result)
                }
                Err(_) => return false,
            };
            // 只有真的换了类型才需要换表单。
            let switched_to = if result.switched {
                result.idx.and_then(|i| shown.get(i)).cloned()
            } else {
                None
            };
            if let Some(choice) = switched_to.clone() {
                let providers = get_providers(&ctx).await;
                update_editor_fields(ui, &providers, &form_fields_type(&choice));
                // 搜到派生类型时同样要把插件默认参数铺上：这条路径（输入搜索词就
                // 换了类型）以前不铺，用户看到的原始文本和实际保存的内容不一致。
                let current = read_ui(ui, |ui| {
                    ui.global::<RemotesG>().get_edit_params().to_string()
                })
                .await
                .unwrap_or_default();
                if let Some(seeded) =
                    seed_derived_defaults(&choice, &remote_types::collect(&ctx), &current)
                {
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        ui.global::<RemotesG>().set_edit_params(seeded.into());
                    });
                }
            }
            let note: Option<String> = match (result.idx.is_some(), switched_to.as_ref()) {
                // 一条都不剩：界面写成 -1，保存会被拒，所以要让用户知道该改搜索词。
                (false, _) => Some(rcgui_i18n::t("没有匹配的类型，换个搜索词再选类型").to_string()),
                (true, Some(c)) => Some(rcgui_i18n::f(
                    "搜索把原来的类型筛掉了，已切到 {}，下面的参数按它显示",
                    &[&c.display as &dyn std::fmt::Display],
                )),
                _ => None,
            };
            if let Some(note) = note {
                toast(ui, 2, note);
            }
            let idx = result.idx;
            let _ = ui.upgrade_in_event_loop(move |ui| push_type_selection(&ui, &shown, idx));
            false
        }
        Cmd::RemoteEditLoad { name } => {
            // 类型没有实现的远程不给改配置：保存时会把参数重写成「普通 webdav」，
            // 标记键一旦被抹掉，装回插件它就再也认不回自己是 K-Vault 了。
            if let Some(msg) = kv_access_blocked(ctx, &name).await {
                toast(ui, 2, msg);
                return false;
            }
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
            // 存进 rclone 的 `type` 只是后端（K-Vault 存的是 `webdav`），所以「该显示成
            // 哪个类型」得另算：参数里有标记、且当前有插件声明那个类型 id，才显示插件
            // 给的名字；插件不在就老实显示 `webdav` —— 它现在确实只是一个 webdav 远程。
            let derived = remote_types::collect(ctx);
            let ty = display_type_of(&entry, &ty, &derived);
            let choice = edit_single_choice(&ty, &derived);
            let providers = get_providers(&ctx).await;
            // 参数表按后端取：派生类型落盘就是 base_type，它自己没有另一张参数表。
            let (desc, mut fields) =
                build_fields_from_provider(&providers, &form_fields_type(&choice));
            fill_fields_with_existing(&mut fields, &existing_params);
            // 既有远程的类型下拉是只读的，只显示当前这一项。平行表也重置成这一项：
            // 保存时要靠它拿回同一个真实类型（派生类型落 base_type，不能让显示名
            // 参与往返，否则插件改一次措辞就会把 type 写坏）。
            let choice_idx = match ctx.type_choices.lock() {
                Ok(mut t) => t.reset(vec![choice.clone()], Some(&choice.id)),
                Err(_) => 0,
            };
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<RemotesG>();
                let one = std::slice::from_ref(&choice);
                push_type_choices(&ui, one, choice_idx);
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
            let (name, picked_idx, params_text, is_new, show_raw) = read_ui(ui, |ui| {
                let g = ui.global::<RemotesG>();
                // 下拉里只有显示名；真实类型要查平行表（插件可以改显示措辞）。
                (
                    g.get_edit_name().to_string(),
                    g.get_type_idx(),
                    g.get_edit_params().to_string(),
                    g.get_edit_is_new(),
                    g.get_show_raw(),
                )
            })
            .await
            .unwrap_or_default();
            if name.trim().is_empty() || name.contains(':') || name.contains(' ') {
                toast(ui, 2, rcgui_i18n::t("名称不能为空，且不能包含空格或冒号"));
                return false;
            }
            // 改既有远程同样要拦，理由同 `RemoteEditLoad`：这一步会把参数整体重写。
            // 新建（`is_new`）不拦：那时名字对应的还不是一条 K-Vault 远程。
            if !is_new {
                if let Some(msg) = kv_access_blocked(ctx, &name).await {
                    toast(ui, 2, msg);
                    return false;
                }
            }
            let choice = match ctx.type_choices.lock() {
                Ok(t) => t.selected(picked_idx),
                Err(_) => None,
            };
            let Some(choice) = choice else {
                // 两种来源：类型清单还没建好（启动后没打开过新建对话框），或者当前
                // 搜索词把选中项筛掉了（`type-idx` 是 -1）。无论哪种，都不能退回
                // 第 0 项去保存——那会拿另一套参数建别的后端。
                toast(
                    ui,
                    2,
                    rcgui_i18n::t("先选一个远程类型再保存（当前搜索词下没有选中项）"),
                );
                return false;
            };
            // 派生类型真正落盘的是 base_type；这里先确认内核有这个后端，
            // 否则 rclone 会抛一句用户看不懂的错（常见于 teldrive 只在 fork 里）。
            if choice.derived {
                let providers = get_providers(ctx).await;
                if let Some(false) = remote_types::backend_available(&providers, &choice.backend) {
                    toast(
                        ui,
                        2,
                        rcgui_i18n::f(
                            "这个内核没有 {} 后端，{} 保存不了。请换用带该后端的内核（维护 → 内核 → 自定义路径）后再试。",
                            &[&choice.backend, &choice.id],
                        ),
                    );
                    return false;
                }
            }
            // 优先使用模板字段，原始模式或字段为空时用 key=value
            let user = if !show_raw {
                let fields = read_fields_from_ui(ui).await;
                if !fields.is_empty() {
                    fields_to_params(&fields)
                } else {
                    serde_json::Value::Object(parse_kv_lines(&params_text))
                }
            } else {
                serde_json::Value::Object(parse_kv_lines(&params_text))
            };
            // 字段表里没有插件独有的键，直接从它组装会把插件的默认参数弄丢，
            // 所以先铺插件的默认值，再让用户填的值盖上去。
            let user = match user {
                serde_json::Value::Object(m) => m,
                _ => serde_json::Map::new(),
            };
            let derived = if choice.derived {
                remote_types::collect(ctx)
            } else {
                Vec::new()
            };
            let client = ctx.rc.client().await;
            let res = match build_save_request(&name, &choice, &derived, user, is_new) {
                SaveRequest::Create {
                    name,
                    rtype,
                    params,
                } => client.config_create(&name, &rtype, params).await,
                SaveRequest::Update { name, params } => client.config_update(&name, params).await,
            };
            match res {
                Ok(()) => {
                    let _ = ui.upgrade_in_event_loop(|ui| {
                        ui.global::<RemotesG>().set_show_editor(false);
                    });
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::RemotesRefresh)).await;
                    toast(ui, 1, rcgui_i18n::f("远程存储 {} 已保存", &[&(name)]));
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("保存失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::RemoteDelete { name } => {
            // 确认已在 UI 层（ConfirmPop）完成。
            //
            // ⚠️ 顺序很关键：**先停服务、卸载挂载，最后才删远程**。
            //
            // 原来的顺序是「先 config/delete 再清理」，结果：
            // 远程一被删除，rclone 侧该 fs 立刻失效，此时再 unmount / serve_stop
            // 要么找不到目标、要么报错，而错误被 `let _ =` 吞掉 —— 表现就是
            // 「删除远程后挂载盘符还在、服务端口还在监听」。
            // 另外只清档案里记录过的挂载点也不够：用户可能在别处手动建过挂载，
            // 运行态的挂载必须一起扫。
            let client = ctx.rc.client().await;

            // 1) 收集要清理的挂载点与服务 id
            let (profile_points, profile_addrs, had_proxy) = {
                let cfg = ctx.cfg.lock().unwrap();
                let had_proxy = proxy::bound_profile(&cfg, &name).is_some();
                let points: Vec<String> = cfg
                    .auto_mounts
                    .iter()
                    .filter(|m| m.remote == name)
                    .map(|m| m.point.clone())
                    .collect();
                let addrs: Vec<String> = cfg
                    .auto_serves
                    .iter()
                    .filter(|s| s.remote == name)
                    .map(|s| s.addr.clone())
                    .collect();
                (points, addrs, had_proxy)
            };

            // 运行态里的挂载点（含用户手动挂的）与服务（含手动启动的）
            let live_mounts = client.mounts().await.unwrap_or_default();
            let mut points_to_unmount: Vec<String> = Vec::new();
            for m in &live_mounts {
                let (rname, _) = split_fs_str(&m.fs);
                if rname == name {
                    points_to_unmount.push(m.mount_point.clone());
                }
            }
            // 档案里记过但当前没在运行的也放进列表：unmount 对不存在的挂载是幂等的，
            // 一并调用可以覆盖「刚卸载又自动重挂」这类竞态。
            for p in &profile_points {
                let key = mount_key(p);
                if !points_to_unmount.iter().any(|x| mount_key(x) == key) {
                    points_to_unmount.push(p.clone());
                }
            }
            let live_serves: Vec<(String, String)> = {
                let raw = client.serve_list().await.unwrap_or(json!({}));
                reconcile::serve_addrs_from(&raw)
                    .into_iter()
                    .filter_map(|(addr, id)| {
                        // 只停「本 remote 的」服务：优先按地址匹配档案记录，
                        // 再按 fs 前缀匹配（fs 形如 name:sub）
                        let by_addr = profile_addrs.iter().any(|a| a == &addr);
                        let by_fs = raw
                            .get("list")
                            .and_then(|x| x.as_array())
                            .map(|arr| {
                                arr.iter().any(|it| {
                                    let a = it
                                        .get("params")
                                        .and_then(|p| p.get("addr"))
                                        .or_else(|| it.get("addr"))
                                        .and_then(|x| x.as_str())
                                        .unwrap_or_default();
                                    if a != addr {
                                        return false;
                                    }
                                    let fs = it
                                        .get("params")
                                        .and_then(|p| p.get("fs"))
                                        .or_else(|| it.get("fs"))
                                        .and_then(|x| x.as_str())
                                        .unwrap_or_default();
                                    split_fs_str(fs).0 == name
                                })
                            })
                            .unwrap_or(false);
                        if by_addr || by_fs {
                            Some((addr, id))
                        } else {
                            None
                        }
                    })
                    .collect()
            };

            // 2) 先停服务、卸载挂载（此时 remote 还在，操作才有效）
            let _busy = RemotesBusyGuard::new(ui);
            let mut errs: Vec<String> = Vec::new();
            for (addr, id) in &live_serves {
                match client.serve_stop(id).await {
                    Ok(()) => {}
                    Err(e) => errs.push(rcgui_i18n::f("停止服务 {}: {}", &[&(addr), &(e)])),
                }
            }
            for p in &points_to_unmount {
                match client.unmount(p).await {
                    Ok(()) => {}
                    Err(e) => errs.push(rcgui_i18n::f("卸载 {}: {}", &[&(p), &(e)])),
                }
            }

            // 3) 再删除远程
            match client.config_delete(&name).await {
                Ok(()) => {
                    // K-Vault 的明文令牌存在侧表里：远程没了，那份令牌也不该留着。
                    // 两个命名空间都清：新的是现在写的地方，旧的是历史遗留
                    // （迁移函数只复制不搬走，所以旧记录可能还在）。
                    let _ = ctx.store.delete(kvault::TOKEN_NS, &name);
                    let _ = ctx.store.delete(kvault::STORE_NS, &name);
                    // 4) 最后清理档案里的记忆（否则重放会一直失败并刷错误提示）
                    {
                        let mut cfg = ctx.cfg.lock().unwrap();
                        // 代理绑定：远程没了，绑定只会变成悬空引用
                        cfg.proxy_bindings
                            .retain(|b| !b.remote.eq_ignore_ascii_case(&name));
                        let _ = persist::forget_remote(&mut cfg, &name);
                        let _ = profiles::save_from_cfg(&cfg);
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::RemotesRefresh)).await;
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ServesRefresh)).await;
                    let mut msg = rcgui_i18n::f(
                        "已删除 {}（停止服务 {} 个、卸载挂载 {} 个）",
                        &[&(name), &(live_serves.len()), &(points_to_unmount.len())],
                    );
                    if had_proxy {
                        msg.push_str(rcgui_i18n::t("，代理绑定已清除"));
                    }
                    if !errs.is_empty() {
                        toast(
                            ui,
                            2,
                            rcgui_i18n::f(
                                "{}；但有 {} 项清理失败：{}",
                                &[&(msg), &(errs.len()), &(errs.join("；"))],
                            ),
                        );
                    } else {
                        toast(ui, 1, msg);
                    }
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("删除失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::RemoteCancel => {
            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.global::<RemotesG>().set_show_editor(false);
            });
            false
        }
        Cmd::ProxyListRefresh => {
            // 代理配置卡片（行 + 候选下拉由 apply_proxy_column 一并刷新）
            let rows: Vec<ProxyRow> = {
                let cfg = ctx.cfg.lock().unwrap();
                cfg.proxies
                    .iter()
                    .map(|p| {
                        let used = proxy::remotes_using(&cfg, &p.id);
                        ProxyRow {
                            id: p.id.clone().into(),
                            name: p.name.clone().into(),
                            protocol: p.protocol.label().into(),
                            target: p.display_target().into(),
                            auth: if p.has_auth() {
                                rcgui_i18n::t("有凭据").into()
                            } else {
                                rcgui_i18n::t("无").into()
                            },
                            used_by: if used.is_empty() {
                                "—".into()
                            } else {
                                used.join("、").into()
                            },
                        }
                    })
                    .collect()
            };
            let _ = ui.upgrade_in_event_loop(move |ui| {
                ui.global::<RemotesG>()
                    .set_proxy_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            });
            // 悬空引用（绑定指向已删除的配置）要主动提示，否则用户只看到一个「待修复」
            // 徽标却不知道该怎么办。
            let dangling = {
                let cfg = ctx.cfg.lock().unwrap();
                proxy::dangling_remotes(&cfg)
            };
            if !dangling.is_empty() {
                toast(
                    ui,
                    2,
                    rcgui_i18n::f(
                        "以下远程引用的代理配置已不存在，请重新选择：{}",
                        &[&(dangling.join("、"))],
                    ),
                );
            }
            apply_proxy_column(ui).await;
            false
        }
        Cmd::ProxyEditLoad { id } => {
            let (is_new, name, proto_idx, host, port, user, pass) = {
                let cfg = ctx.cfg.lock().unwrap();
                match cfg.proxies.iter().find(|p| p.id == id) {
                    Some(p) => (
                        false,
                        p.name.clone(),
                        p.protocol.index(),
                        p.host.clone(),
                        p.port.to_string(),
                        p.user.clone(),
                        p.pass.clone(),
                    ),
                    None => (
                        true,
                        String::new(),
                        0,
                        String::new(),
                        "8080".to_string(),
                        String::new(),
                        String::new(),
                    ),
                }
            };
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<RemotesG>();
                g.set_proxy_edit_id(id.into());
                g.set_proxy_edit_is_new(is_new);
                g.set_proxy_edit_name(name.into());
                g.set_proxy_edit_proto(proto_idx);
                g.set_proxy_edit_host(host.into());
                g.set_proxy_edit_port(port.into());
                g.set_proxy_edit_user(user.into());
                g.set_proxy_edit_pass(pass.into());
                g.set_proxy_show_editor(true);
            });
            false
        }
        Cmd::ProxyCancel => {
            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.global::<RemotesG>().set_proxy_show_editor(false);
            });
            false
        }
        Cmd::ProxySave => {
            let f = read_ui(ui, |ui| {
                let g = ui.global::<RemotesG>();
                (
                    g.get_proxy_edit_id().to_string(),
                    g.get_proxy_edit_name().to_string(),
                    g.get_proxy_edit_proto(),
                    g.get_proxy_edit_host().to_string(),
                    g.get_proxy_edit_port().to_string(),
                    g.get_proxy_edit_user().to_string(),
                    g.get_proxy_edit_pass().to_string(),
                )
            })
            .await
            .unwrap_or_default();
            let (edit_id, name, proto_idx, host, port_text, user, pass) = f;
            let port: u16 = match port_text.trim().parse() {
                Ok(p) => p,
                Err(_) => {
                    toast(ui, 2, rcgui_i18n::t("端口必须是 1–65535 的数字"));
                    return false;
                }
            };
            // 新建时 id 为空，需要现生成；先记下「是不是新建」供重名校验排除自己
            let edit_id_is_new = edit_id.is_empty();
            let profile = rcgui_profile::ProxyProfile {
                id: if edit_id_is_new {
                    proxy::new_proxy_id()
                } else {
                    edit_id
                },
                name,
                protocol: rcgui_profile::ProxyProtocol::from_index(proto_idx),
                host,
                port,
                user,
                pass,
            };
            if let Err(e) = profile.validate() {
                toast(ui, 2, e);
                return false;
            }
            // 重名校验：代理配置是 RCGUI 自己的档案数据，rclone 不会帮我们查重，
            // 必须在这里拦住，否则会出现两个同名配置，下拉框里无法区分。
            // 编辑时排除自己的 id，这样只改端口/地址不会被自己名字挡住。
            {
                let cfg = ctx.cfg.lock().unwrap();
                let exclude = if edit_id_is_new {
                    ""
                } else {
                    profile.id.as_str()
                };
                if let Some(dup) = proxy::proxy_name_conflict(&cfg.proxies, &profile.name, exclude)
                {
                    drop(cfg);
                    toast(
                        ui,
                        2,
                        rcgui_i18n::f("已存在同名代理配置「{}」，请换一个名称", &[&(dup)]),
                    );
                    return false;
                }
            }
            // 校验 URL 能构造出来（覆盖 IPv6 / 特殊字符等边界）
            if let Err(e) = profile.to_proxy_url() {
                toast(ui, 2, e);
                return false;
            }
            // 找出使用该配置的 remote，稍后需要对它们重新下发
            let affected: Vec<String> = {
                let mut cfg = ctx.cfg.lock().unwrap();
                let is_new = !cfg.proxies.iter().any(|p| p.id == profile.id);
                let label = if is_new {
                    rcgui_i18n::t("已新增")
                } else {
                    rcgui_i18n::t("已更新")
                };
                cfg.proxies.retain(|p| p.id != profile.id);
                cfg.proxies.push(profile.clone());
                // 改动配置内容后，用它的 remote 都标记为「待应用」，由下面的
                // reconcile 逐项比对后重写。
                // **不能清 applied_value**：它是回滚依据，清掉会让我们的旧值在
                // 下一轮被当成「别人改的」→ 误报冲突；协议变了也会丢掉旧键记录。
                let affected: Vec<String> = cfg
                    .proxy_bindings
                    .iter()
                    .filter(|b| b.proxy_id == profile.id)
                    .map(|b| b.remote.clone())
                    .collect();
                for b in cfg.proxy_bindings.iter_mut() {
                    if b.proxy_id == profile.id {
                        b.applied = false;
                    }
                }
                if let Err(e) = profiles::save_from_cfg(&cfg) {
                    toast(ui, 2, rcgui_i18n::f("保存代理配置失败: {}", &[&(e)]));
                    return false;
                }
                toast(
                    ui,
                    1,
                    rcgui_i18n::f("代理配置「{}」{}", &[&(profile.name), &(label)]),
                );
                affected
            };
            {
                let _ = ui.upgrade_in_event_loop(|ui| {
                    ui.global::<RemotesG>().set_proxy_show_editor(false);
                });
            }
            // 立刻把新值下发到所有正在使用它的 remote（连上时）。
            //
            // 这是「保存代理配置后远程仍用旧代理」的修复：只把 applied 置 false
            // 只是标记「待应用」，要等下次重连才生效，用户会以为修改丢了。
            // 改走统一入口 `reconcile()`：它会把「档案里的期望代理」与
            // 「rclone 里当前实际生效的代理」逐项比对，只对变了的项下发。
            if !affected.is_empty() && ctx.rc.is_live() {
                let _busy = RemotesBusyGuard::new(ui);
                let client = ctx.rc.client().await;
                let mut draft = ctx.cfg.lock().unwrap().clone();
                // 插件不在的远程不重挂、不重起服务：对账本身不知道插件的事
                let blocked = kv_blocked_map(ctx, &draft).await;
                let rep = reconcile::reconcile(&client, &mut draft, &blocked).await;
                {
                    let mut cfg = ctx.cfg.lock().unwrap();
                    cfg.proxy_bindings = draft.proxy_bindings.clone();
                    let _ = profiles::save_from_cfg(&cfg);
                }
                if !rep.failed.is_empty() {
                    toast(ui, 2, format!("{}：{}", rep.summary(), rep.detail()));
                } else if !rep.skipped.is_empty() {
                    toast(ui, 0, format!("{}：{}", rep.summary(), rep.detail()));
                } else {
                    toast(
                        ui,
                        1,
                        format!("{}。{}", rep.summary(), proxy::restart_hint()),
                    );
                }
            } else if !affected.is_empty() {
                toast(
                    ui,
                    0,
                    rcgui_i18n::f(
                        "代理配置已保存，{} 个远程标记为待应用（连接后将自动下发）",
                        &[&(affected.len())],
                    ),
                );
            }
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ProxyListRefresh)).await;
            false
        }
        Cmd::ProxyDeleteAsk { id } => {
            let (name, affected) = {
                let cfg = ctx.cfg.lock().unwrap();
                let name = cfg
                    .proxies
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                (name, proxy::remotes_using(&cfg, &id))
            };
            if name.is_empty() {
                toast(ui, 2, rcgui_i18n::t("该代理配置已不存在，请刷新后重试"));
                return false;
            }
            let affected_text = if affected.is_empty() {
                String::new()
            } else {
                rcgui_i18n::f("以下远程会回落到「不代理」：{}", &[&(affected.join("、"))])
            };
            // 没有关联时不必二次确认，直接删
            if affected.is_empty() {
                delete_proxy_now(ctx, ui, &id).await;
                return false;
            }
            let _ = ui.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<RemotesG>();
                g.set_proxy_del_id(id.into());
                g.set_proxy_del_name(name.into());
                g.set_proxy_del_affected(affected_text.into());
                g.set_proxy_del_show(true);
            });
            false
        }
        Cmd::ProxyDelete { id } => {
            delete_proxy_now(ctx, ui, &id).await;
            false
        }
        Cmd::ProxyBind { remote, idx } => {
            let (profile, proxy_id) = {
                let cfg = ctx.cfg.lock().unwrap();
                if idx <= 0 {
                    (None, String::new())
                } else {
                    match cfg.proxies.get((idx - 1) as usize) {
                        Some(p) => (Some(p.clone()), p.id.clone()),
                        None => {
                            drop(cfg);
                            toast(ui, 2, rcgui_i18n::t("选择的代理配置已不存在，请刷新后重试"));
                            return false;
                        }
                    }
                }
            };
            // 1) 先写档案（离线也要能存），标记为待应用
            {
                let mut cfg = ctx.cfg.lock().unwrap();
                cfg.bind_proxy_slot(&remote, &proxy_id);
                if let Err(e) = profiles::save_from_cfg(&cfg) {
                    toast(ui, 2, rcgui_i18n::f("保存代理绑定失败: {}", &[&(e)]));
                    return false;
                }
            }
            // ftp / sftp 走原生键：要让用户知道键名、只补空缺策略和凭据限制。
            let native_hint = match &profile {
                Some(_) => remote_type(ctx, &remote).and_then(|t| proxy::native_note(&t)),
                None => None,
            };
            // 2) 连上了就立刻让它生效。
            //
            // 这里改走统一的 `reconcile::reconcile()`，不再手工逐条下发：
            // 它会把档案（期望）与 rclone 运行态（现状）做一次逐项比对，
            // 只对真正不一致的项动作。这样「切换代理」和「改代理配置后重载」
            // 以及「导入后的重载」共用同一套代码，不会再出现某条路径漏写。
            if ctx.rc.is_live() {
                let _busy = RemotesBusyGuard::new(ui);
                let client = ctx.rc.client().await;
                // 关键：先 clone 出配置并**立刻放锁**，再 await reconcile。
                // 持有 MutexGuard 跨 await 会让 future 非 Send（tokio::spawn 会拒绝）。
                let mut draft = ctx.cfg.lock().unwrap().clone();
                // 插件不在的远程不重挂、不重起服务：对账本身不知道插件的事
                let blocked = kv_blocked_map(ctx, &draft).await;
                let rep = reconcile::reconcile(&client, &mut draft, &blocked).await;
                {
                    // 写回：只把 reconcile 会改动的字段合并回真实配置，
                    // 避免整份覆盖掉这期间 UI 可能改过的其他设置。
                    let mut cfg = ctx.cfg.lock().unwrap();
                    cfg.proxy_bindings = draft.proxy_bindings.clone();
                    let _ = profiles::save_from_cfg(&cfg);
                }
                if !rep.failed.is_empty() {
                    toast(
                        ui,
                        2,
                        format!(
                            "{}：{}{}",
                            rep.summary(),
                            rep.detail(),
                            native_hint.unwrap_or("")
                        ),
                    );
                } else if !rep.skipped.is_empty() {
                    toast(
                        ui,
                        0,
                        format!(
                            "{}：{}{}",
                            rep.summary(),
                            rep.detail(),
                            native_hint.unwrap_or("")
                        ),
                    );
                } else {
                    toast(
                        ui,
                        1,
                        format!(
                            "{}。{}{}",
                            rep.summary(),
                            proxy::restart_hint(),
                            native_hint.unwrap_or("")
                        ),
                    );
                }
            } else {
                toast(
                    ui,
                    0,
                    rcgui_i18n::f(
                        "{} 的代理已保存，当前未连接 rclone，将在连接后自动应用。{}{}",
                        &[
                            &(remote),
                            &(proxy::restart_hint()),
                            &(native_hint.unwrap_or("")),
                        ],
                    ),
                );
            }
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ProxyListRefresh)).await;
            false
        }
        Cmd::ServesRefresh => {
            // 与挂载页同一个入口：一份快照同时更新服务列表与仪表盘里的计数
            refresh::refresh_state(ctx, ui, true).await;
            false
        }
        Cmd::ServeStart => {
            let (stype, remote, sub_path, addr, user, pass, svc_name) = read_ui(ui, |ui| {
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
                toast(ui, 2, rcgui_i18n::t("请先在「远程」页添加仓库，再选择仓库"));
                return false;
            }
            let sub_path = sub_path.trim().trim_matches('/').to_string();
            // 服务等于把远程内容重新暴露成一个局域网地址：插件不在时启动它，
            // 只是多出一个打不开的地址，还会让用户以为这个远程还能用。
            if let Some(msg) = kv_access_blocked(ctx, &remote).await {
                toast(ui, 2, msg);
                return false;
            }
            let fs = compose_fs(&remote, &sub_path);
            let addr_t = addr.trim().to_string();
            if addr_t.is_empty() {
                toast(ui, 2, rcgui_i18n::t("请填写监听地址"));
                return false;
            }
            // 服务显示名重名校验：必须在**启动之前**拦，否则服务已经起来了才发现重名，
            // 用户还得手动去停。同地址视为「编辑自己」，允许保留原名。
            {
                let svc = svc_name.trim().to_string();
                if !svc.is_empty() {
                    let cfg = ctx.cfg.lock().unwrap();
                    if let Some(dup) = proxy::serve_name_conflict(&cfg.serve_names, &svc, &addr_t) {
                        drop(cfg);
                        toast(
                            ui,
                            2,
                            rcgui_i18n::f("已存在同名服务「{}」，请换一个名称", &[&(dup)]),
                        );
                        return false;
                    }
                }
            }
            // busy 由守卫托管：任何返回路径都会复位
            let _busy = BusyGuard::new(ui, BusyKind::Serves);
            let client = ctx.rc.client().await;
            let res = client.serve_start(&stype, &fs, &addr_t, &user, &pass).await;
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
                            &mut cfg, &stype, &remote, &sub_path, &addr_t, &name,
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
                    toast(ui, 1, rcgui_i18n::f("服务已启动: {}", &[&(id)]));
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("启动失败: {}", &[&(e)])),
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
                    toast(ui, 1, rcgui_i18n::f("已停止 {}", &[&(id)]));
                }
                Err(e) => toast(ui, 2, rcgui_i18n::f("停止失败: {}", &[&(e)])),
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
            toast(ui, 1, rcgui_i18n::t("限速已应用"));
            false
        }
        Cmd::JobStop { id } => {
            // id 为 0 说明这一行没能对上 rclone 的 job（例如 core/stats 没给 group）。
            // 直接发 job/stop 0 会打到别的任务上，所以只提示、不发请求。
            if id <= 0 {
                toast(ui, 2, rcgui_i18n::t("该任务没有可用的 job 号，无法取消"));
            } else {
                let client = ctx.rc.client().await;
                match client.job_stop(id).await {
                    Ok(()) => {
                        build_tasks(ctx, ui).await;
                        toast(ui, 1, rcgui_i18n::f("任务 #{} 已停止", &[&(id)]));
                    }
                    Err(e) => toast(ui, 2, rcgui_i18n::f("停止失败: {}", &[&(e)])),
                }
            }
            false
        }
        Cmd::OptionsLoad => {
            let client = ctx.rc.client().await;
            match client.options_get().await {
                Ok(v) => {
                    let main = v.get("main").cloned().unwrap_or(json!({}));
                    let transfers = main
                        .get("Transfers")
                        .and_then(|x| x.as_i64())
                        .map(|n| n.to_string())
                        .unwrap_or_default();
                    let checkers = main
                        .get("Checkers")
                        .and_then(|x| x.as_i64())
                        .map(|n| n.to_string())
                        .unwrap_or_default();
                    let tps = main
                        .get("TPSLimit")
                        .and_then(|x| x.as_f64())
                        .map(|n| format!("{:.1}", n))
                        .unwrap_or_default();
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
                Err(e) => toast(ui, 2, rcgui_i18n::f("读取参数失败: {}", &[&(e)])),
            }
            false
        }
        Cmd::OptionsSave => {
            let (transfers, checkers, tps, log_level) = read_ui(ui, |ui| {
                let g = ui.global::<SettingsG>();
                let li = g.get_log_level_idx() as usize;
                let lv = g
                    .get_log_levels()
                    .row_data(li)
                    .unwrap_or_default()
                    .to_string();
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
                Ok(()) => toast(ui, 1, rcgui_i18n::t("运行参数已保存")),
                Err(e) => toast(ui, 2, rcgui_i18n::f("保存失败: {}", &[&(e)])),
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
            let mut opened = std::process::Command::new("explorer.exe")
                .arg(&d)
                .spawn()
                .is_ok();
            if !opened {
                opened = std::process::Command::new("cmd.exe")
                    .args(["/c", "start", "", d.to_string_lossy().as_ref()])
                    .spawn()
                    .is_ok();
            }
            toast(
                ui,
                if opened { 1 } else { 2 },
                rcgui_i18n::f(
                    "{}插件目录：{}",
                    &[
                        &(if opened {
                            rcgui_i18n::t("已请求打开")
                        } else {
                            rcgui_i18n::t("无法打开")
                        }),
                        &(d.display()),
                    ],
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
            if let Some((id, old_enabled, _kind, verified, path)) = row {
                // 未通过完整性校验的插件一律不允许启用
                if !old_enabled && !verified {
                    toast(
                        ui,
                        2,
                        rcgui_i18n::f("插件 {} 未通过完整性校验，无法启用", &[&(id)]),
                    );
                    return false;
                }
                let enabled = !old_enabled;

                // 旧格式插件包：启用 = 执行 entry_file（例如安装 WinFsp 的 msi）。
                // 新格式（config.json + main.wasm）没有「入口可执行文件」这一步，
                // 它是 WASI 脚本，由宿主在面板/动作调用时载入。
                if enabled {
                    let dir = PathBuf::from(&path);
                    if dir.is_dir() && dir.join("plugin.json").is_file() {
                        match read_plugin_json_lenient(&dir) {
                            Some(meta) => {
                                // 再次校验，避免文件在校验之后被替换（TOCTOU）
                                if let Err(e) =
                                    plugin_host::package::load_from_dir(&dir, &gui_version())
                                {
                                    toast(
                                        ui,
                                        2,
                                        rcgui_i18n::f(
                                            "插件 {} 校验失败: {}",
                                            &[&(id), &(format!("{:#}", e))],
                                        ),
                                    );
                                    return false;
                                }
                                toast(
                                    ui,
                                    1,
                                    rcgui_i18n::f("正在执行插件 {} 的入口文件…", &[&(id)]),
                                );
                                let r = tokio::task::spawn_blocking(move || {
                                    plugin_host::package::run_entry_file(&dir, &meta)
                                })
                                .await;
                                match r {
                                    Ok(Ok(0)) => toast(
                                        ui,
                                        1,
                                        rcgui_i18n::f(
                                            "插件 {} 已启用（入口文件执行成功）",
                                            &[&(id)],
                                        ),
                                    ),
                                    Ok(Ok(code)) => toast(
                                        ui,
                                        2,
                                        rcgui_i18n::f(
                                            "插件 {} 入口文件退出码 {}（可能未成功，请检查）",
                                            &[&(id), &(code)],
                                        ),
                                    ),
                                    Ok(Err(e)) => {
                                        toast(
                                            ui,
                                            2,
                                            rcgui_i18n::f(
                                                "插件 {} 启动失败: {}",
                                                &[&(id), &(format!("{:#}", e))],
                                            ),
                                        );
                                        return false; // 执行失败不记录为已启用
                                    }
                                    Err(e) => {
                                        toast(
                                            ui,
                                            2,
                                            rcgui_i18n::f(
                                                "插件 {} 执行线程异常: {}",
                                                &[&(id), &(e)],
                                            ),
                                        );
                                        return false;
                                    }
                                }
                            }
                            None => {
                                toast(
                                    ui,
                                    2,
                                    rcgui_i18n::f("插件 {} 的 plugin.json 无法读取", &[&(id)]),
                                );
                                return false;
                            }
                        }
                    }
                }

                {
                    let mut cfg = ctx.cfg.lock().unwrap();
                    cfg.plugin_enabled.insert(id.clone(), enabled);
                    let _ = config::save(&cfg);
                }
                if !enabled {
                    // 停用 = 这个插件不再有面板，也不再能要求宿主替它弹对话框；
                    // 它之前挑过、还没发出去的文件授权必须一起失效，
                    // 否则一次已经停用的插件仍能把那个文件发出去。
                    plugin_host::upload::revoke_plugin(&id);
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
                        rcgui_i18n::f(
                            "插件 {} 已{}",
                            &[
                                &(id),
                                &(if enabled {
                                    rcgui_i18n::t("启用")
                                } else {
                                    rcgui_i18n::t("停用")
                                }),
                            ],
                        ),
                    );
                });
            }
            false
        }
        // ---------------- 图床面板：上传 → 直链 → 短链 ----------------
        // ---------------- 插件通用面板 / 卸载 ----------------
        Cmd::PluginPanelOpen { idx } => {
            let row = read_ui(ui, move |ui| {
                ui.global::<PluginsG>()
                    .get_rows()
                    .row_data(idx as usize)
                    .map(|r| {
                        (
                            r.id.to_string(),
                            r.has_panel,
                            r.verified,
                            r.status.to_string(),
                        )
                    })
            })
            .await
            .flatten();
            if let Some((id, has_panel, verified, status)) = row {
                // 先无条件清空面板：校验失败的插件也要清，
                // 否则它会继续显示"上一个插件"的面板（用户会以为面板串了）。
                let panel_id = id.clone();
                let fail = if verified {
                    String::new()
                } else if status.trim().is_empty() {
                    rcgui_i18n::t("插件未通过完整性校验，设置面板不可用").to_string()
                } else {
                    rcgui_i18n::f("插件未通过完整性校验：{}", &[&(status)])
                };
                let fail_ui = fail.clone();
                // 清空面板会把 `panel-page` 抹成空串，之后就没法知道用户原来停在哪一页。
                // 所以在清空之前，先在 UI 线程上把"当前是哪个插件、停在哪一页"记下来，
                // 行建好之后交给 `apply_panel_reopened` 判断该不该回到原来那页。
                let before: Arc<Mutex<Option<plugins::PanelReopen>>> = Arc::new(Mutex::new(None));
                let before_ui = Arc::clone(&before);
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    let g = ui.global::<PluginsG>();
                    // 先记，再清：清空会把 `panel-page` 抹成空串，那时再读就没得救了。
                    if let Ok(mut w) = before_ui.lock() {
                        *w = Some(plugins::PanelReopen::capture(
                            g.get_panel_plugin().as_str(),
                            g.get_panel_page().as_str(),
                        ));
                    }
                    g.set_panel_plugin(panel_id.as_str().into());
                    g.set_panel_loading(has_panel && fail_ui.is_empty());
                    g.set_panel_error(fail_ui.as_str().into());
                    plugins::clear_panel(&ui);
                });
                if !verified {
                    toast(ui, 2, fail.as_str());
                    return false;
                }
                if has_panel {
                    match plugins::build_panel(ctx, &id).await {
                        Ok(rows) => {
                            let before = before
                                .lock()
                                .map(|w| w.clone())
                                .unwrap_or_else(|e| e.into_inner().clone())
                                .unwrap_or_else(|| plugins::PanelReopen::capture("", ""));
                            let opened = id.clone();
                            let _ = ui.upgrade_in_event_loop(move |ui| {
                                plugins::apply_panel_reopened(&ui, &rows, &opened, &before);
                                ui.global::<PluginsG>().set_panel_loading(false);
                            });
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            let _ = ui.upgrade_in_event_loop(move |ui| {
                                let g = ui.global::<PluginsG>();
                                g.set_panel_loading(false);
                                g.set_panel_error(msg.as_str().into());
                            });
                        }
                    }
                }
            }
            false
        }
        Cmd::PluginPanelClose => {
            // 面板关了就别再催插件的定时动作了
            plugins::panel_repeat_clear();
            let _ = ui.upgrade_in_event_loop(|ui| {
                let g = ui.global::<PluginsG>();
                g.set_panel_plugin("".into());
                plugins::clear_panel(&ui);
                g.set_panel_error("".into());
            });
            false
        }
        Cmd::PluginPanelField { key, value } => {
            let id = read_ui(ui, |ui| {
                ui.global::<PluginsG>().get_panel_plugin().to_string()
            })
            .await
            .unwrap_or_default();
            if !id.is_empty() && !key.is_empty() {
                // 立刻落盘：用户在输入框里的每个字符都要保住
                plugins::set_form_field(&ctx.store, &id, &key, &value);
            }
            false
        }
        Cmd::PluginPanelPick { key, value } => {
            let id = read_ui(ui, |ui| {
                ui.global::<PluginsG>().get_panel_plugin().to_string()
            })
            .await
            .unwrap_or_default();
            if id.is_empty() || key.is_empty() {
                return false;
            }
            // 落盘 + 立刻重拉面板：输入框的 text 绑在面板模型上，
            // 不重拉就会继续显示旧值（用户点了历史条目却看不到变化）。
            plugins::set_form_field(&ctx.store, &id, &key, &value);
            if let Ok(rows) = plugins::build_panel(ctx, &id).await {
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    plugins::apply_panel(&ui, &rows);
                    ui.global::<PluginsG>().set_panel_loading(false);
                });
            }
            false
        }
        Cmd::PluginPanelAction { action } => {
            let id = read_ui(ui, |ui| {
                ui.global::<PluginsG>().get_panel_plugin().to_string()
            })
            .await
            .unwrap_or_default();
            if id.is_empty() || action.is_empty() {
                return false;
            }
            // 面板动作可能跨好几轮能力协商（「启动 rclone」还要等进程起来并握手），
            // 期间必须给出可见反馈：按钮换成进行时文案 + 转圈、整排置灰，
            // 否则用户会以为点了没反应而反复点击，反而拉起第二个实例。
            {
                let act = action.clone();
                let label = match action.as_str() {
                    "start" => rcgui_i18n::t("启动中…"),
                    "stop" => rcgui_i18n::t("停止中…"),
                    "install" => rcgui_i18n::t("下载校验中…"),
                    "revalidate" => rcgui_i18n::t("校验中…"),
                    "pick_exe" => rcgui_i18n::t("选择中…"),
                    "search_exe" => rcgui_i18n::t("搜索中…"),
                    "search_deep" => rcgui_i18n::t("深度扫描中…"),
                    "pick_history" => rcgui_i18n::t("读取中…"),
                    _ => rcgui_i18n::t("处理中…"),
                }
                .to_string();
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    let g = ui.global::<PluginsG>();
                    g.set_panel_busy_action(act.as_str().into());
                    g.set_panel_busy_label(label.as_str().into());
                });
            }
            match plugins::run_action(ctx, &id, &action, ui).await {
                Ok(()) => {
                    // 动作可能改变了状态与表单，重新拉一次面板；
                    // 进行时状态要等新面板刷上去再清掉，避免中间闪一帧旧按钮。
                    match plugins::build_panel(ctx, &id).await {
                        Ok(rows) => {
                            let _ = ui.upgrade_in_event_loop(move |ui| {
                                let g = ui.global::<PluginsG>();
                                plugins::apply_panel(&ui, &rows);
                                g.set_panel_loading(false);
                                g.set_panel_busy_action("".into());
                                g.set_panel_busy_label("".into());
                            });
                        }
                        Err(_) => {
                            let _ = ui.upgrade_in_event_loop(move |ui| {
                                let g = ui.global::<PluginsG>();
                                g.set_panel_loading(false);
                                g.set_panel_busy_action("".into());
                                g.set_panel_busy_label("".into());
                            });
                        }
                    }
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::PluginsRescan)).await;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let msg2 = msg.clone();
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        let g = ui.global::<PluginsG>();
                        g.set_panel_loading(false);
                        g.set_panel_error(msg2.as_str().into());
                        g.set_panel_busy_action("".into());
                        g.set_panel_busy_label("".into());
                    });
                    toast(ui, 2, rcgui_i18n::f("插件动作失败: {}", &[&(msg)]));
                }
            }
            false
        }
        Cmd::PluginUninstall { idx } => {
            let row = read_ui(ui, move |ui| {
                ui.global::<PluginsG>()
                    .get_rows()
                    .row_data(idx as usize)
                    .map(|r| {
                        (
                            r.id.to_string(),
                            r.name.to_string(),
                            r.path.to_string(),
                            r.version.to_string(),
                        )
                    })
            })
            .await
            .flatten();
            if let Some((id, name, path, version)) = row {
                // 先让插件自己收尾（停进程等），再删本体
                plugins::shutdown_plugin(ctx, &id).await;
                // 卸载后这个 id 不复存在：它挑过的文件授权留着没有任何用处，
                // 只会在用户日后装回同名插件时变成一份来历不明的许可。
                plugin_host::upload::revoke_plugin(&id);
                match plugins::uninstall(ctx, &id, &path) {
                    Ok(removed) => {
                        let recoverable = plugins::ships_with_app(&id);
                        let msg = if recoverable {
                            rcgui_i18n::f(
                                "已卸载「{}」；如需再次使用，可在插件页点「恢复内置」",
                                &[&(name)],
                            )
                        } else {
                            rcgui_i18n::f("已卸载「{}」", &[&(name)])
                        };
                        eprintln!("[plugin] 已卸载 {id}: {}", removed.display());
                        toast(ui, 1, msg);
                        // 先在 UI 线程里「乐观」抹掉这一行：卸载的磁盘动作已经成功，
                        // 列表就不该再显示它。后续重扫只是把这条结果校准成磁盘真相，
                        // 万一重扫没跑到位，用户也不会看到「已卸载但卡片还在」。
                        // 注意：这里是「发出就返回」，不能 await——调用方本身就在事件循环里。
                        let drop_id = id.clone();
                        let restorable =
                            recoverable.then(|| (id.clone(), name.clone(), version.clone()));
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            let g = ui.global::<PluginsG>();
                            g.set_panel_plugin("".into());
                            g.set_expanded(-1);
                            plugins::clear_panel(&ui);
                            remove_plugin_row(&ui, &drop_id);
                            // 卡片一走，挂在卡片上的「恢复内置」按钮也跟着没了，
                            // 顶部那条已卸载提示必须同时出现，否则用户没有回头路。
                            if let Some((rid, rname, rver)) = restorable {
                                add_removed_builtin(&ui, &rid, &rname, &rver);
                            }
                        });
                        // 再按磁盘真相重扫一次（校准条数、已卸载入口、图床面板等）
                        rescan_plugins(ctx, ui).await;
                    }
                    Err(e) => toast(
                        ui,
                        2,
                        rcgui_i18n::f("卸载失败: {}", &[&(format!("{:#}", e))]),
                    ),
                }
            }
            false
        }
        Cmd::PluginRestore { idx } => {
            let row = read_ui(ui, move |ui| {
                ui.global::<PluginsG>()
                    .get_rows()
                    .row_data(idx as usize)
                    .map(|r| (r.id.to_string(), r.name.to_string()))
            })
            .await
            .flatten();
            if let Some((id, name)) = row {
                match plugins::restore_builtin(ctx, &id) {
                    Ok(()) => {
                        toast(ui, 1, rcgui_i18n::f("已恢复内置插件「{}」", &[&(name)]));
                        // 与卸载对称：先立刻把顶部那条提示去掉，再重扫把卡片加回来。
                        let rid = id.clone();
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            remove_removed_builtin(&ui, &rid);
                        });
                        rescan_plugins(ctx, ui).await;
                    }
                    Err(e) => toast(
                        ui,
                        2,
                        rcgui_i18n::f("恢复失败: {}", &[&(format!("{:#}", e))]),
                    ),
                }
            }
            false
        }
        Cmd::PluginRestoreById { id } => {
            let name = plugins::removed_builtins(ctx)
                .into_iter()
                .find(|(bid, _, _)| bid == &id)
                .map(|(_, name, _)| name)
                .unwrap_or_else(|| id.clone());
            match plugins::restore_builtin(ctx, &id) {
                Ok(()) => {
                    toast(ui, 1, rcgui_i18n::f("已恢复内置插件「{}」", &[&(name)]));
                    let rid = id.clone();
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        remove_removed_builtin(&ui, &rid);
                    });
                    rescan_plugins(ctx, ui).await;
                }
                Err(e) => toast(
                    ui,
                    2,
                    rcgui_i18n::f("恢复失败: {}", &[&(format!("{:#}", e))]),
                ),
            }
            false
        }
        Cmd::PluginSearch => {
            let _ = Box::pin(handle_cmd(ctx, ui, Cmd::PluginsRescan)).await;
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
                    // 这里**不写** rclone.url/user/pass/connect_on_start：
                    // 界面上已经没有能改它们的控件了，界面里的副本永远是启动时的旧值，
                    // 写回去只会把用户以前填的外部地址覆盖成我们自己的默认值。
                    cfg.theme_mode = s.get_theme_idx() as u8;
                    cfg.accent_idx = s.get_accent_idx() as u8;
                    cfg.language = lang_idx_to_tag(s.get_language_idx()).to_string();
                    cfg.autostart = s.get_autostart();
                    cfg.silent_start = s.get_silent();
                    // 用户在设置页动过「关闭到托盘」复选框 = 已经做过选择，
                    // 之后关窗不再弹首次提示（用户明确要求：操作过就置 flag）。
                    let want_tray = s.get_close_to_tray();
                    if want_tray != cfg.close_to_tray {
                        cfg.close_to_tray = want_tray;
                        cfg.close_prompt_done = true;
                    }
                    cfg.http.enabled = s.get_http_enabled();
                    cfg.http.port = s.get_http_port().parse().unwrap_or(5273);
                    cfg.winget_mirror_source = s.get_winget_mirror_source().to_string();
                    set_autostart(cfg.autostart, cfg.silent_start);
                    apply_theme(&ui, cfg.theme_mode);
                    ui.global::<ThemeG>().set_accent_idx(cfg.accent_idx as i32);
                    *ctx.cfg.lock().unwrap() = cfg.clone();
                    config::save(&cfg).map_err(|e| e.to_string())
                }
            })
            .await;
            // 语言改动立即生效（不重建窗口）：先切语言，后面所有提示与刷新都用新语言。
            // 接着重发一遍各页面的刷新命令——已经由 Rust 写进界面属性的动态文案
            // 不会随 `@tr` 重算而更新。
            let want_lang = {
                let tag = ctx.cfg.lock().unwrap().language.clone();
                resolve_lang(&tag)
            };
            if want_lang != rcgui_i18n::lang() {
                rcgui_i18n::set_lang(want_lang);
                // 译文索引是**界面线程本地**的：`slint::select_bundled_translation`
                // 在工作线程里调只会拿到 `NoTranslationsBundled`（GLOBAL_CONTEXT 不在
                // 这个线程上），界面一动不动。这条命令由 tokio 派生在工作线程执行，
                // 所以必须投回界面线程。刷新命令不依赖它完成，不用等回执。
                if let Err(e) =
                    slint::invoke_from_event_loop(move || apply_slint_language(want_lang))
                {
                    eprintln!("界面语言切换无法投递到界面线程: {e}");
                }
                // 托盘菜单是宿主用 `t()` 在创建时定下的文字，重建不了图标，只能通知
                // 托盘线程把它自己持有的两个菜单项重设一遍。
                TRAY_TEXT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // 各页面顶部的状态条、列表里的状态词、插件面板……都是 Rust 侧算好再
                // 写进界面属性的，`@tr` 重算碰不到它们，必须重发一遍加载命令。
                for cmd in [
                    Cmd::RefreshDashboard,
                    Cmd::Foregrounded,
                    Cmd::RemotesRefresh,
                    Cmd::MountsRefresh,
                    Cmd::ServesRefresh,
                    Cmd::TransfersEnter,
                    Cmd::ProxyListRefresh,
                    Cmd::PluginsRescan,
                    Cmd::OptionsLoad,
                ] {
                    let _ = Box::pin(handle_cmd(ctx, ui, cmd)).await;
                }
                // 插件面板若正开着，就地重建一遍：面板文案由宿主按插件配置合成，
                // 不重跑就还是旧语言。
                let open_panel = read_ui(ui, |u| {
                    let g = u.global::<PluginsG>();
                    let id = g.get_panel_plugin().to_string();
                    if id.is_empty() {
                        None
                    } else {
                        g.get_rows()
                            .iter()
                            .position(|r| r.id.as_str() == id.as_str())
                            .map(|i| i as i32)
                    }
                })
                .await;
                if let Some(Some(idx)) = open_panel {
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::PluginPanelOpen { idx })).await;
                }
            }
            match save_result {
                Some(Ok(())) => toast(ui, 1, rcgui_i18n::t("设置已保存（连接相关改动重启后生效）")),
                Some(Err(e)) => toast(ui, 2, rcgui_i18n::f("配置保存失败: {}", &[&(e)])),
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
            rehandshake(ctx, ui).await;
            false
        }
        Cmd::ConfExport {
            path,
            include_secrets,
        } => {
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
                    toast(ui, 1, rcgui_i18n::f("预览完成（尚未写入）：{}", &[&(info)]));
                }
                Err(e) => {
                    *ctx.pending_import.lock().unwrap() = None;
                    let _ = ui.upgrade_in_event_loop(|ui| {
                        let g = ui.global::<SettingsG>();
                        g.set_conf_pending(false);
                        g.set_conf_info(rcgui_i18n::t("导入预览失败，详见提示").into());
                    });
                    toast(ui, 2, rcgui_i18n::f("导入预览失败: {}", &[&(e)]));
                }
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
                toast(ui, 0, rcgui_i18n::t("当前已是该权限，无需重启"));
                return false;
            }
            let msg = if target {
                rcgui_i18n::t("软件将重启，并以管理员身份运行。若弹出 UAC 提示，请选择「是」。")
            } else {
                rcgui_i18n::t("软件将重启，并以普通权限运行。")
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
            toast(ui, 0, rcgui_i18n::t("已取消，保持当前权限"));
            false
        }
        Cmd::TrayFailed(msg) => {
            // 托盘失败只在界面里提示（不弹窗打断），并记进历史记录。
            toast(ui, 2, msg);
            false
        }
        Cmd::ConfApply => {
            let Some(conf) = ctx.pending_import.lock().unwrap().take() else {
                toast(ui, 0, rcgui_i18n::t("请先「导入预览」，确认无误后再应用"));
                return false;
            };
            let mut cfg = ctx.cfg.lock().unwrap().clone();
            match conf_io::apply(&conf, &mut cfg) {
                Ok(bak) => {
                    *ctx.cfg.lock().unwrap() = cfg.clone();
                    // 包里带的插件设置（含已保存的表单）写回 kv。不写的话导入会显示
                    // "成功"，插件却还跑旧配置；脱敏导出缺的键一律保留本机值。
                    let plugin_touched =
                        crate::plugins::apply_imported_settings(&ctx.store, &conf.plugins);
                    if !plugin_touched.is_empty() {
                        crate::logging::event(
                            "info",
                            "import",
                            &format!("plugin settings restored: {}", plugin_touched.join(",")),
                        );
                    }
                    // 写入档案后，再通过 RC 把远程/挂载/服务真正应用到本地 rclone。
                    // 这一步可能很慢（逐条建远程、挂载、起服务），因此全程显示 spinner，
                    // 让用户知道「正在做什么」而不是点完没反应。
                    let (ok_r, ok_m, ok_s, errs) = {
                        let _busy = RemotesBusyGuard::new(ui);
                        apply_bundle_to_rclone(ctx, &conf).await
                    };
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::RemotesRefresh)).await;
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::MountsRefresh)).await;
                    let _ = Box::pin(handle_cmd(ctx, ui, Cmd::ServesRefresh)).await;
                    let mut info = rcgui_i18n::f(
                        "已应用到本地 rclone：远程 {} / 挂载 {} / 服务 {}；原档案备份在 {}",
                        &[&(ok_r), &(ok_m), &(ok_s), &(bak.display())],
                    );
                    if !plugin_touched.is_empty() {
                        // 说了"应用成功"就得让用户知道插件配置也被换了，否则他去改插件设置会看到意料之外的值。
                        info.push_str(&rcgui_i18n::f(
                            "；插件设置已恢复 {} 项",
                            &[&(plugin_touched.len())],
                        ));
                    }
                    if !errs.is_empty() {
                        info.push_str(&rcgui_i18n::f(
                            "；失败 {} 项：{}",
                            &[&(errs.len()), &(errs.join("；"))],
                        ));
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
                    toast(
                        ui,
                        2,
                        rcgui_i18n::f("应用导入失败（数据未改动）: {}", &[&(e)]),
                    );
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

/// 重扫栅栏：记录最后发起的重扫序号，保证只有最新的那次能写列表。
///
/// 见 [`rescan_plugins`]：它解决「先发起的慢重扫后返回、把旧列表盖回新的上面」。
static NEXT_RESCAN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn rescan_plugins(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    // 重扫是「先异步扫盘、再回 UI 线程写列表」。如果两次重扫并发（页面刚打开时
    // 一次、卸载后又一次），先发起的那次可能后返回，把旧列表盖回新的上面——
    // 表现就是「卸载成功了但卡片还在」，点一下刷新又正常。
    // 用递增序号做栅栏：只有最后一次发起的重扫才有权写列表。
    let seq = NEXT_RESCAN_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let all = plugins::scan_rows(ctx).await;
    if NEXT_RESCAN_SEQ.load(std::sync::atomic::Ordering::SeqCst) != seq {
        return; // 已有更新的重扫发起，这份结果过期，丢弃
    }
    let needle = read_ui(ui, |ui| ui.global::<PluginsG>().get_search().to_string())
        .await
        .unwrap_or_default();
    let rows = plugins::filter_rows(&all, &needle);
    let dir_text = plugins::plugins_dir().display().to_string();
    // 已卸载但仍可恢复的内置插件：列表里已经没有它们的卡片，
    // 只能靠顶部这一条入口把「恢复内置」按钮留住。
    let removed = plugins::removed_builtins(ctx);
    if NEXT_RESCAN_SEQ.load(std::sync::atomic::Ordering::SeqCst) != seq {
        return; // 期间又有更新的重扫发起，别再往下写
    }
    // `PluginRow` 含 `ModelRc`（Rc），不是 Send：只把纯数据的 `Row` 移进闭包，
    // 在 UI 线程内部再转成 Slint 结构体。
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let g = ui.global::<PluginsG>();
        let list = plugins::rows_to_slint(&rows);
        g.set_rows(ModelRc::from(Rc::new(VecModel::from(list))));
        g.set_dir(dir_text.into());
        let rb = removed
            .iter()
            .map(|(id, name, version)| crate::RemovedRow {
                id: id.as_str().into(),
                name: name.as_str().into(),
                version: version.as_str().into(),
            })
            .collect::<Vec<_>>();
        g.set_removed_builtins(ModelRc::from(Rc::new(VecModel::from(rb))));
    });
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
        .set_title(rcgui_i18n::t("选择插件包（.zip）"))
        .add_filter(rcgui_i18n::t("插件包"), &["zip"])
        .pick_file()
    else {
        return;
    };

    // 1) 文件大小判定（太小/太大都说明选错了目标）
    let size = match std::fs::metadata(&path) {
        Ok(m) => m.len(),
        Err(e) => {
            toast(ui, 2, rcgui_i18n::f("无法读取文件信息: {}", &[&(e)]));
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
            toast(ui, 2, rcgui_i18n::f("读取文件失败: {}", &[&(e)]));
            return;
        }
    };
    let root = plugins_dir();
    let tmp = plugins_tmp_dir();
    let gv = gui_version();
    let res = tokio::task::spawn_blocking(move || {
        // 先按新格式（config.json / check.sha256 / main.wasm）装；
        // 只有它明确判定「这不是新格式包」时才回落到旧格式（plugin.json / manifest.sha256）。
        match plugin_host::bundle::install_zip(&data, &root, &tmp, &gv) {
            Ok(outcome) => Ok(outcome),
            Err(new_err) => match plugin_host::package::install_from_zip(&data, &root, &tmp, &gv) {
                Ok(outcome) => Ok(outcome),
                // 两种格式都不认：报新格式的错误（用户手上的多半是新格式包）
                Err(_) => Err(new_err),
            },
        }
    })
    .await;

    match res {
        Ok(Ok(outcome)) => {
            // 卸载过又装回来：把当时备份的设置放回原位（只补空位，不覆盖新填的值）。
            crate::plugins::restore_backup_after_install(&outcome);
            toast(ui, 1, outcome.message());
            rescan_plugins(ctx, ui).await;
        }
        Ok(Err(e)) => toast(
            ui,
            2,
            rcgui_i18n::f("添加插件失败: {}", &[&(format!("{:#}", e))]),
        ),
        Err(e) => toast(ui, 2, rcgui_i18n::f("安装线程异常: {}", &[&(e)])),
    }
}

/// 选择一个**已解压**的插件文件夹并安装。
async fn plugin_add_from_dir(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    let Some(dir) = rfd::FileDialog::new()
        .set_title(rcgui_i18n::t("选择已解压的插件文件夹"))
        .pick_folder()
    else {
        return;
    };
    let root = plugins_dir();
    let gv = gui_version();
    let res = tokio::task::spawn_blocking(move || {
        // 同 plugin_add_from_file：新格式优先，旧格式回落
        match plugin_host::bundle::install_dir(&dir, &root, &gv, false) {
            Ok(outcome) => Ok(outcome),
            Err(new_err) => match plugin_host::package::install_verified_dir(&dir, &root, &gv) {
                Ok(outcome) => Ok(outcome),
                Err(_) => Err(new_err),
            },
        }
    })
    .await;

    match res {
        Ok(Ok(outcome)) => {
            // 卸载过又装回来：把当时备份的设置放回原位（只补空位，不覆盖新填的值）。
            crate::plugins::restore_backup_after_install(&outcome);
            toast(ui, 1, outcome.message());
            rescan_plugins(ctx, ui).await;
        }
        Ok(Err(e)) => toast(
            ui,
            2,
            rcgui_i18n::f("添加插件失败: {}", &[&(format!("{:#}", e))]),
        ),
        Err(e) => toast(ui, 2, rcgui_i18n::f("安装线程异常: {}", &[&(e)])),
    }
}

// ---------------------------------------------------------------- 图床面板

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
    // 维护页的所有按钮都只发这一个回调：第一个参数是动作名，第二个是它作用的条目
    // （商店插件 id / 同步任务 id），没有就是空串。真正的分派在 `maintenance::handle`。
    ui.on_maintenance_action({
        let tx = tx.clone();
        move |action, id| {
            let _ = tx.send(Cmd::Maint {
                action: action.to_string(),
                id: id.to_string(),
            });
        }
    });
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
                .set_title(rcgui_i18n::t("保存文件到…"))
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
            let (name, new_name, is_dir) = ui
                .upgrade()
                .map(|ui| {
                    let g = ui.global::<FilesG>();
                    let row = g.get_entries().row_data(i as usize);
                    let name = row.as_ref().map(|r| r.name.to_string()).unwrap_or_default();
                    let is_dir = row.as_ref().map(|r| r.is_dir).unwrap_or(false);
                    (name, g.get_rename_name().to_string(), is_dir)
                })
                .unwrap_or_default();
            let _ = tx.send(Cmd::FilesRename {
                from: name,
                to: new_name,
                is_dir,
            });
        }
    });
    ui.on_files_copy({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let (name, is_dir) = ui
                .upgrade()
                .and_then(|ui| ui.global::<FilesG>().get_entries().row_data(i as usize))
                .map(|r| (r.name.to_string(), r.is_dir))
                .unwrap_or_default();
            let _ = tx.send(Cmd::FilesCopy { name, is_dir });
        }
    });
    // ---- 会话 18：上传 / 排序 / 多选 ----
    ui.on_files_upload_pick({
        let tx = tx.clone();
        move || {
            // 打开资源管理器选文件（rfd 的系统文件选择器）。多选是默认支持的。
            let paths = rfd::FileDialog::new()
                .set_title(rcgui_i18n::t("选择要上传的文件"))
                .pick_files()
                .unwrap_or_default();
            if paths.is_empty() {
                return;
            }
            let _ = tx.send(Cmd::FilesUpload { paths, dest: None });
        }
    });
    ui.on_files_sort_by({
        let tx = tx.clone();
        move |key| {
            let _ = tx.send(Cmd::FilesSort {
                key: key.to_string(),
            });
        }
    });
    ui.on_files_multi_toggle({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::FilesSelect { mode: 3 });
        }
    });
    ui.on_files_select_all({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::FilesSelect { mode: 1 });
        }
    });
    ui.on_files_select_invert({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::FilesSelect { mode: 2 });
        }
    });
    ui.on_files_check_row({
        let tx = tx.clone();
        move |idx, shift| {
            let _ = tx.send(Cmd::FilesCheck { idx, shift });
        }
    });
    ui.on_files_download_many({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let items = checked_items(&ui);
            if items.is_empty() {
                return;
            }
            // 与单行「下载到…」一致：选目录，不选文件名
            if let Some(dir) = rfd::FileDialog::new()
                .set_title(rcgui_i18n::t("选择下载目录"))
                .pick_folder()
            {
                let _ = tx.send(Cmd::FilesDownloadMany { items, dir });
            }
        }
    });
    ui.on_files_del_many({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let items = checked_items(&ui);
            if items.is_empty() {
                return;
            }
            // 确认框（FilesView 里的 delmp）已经在界面上点过，这里直接执行
            let _ = tx.send(Cmd::FilesDelMany { items });
        }
    });
    // ---- 会话 18：本机窗格（双栏左半）----
    ui.on_local_refresh({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::LocalRefresh { path: None });
        }
    });
    ui.on_local_open({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let name = ui
                .upgrade()
                .and_then(|ui| ui.global::<LocalG>().get_entries().row_data(i as usize))
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            if name.is_empty() {
                return;
            }
            let _ = tx.send(Cmd::LocalEnter { name });
        }
    });
    ui.on_local_up({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::LocalUp);
        }
    });
    ui.on_local_choose_dir({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::LocalChooseDir);
        }
    });
    ui.on_local_upload({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let (name, is_dir) = ui
                .upgrade()
                .and_then(|ui| ui.global::<LocalG>().get_entries().row_data(i as usize))
                .map(|r| (r.name.to_string(), r.is_dir))
                .unwrap_or_default();
            if name.is_empty() {
                return;
            }
            let _ = tx.send(Cmd::LocalUpload { name, is_dir });
        }
    });
    ui.on_files_crumb_nav({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |path| {
            let Some(ui) = ui.upgrade() else { return };
            let g = ui.global::<FilesG>();
            if g.get_loading() {
                return;
            }
            // 面包屑点进来的是规范路径；路径框手输的要宽松处理：
            // 去首尾空白、反斜杠转正斜杠、去掉「远程名:」前缀与开头的 "/"。
            let mut p = path.trim().replace('\\', "/");
            let cur_remote = g
                .get_remotes()
                .row_data(g.get_remote_idx().max(0) as usize)
                .map(|r| r.to_string())
                .unwrap_or_default();
            if !cur_remote.is_empty() {
                if let Some(rest) = p.strip_prefix(&format!("{cur_remote}:")) {
                    p = rest.to_string();
                }
            }
            while let Some(rest) = p.strip_prefix('/') {
                p = rest.to_string();
            }
            while p.len() > 1 && p.ends_with('/') {
                p.pop();
            }
            g.set_loading(true);
            g.set_seq(g.get_seq() + 1);
            let _ = tx.send(Cmd::FilesJump {
                path: p,
                seq: g.get_seq(),
            });
        }
    });
    ui.on_files_crumb_refit({
        let ui = ui.as_weak();
        move |w| {
            let Some(ui) = ui.upgrade() else { return };
            let g = ui.global::<FilesG>();
            g.set_path_box_width(w);
            let all = collect_crumbs(&g.get_crumb_all());
            apply_crumbs(&ui, all);
        }
    });
    ui.on_files_download({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |i| {
            let row = ui
                .upgrade()
                .and_then(|ui| ui.global::<FilesG>().get_entries().row_data(i as usize))
                .map(|r| (r.name.to_string(), r.is_dir));
            let Some((name, is_dir)) = row else { return };
            if let Some(dir) = rfd::FileDialog::new()
                .set_title(rcgui_i18n::t("选择下载目录"))
                .pick_folder()
            {
                let _ = tx.send(Cmd::FilesDownloadToDir { name, dir, is_dir });
            }
        }
    });
    // 「移动到…」的浏览：进入子目录 / 上级 / 根目录共用一段逻辑
    let move_go = {
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |path: String| {
            let Some(ui) = ui.upgrade() else { return };
            let g = ui.global::<FilesG>();
            g.set_move_busy(true);
            g.set_move_seq(g.get_move_seq() + 1);
            let _ = tx.send(Cmd::FilesMoveList {
                path,
                seq: g.get_move_seq(),
            });
        }
    };
    ui.on_files_move_list({
        let move_go = move_go.clone();
        move |path| move_go(path.to_string())
    });
    ui.on_files_move_enter({
        let move_go = move_go.clone();
        let ui = ui.as_weak();
        move |name| {
            let path = ui
                .upgrade()
                .map(|ui| {
                    let g = ui.global::<FilesG>();
                    join_path(&g.get_move_path(), &name)
                })
                .unwrap_or_default();
            move_go(path);
        }
    });
    ui.on_files_move_up({
        let move_go = move_go.clone();
        let ui = ui.as_weak();
        move || {
            let path = ui
                .upgrade()
                .map(|ui| parent_path(&ui.global::<FilesG>().get_move_path()))
                .unwrap_or_default();
            move_go(path);
        }
    });
    ui.on_files_move_root({
        let move_go = move_go.clone();
        move || move_go(String::new())
    });
    ui.on_files_move_confirm({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let g = ui.global::<FilesG>();
            let _ = tx.send(Cmd::FilesMove {
                name: g.get_move_name().to_string(),
                is_dir: g.get_move_is_dir(),
                dest: g.get_move_path().to_string(),
            });
        }
    });
    // ---- 会话 19：右键「下载到当前本地目录」/ 本机多选 / 本机排序 / 上传到… / 拖拽 ----
    ui.on_files_download_here({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let fg = ui.global::<FilesG>();
            let lg = ui.global::<LocalG>();
            let dir = PathBuf::from(lg.get_path().to_string());
            if dir.as_os_str().is_empty() {
                toast_ui(&ui, 2, rcgui_i18n::t("左侧本机目录还是空的，请先选择目录"));
                return;
            }
            // 右键点的是已勾选的一行 → 下整组；否则只下这一行（修「多选只下一个」的 bug）
            let multi_take = fg.get_multi()
                && fg
                    .get_checked()
                    .row_data(fg.get_op_idx().max(0) as usize)
                    .unwrap_or(false);
            if multi_take {
                let items = checked_items(&ui);
                if items.is_empty() {
                    return;
                }
                let _ = tx.send(Cmd::FilesDownloadMany { items, dir });
                return;
            }
            let idx = fg.get_op_idx();
            let Some(row) = fg.get_entries().row_data(idx.max(0) as usize) else {
                return;
            };
            let _ = tx.send(Cmd::FilesDownloadToDir {
                name: row.name.to_string(),
                dir,
                is_dir: row.is_dir,
            });
        }
    });
    ui.on_local_check_row({
        let tx = tx.clone();
        move |idx, shift| {
            let _ = tx.send(Cmd::LocalCheck { idx, shift });
        }
    });
    ui.on_local_multi_toggle({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::LocalSelect { mode: 3 });
        }
    });
    ui.on_local_select_all({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::LocalSelect { mode: 1 });
        }
    });
    ui.on_local_select_invert({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::LocalSelect { mode: 2 });
        }
    });
    ui.on_local_sort_by({
        let tx = tx.clone();
        move |key| {
            let _ = tx.send(Cmd::LocalSort {
                key: key.to_string(),
            });
        }
    });
    // 本机多选「上传」：勾选项一次性传到远程当前目录
    ui.on_local_upload_many({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let items = local_checked_items(&ui);
            if items.is_empty() {
                return;
            }
            // 复用「上传到当前远程目录」：拼成本机绝对路径交给 FilesUpload
            let cur = PathBuf::from(ui.global::<LocalG>().get_path().to_string());
            let paths: Vec<PathBuf> = items.iter().map(|(name, _)| cur.join(name)).collect();
            let _ = tx.send(Cmd::FilesUpload { paths, dest: None });
        }
    });
    // 「上传到…」对话框：列出远程子目录
    let up_go = {
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |path: String| {
            let Some(ui) = ui.upgrade() else { return };
            let g = ui.global::<LocalG>();
            g.set_up_busy(true);
            g.set_up_seq(g.get_up_seq() + 1);
            let _ = tx.send(Cmd::LocalUploadToList {
                path,
                seq: g.get_up_seq(),
            });
        }
    };
    ui.on_local_upload_to({
        let up_go = up_go.clone();
        let ui = ui.as_weak();
        move |i| {
            let Some(ui) = ui.upgrade() else { return };
            let lg = ui.global::<LocalG>();
            // 勾选了多项就传整组，否则只传右键点的那一行
            let hits = lg.get_checked_count();
            lg.set_op_idx(i);
            let name = lg
                .get_entries()
                .row_data(i.max(0) as usize)
                .map(|r| (r.name.to_string(), r.is_dir));
            let Some((name, is_dir)) = name else { return };
            lg.set_op_name(name.as_str().into());
            lg.set_op_is_dir(is_dir);
            if hits <= 0 {
                // 单选：先把自己勾上，让「上传到…」只有一个统一的数据来源
                let n = lg.get_entries().row_count();
                let mut flags: Vec<bool> = (0..n)
                    .map(|k| lg.get_checked().row_data(k).unwrap_or(false))
                    .collect();
                flags[i.max(0) as usize] = true;
                let cnt = flags.iter().filter(|v| **v).count() as i32;
                lg.set_checked(ModelRc::from(Rc::new(VecModel::from(flags))));
                lg.set_checked_count(cnt);
            }
            let count = lg.get_checked_count().max(1);
            lg.set_up_count(count);
            lg.set_up_dirs(ModelRc::from(Rc::new(
                VecModel::from(Vec::<MoveDir>::new()),
            )));
            lg.set_up_path("".into());
            // 从远程根开始浏览
            up_go(String::new());
        }
    });
    ui.on_local_upload_to_list({
        let up_go = up_go.clone();
        move |path| up_go(path.to_string())
    });
    ui.on_local_upload_to_enter({
        let up_go = up_go.clone();
        let ui = ui.as_weak();
        move |name| {
            let path = ui
                .upgrade()
                .map(|ui| join_path(&ui.global::<LocalG>().get_up_path(), &name))
                .unwrap_or_default();
            up_go(path);
        }
    });
    ui.on_local_upload_to_up({
        let up_go = up_go.clone();
        let ui = ui.as_weak();
        move || {
            let path = ui
                .upgrade()
                .map(|ui| parent_path(&ui.global::<LocalG>().get_up_path()))
                .unwrap_or_default();
            up_go(path);
        }
    });
    ui.on_local_upload_to_root({
        let up_go = up_go.clone();
        move || up_go(String::new())
    });
    ui.on_local_upload_to_confirm({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let lg = ui.global::<LocalG>();
            let dest = lg.get_up_path().to_string();
            // 勾选项（单选时 on_local_upload_to 已经把自己勾上了）
            let items = local_checked_items(&ui);
            if items.is_empty() {
                return;
            }
            // 直接走「上传到指定目录」的命令，避免二次弹框
            let _ = tx.send(Cmd::LocalUploadTo { dest });
        }
    });
    // ---- 拖拽 ----
    // 载荷只携带 "local:<行号>" / "remote:<行号>" 这样的描述串，真正的用途列表在落点
    // 回调里按当前勾选状态展开。这样「拖一个已勾选的行 = 拖整组」只有一处实现，
    // 也不依赖 DragArea 在渲染期就能拿到完整路径列表。
    ui.on_local_make_transfer(|token| {
        let mut dt = slint::DataTransfer::default();
        dt.set_plain_text(token);
        dt
    });
    ui.on_local_accept_remote(|dt| {
        dt.plain_text()
            .map(|t| t.starts_with("remote:"))
            .unwrap_or(false)
    });
    ui.on_local_drop_remote({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |dt| {
            let Some(ui) = ui.upgrade() else { return };
            let text = dt.plain_text().unwrap_or_default();
            let Ok(idx) = text.trim_start_matches("remote:").parse::<i32>() else {
                return;
            };
            let fg = ui.global::<FilesG>();
            let lg = ui.global::<LocalG>();
            let dir = PathBuf::from(lg.get_path().to_string());
            if dir.as_os_str().is_empty() {
                toast_ui(&ui, 2, rcgui_i18n::t("左侧本机目录还是空的，请先选择目录"));
                return;
            }
            // 拖的是已勾选的行 → 整组；否则只拖这一行
            let items = if fg.get_multi()
                && fg
                    .get_checked()
                    .row_data(idx.max(0) as usize)
                    .unwrap_or(false)
            {
                checked_items(&ui)
            } else {
                fg.get_entries()
                    .row_data(idx.max(0) as usize)
                    .map(|r| vec![(r.name.to_string(), r.is_dir)])
                    .unwrap_or_default()
            };
            if items.is_empty() {
                return;
            }
            let _ = tx.send(Cmd::FilesDownloadMany { items, dir });
        }
    });
    ui.on_files_make_transfer(|token| {
        let mut dt = slint::DataTransfer::default();
        dt.set_plain_text(token);
        dt
    });
    ui.on_files_accept_local(|dt| {
        dt.plain_text()
            .map(|t| t.starts_with("local:"))
            .unwrap_or(false)
    });
    ui.on_files_drop_local({
        let tx = tx.clone();
        let ui = ui.as_weak();
        move |dt| {
            let Some(ui) = ui.upgrade() else { return };
            let text = dt.plain_text().unwrap_or_default();
            let Ok(idx) = text.trim_start_matches("local:").parse::<i32>() else {
                return;
            };
            let lg = ui.global::<LocalG>();
            let cur = PathBuf::from(lg.get_path().to_string());
            if cur.as_os_str().is_empty() {
                toast_ui(&ui, 2, rcgui_i18n::t("请先在左侧选择一个本机目录"));
                return;
            }
            // 拖的是已勾选的行 → 整组；否则只拖这一行
            let names: Vec<String> = if lg.get_multi()
                && lg
                    .get_checked()
                    .row_data(idx.max(0) as usize)
                    .unwrap_or(false)
            {
                local_checked_items(&ui)
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect()
            } else {
                lg.get_entries()
                    .row_data(idx.max(0) as usize)
                    .map(|r| vec![r.name.to_string()])
                    .unwrap_or_default()
            };
            if names.is_empty() {
                return;
            }
            let paths: Vec<PathBuf> = names.iter().map(|n| cur.join(n)).collect();
            let _ = tx.send(Cmd::FilesUpload { paths, dest: None });
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
    // 一键安装 WinFsp：源（0 官方 / 1 镜像）由横幅上的两个小按钮选，
    // 点安装时把当前选择直接带进来，省得再读一遍全局。
    ui.on_install_winfsp({
        let tx = tx.clone();
        move |src| {
            let _ = tx.send(Cmd::WinFspInstall { mirror: src == 1 });
        }
    });
    ui.on_mount_remove({
        let tx = tx.clone();
        move |point| {
            let _ = tx.send(Cmd::MountRemove {
                point: point.to_string(),
            });
        }
    });
    ui.on_mount_path_nav({
        let tx = tx.clone();
        move |name| {
            let _ = tx.send(Cmd::HistPick {
                scope: 0,
                path: name.to_string(),
            });
        }
    });
    ui.on_serve_path_nav({
        let tx = tx.clone();
        move |name| {
            let _ = tx.send(Cmd::HistPick {
                scope: 1,
                path: name.to_string(),
            });
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
            let _ = tx.send(Cmd::BrowseGoto {
                name: name.to_string(),
            });
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
                0 => {
                    let _ = tx.send(Cmd::RefreshDashboard);
                }
                1 => {
                    let _ = tx.send(Cmd::FilesInit);
                    // 双栏左半（本机）也要有一次初始列目录
                    let _ = tx.send(Cmd::LocalRefresh { path: None });
                }
                2 => {
                    let _ = tx.send(Cmd::RemotesRefresh);
                }
                3 => {
                    let _ = tx.send(Cmd::MountsRefresh);
                }
                4 => {
                    let _ = tx.send(Cmd::ServesRefresh);
                }
                5 => {
                    let _ = tx.send(Cmd::TransfersEnter);
                }
                7 => {
                    let _ = tx.send(Cmd::OptionsLoad);
                }
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
    ui.on_remote_type_search({
        let tx = tx.clone();
        move |q: slint::SharedString| {
            let _ = tx.send(Cmd::RemoteTypeSearch(q.to_string()));
        }
    });
    // ---------------- 代理 ----------------
    ui.on_proxy_changed({
        let tx = tx.clone();
        move |remote, idx| {
            let _ = tx.send(Cmd::ProxyBind {
                remote: remote.to_string(),
                idx,
            });
        }
    });
    ui.on_proxy_add({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::ProxyEditLoad { id: String::new() });
        }
    });
    ui.on_proxy_edit({
        let tx = tx.clone();
        move |id| {
            let _ = tx.send(Cmd::ProxyEditLoad { id: id.to_string() });
        }
    });
    ui.on_proxy_save({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::ProxySave);
        }
    });
    ui.on_proxy_cancel({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::ProxyCancel);
        }
    });
    ui.on_proxy_del({
        let tx = tx.clone();
        move |id| {
            // 先问后删：由 ProxyDeleteAsk 算出「哪些 remote 在用」并弹确认，
            // 用户确认后才发 ProxyDelete。用户明确要求删除被使用的配置时必须提示关联。
            let _ = tx.send(Cmd::ProxyDeleteAsk { id: id.to_string() });
        }
    });
    ui.on_proxy_del_confirm({
        let tx = tx.clone();
        move |id| {
            let _ = tx.send(Cmd::ProxyDelete { id: id.to_string() });
        }
    });
    ui.on_proxy_refresh({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::ProxyListRefresh);
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
            // 标题栏的关闭按钮与系统关闭按钮走同一条路：
            // 由 WindowClose 按「关闭到托盘」设置决定隐藏 / 询问 / 退出
            let _ = tx.send(Cmd::WindowClose);
        }
    });
    ui.on_close_prompt_confirm({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::CloseToTrayConfirm);
        }
    });
    ui.on_close_prompt_cancel({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::CloseToTrayCancel);
        }
    });
    ui.on_close_prompt_quit({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::CloseToTrayQuit);
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
    // ---- 内置插件「一键 rclone」----
    ui.on_plugin_panel_open({
        let tx = tx.clone();
        move |idx| {
            let _ = tx.send(Cmd::PluginPanelOpen { idx });
        }
    });
    ui.on_plugin_panel_close({
        let tx = tx.clone();
        move || {
            let _ = tx.send(Cmd::PluginPanelClose);
        }
    });
    ui.on_plugin_panel_field({
        let tx = tx.clone();
        move |key, value| {
            let _ = tx.send(Cmd::PluginPanelField {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
    });
    ui.on_plugin_panel_pick({
        let tx = tx.clone();
        move |key, value| {
            let _ = tx.send(Cmd::PluginPanelPick {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
    });
    ui.on_plugin_panel_action({
        let tx = tx.clone();
        move |action| {
            let _ = tx.send(Cmd::PluginPanelAction {
                action: action.to_string(),
            });
        }
    });
    ui.on_plugin_uninstall({
        let tx = tx.clone();
        move |idx| {
            let _ = tx.send(Cmd::PluginUninstall { idx });
        }
    });
    ui.on_plugin_restore({
        let tx = tx.clone();
        move |idx| {
            let _ = tx.send(Cmd::PluginRestore { idx });
        }
    });
    ui.on_plugin_restore_by_id({
        let tx = tx.clone();
        move |id| {
            let _ = tx.send(Cmd::PluginRestoreById { id: id.to_string() });
        }
    });
    ui.on_plugin_search({
        let tx = tx.clone();
        move |_q| {
            let _ = tx.send(Cmd::PluginSearch);
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
                    rcgui_i18n::t("导出 rcgui.conf（含凭据）")
                } else {
                    rcgui_i18n::t("导出 rcgui.conf（不含凭据）")
                })
                .save_file()
            {
                let _ = tx.send(Cmd::ConfExport {
                    path,
                    include_secrets,
                });
            }
        }
    });
    ui.on_conf_import_preview({
        let tx = tx.clone();
        move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("rcgui.conf", &["conf", "json"])
                .set_title(rcgui_i18n::t("选择要导入的 rcgui.conf"))
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
                .set_title(rcgui_i18n::t("导出 rcgui.conf（含凭据）"))
                .save_file()
            {
                let _ = tx.send(Cmd::ConfExport {
                    path,
                    include_secrets: true,
                });
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
    }); // 「选择 rclone.exe」已移除：RCGUI 不再启动 rclone，不需要它的路径。

    // 初始加载
    let _ = tx.send(Cmd::RefreshDashboard);
    let _ = tx.send(Cmd::FilesInit);
    let _ = tx.send(Cmd::RemotesRefresh);
    let _ = tx.send(Cmd::MountsRefresh);
    let _ = tx.send(Cmd::PluginsRescan);
    // 代理配置表也要在启动时拉一次：远程行的下拉是从 `proxy-options` 渲染的，
    // 而「代理配置」卡片用的是 `proxy-rows`。少了这一条，重启后会看到
    // 远程行显示着代理名、下面的卡片却是「代理配置（0）」，必须手动点刷新才恢复。
    let _ = tx.send(Cmd::ProxyListRefresh);
}

// ---------------- 托盘 ----------------

/// 托盘菜单文案的「代」计数。
///
/// 托盘线程自己跑消息循环，宿主没有别的通道通知它，切语言时把计数 +1，
/// 线程每次循环比较一下，变了就把菜单项文字重设成新语言。
static TRAY_TEXT_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 托盘图标 + 右键菜单。
///
/// ## 为什么必须在**同一个线程**里创建并跑消息循环
///
/// `tray-icon` 的 Windows 后端只做了两件事：`CreateWindowExW` 建一个隐藏窗口，
/// 然后用 `Shell_NotifyIconW` 把 `WM_USER_TRAYICON`(=6002) 作为回调消息注册进去。
/// 它**不运行任何消息循环**（源码里没有 `GetMessage`/`DispatchMessage`）。
///
/// 也就是说：鼠标在托盘图标上的点击，是系统把 `WM_USER_TRAYICON` 投递到那个
/// 隐藏窗口的消息队列里；**只有该线程跑起消息循环**，这些消息才会被取出、
/// 进而被 `tray-icon` 翻译成 `TrayIconEvent`。之前这里只在 `std::thread` 里
/// `try_recv()` 轮询 —— 队列永远收不到东西，所以左右键都没反应。
///
/// 现在的做法：托盘在自己的线程里创建，然后**跑一个真正的 GetMessage 循环**，
/// 事件由 `tray-icon` 通过 channel 送达，我们在循环里定时 try_recv 处理。
fn spawn_tray(ui: Weak<MainWindow>, tx: mpsc::UnboundedSender<Cmd>) {
    let accent = accent_rgb(&crate::config::load());
    std::thread::spawn(move || {
        use tray_icon::menu::{Menu, MenuEvent, MenuItem};
        use tray_icon::{TrayIconBuilder, TrayIconEvent};

        #[cfg(windows)]
        {
            use windows::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
            };

            // 图标与 AppMark 同一造型、跟随强调色
            let icon_rgba = make_icon_rgba(32, accent);
            tray_log("tray: icon rgba built");
            let icon = match tray_icon::Icon::from_rgba(icon_rgba, 32, 32) {
                Ok(i) => i,
                Err(e) => {
                    tray_log(&format!("tray: Icon::from_rgba FAILED: {e}"));
                    return;
                }
            };
            tray_log("tray: icon created");
            // 完整性级别是托盘能否注册的**决定性因素**，先记下来。
            match integrity_rid() {
                Some(rid) => tray_log(&format!("tray: process integrity RID=0x{rid:X}")),
                None => tray_log("tray: process integrity unknown"),
            }

            // 菜单：主界面 / 退出
            let menu = Menu::new();
            let open_item = MenuItem::with_id("open", rcgui_i18n::t("主界面"), true, None);
            let quit_item = MenuItem::with_id("quit", rcgui_i18n::t("退出"), true, None);
            let _ = menu.append_items(&[&open_item, &quit_item]);

            // 关键：`_tray` 必须活到线程结束。写成 `let _tray` 会在作用域结束时
            // drop，图标随之消失；这里显式持有。
            let tray = TrayIconBuilder::new()
                // 右键菜单由系统在图标上弹出；左键双击我们自己处理
                .with_menu(Box::new(menu.clone()))
                .with_tooltip("RCGUI")
                .with_icon(icon)
                .build();
            // 关键：`_tray` 必须活到线程结束 —— 一旦 drop，图标就从托盘消失。
            let _tray = match tray {
                Ok(t) => {
                    tray_log("tray: icon added to tray");
                    t
                }
                Err(e) => {
                    // 托盘注册失败要**明确告知用户**，不能静默。
                    //
                    // 根因（会话 11 实测确认）：托盘图标由 explorer.exe 托管，
                    // 若 **exe 文件本身或所在目录被打了低完整性标签**
                    // （Mandatory Label = Low，SACL 里 `LABEL=0x1000`），
                    // `Shell_NotifyIconW(NIM_ADD)` 直接返回 FALSE + `拒绝访问 (5)`。
                    // 对照实验：同一份 exe 放在 Medium 目录下托盘成功、
                    // 放在 Low 目录下必失败；进程自身 IL 也随之是 0x1000。
                    // 注意 `Shell_NotifyIconW` 失败时并不保证刷新 last error，
                    // 所以 `os error 5` 只是「最可能」而非「必然」的取值。
                    let msg = rcgui_i18n::f(
                        "托盘图标注册失败（{}）。若系统托盘里没有 RCGUI：\
                         通常是程序所在文件/目录被标记为低完整性（Low），\
                         或程序以受限权限运行。已尝试把文件与目录完整性调回普通级别，\
                         若提示里出现「成功」，请**退出并重新启动本程序**后托盘才会出现。",
                        &[&(e)],
                    );
                    tray_log(&format!("tray: build FAILED: {e}"));
                    // 自愈：exe 与目录都调回 Medium。
                    //
                    // 只调目录不够：文件可能自己带着 Low 标签（复制/解压时继承，
                    // 或旧的 setintegritylevel 只作用于目录）。而且**当前进程的
                    // 完整性在启动时就定了**，改标签不会让本进程变回 Medium，
                    // 必须重启程序才生效 —— 提示语里已经写明。
                    //
                    // 若本进程已是 Low，icacls 自己也会被拒（实测 exit 1299），
                    // 这种情况只能如实记录，让用户手动处理。
                    if let Ok(exe) = std::env::current_exe() {
                        use std::os::windows::process::CommandExt;
                        let mut targets = vec![exe.clone()];
                        if let Some(dir) = exe.parent() {
                            targets.push(dir.to_path_buf());
                        }
                        for t in targets {
                            match std::process::Command::new("icacls")
                                .arg(&t)
                                .arg("/setintegritylevel")
                                .arg("Medium")
                                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                                .output()
                            {
                                Ok(o) => tray_log(&format!(
                                    "tray: heal {} -> {:?}",
                                    t.display(),
                                    o.status
                                )),
                                Err(err) => tray_log(&format!(
                                    "tray: heal {} spawn failed: {err}",
                                    t.display()
                                )),
                            }
                        }
                    }
                    let _ = tx.send(Cmd::TrayFailed(msg));
                    return;
                }
            };
            tray_log("tray: entering message loop");

            // 记录上一次左键按下的时间，用于识别「双击」。
            // 不能用 TrayIconEvent::DoubleClick：tray-icon 在 Windows 上
            // 只对部分版本可靠，手动计时更稳（阈值取系统默认的 500ms）。
            let mut last_left_click: Option<std::time::Instant> = None;
            // 菜单文字对应的「代」；切语言时宿主会把 TRAY_TEXT_EPOCH 加一。
            let mut last_text_epoch = TRAY_TEXT_EPOCH.load(std::sync::atomic::Ordering::Relaxed);

            loop {
                // 0) 语言变了就重设菜单项文字（热更新，不重启）。
                let epoch = TRAY_TEXT_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
                if epoch != last_text_epoch {
                    last_text_epoch = epoch;
                    open_item.set_text(rcgui_i18n::t("主界面"));
                    quit_item.set_text(rcgui_i18n::t("退出"));
                }

                // 1) 先把消息队列里的消息泵出去。
                //
                // 用 PeekMessageW（非阻塞）而不是 GetMessageW（阻塞）：
                // 托盘事件由 `tray-icon` 通过 channel 投递，若阻塞在 GetMessageW
                // 里，channel 中的事件要等到下一条窗口消息才会被处理，
                // 表现为「点了要等很久才响应」。
                let mut msg = MSG::default();
                loop {
                    let has = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() };
                    if !has {
                        break;
                    }
                    let _ = unsafe { TranslateMessage(&msg) };
                    unsafe { DispatchMessageW(&msg) };
                }

                // 2) 菜单项点击
                while let Ok(ev) = MenuEvent::receiver().try_recv() {
                    match ev.id.0.as_str() {
                        "open" => {
                            let _ = ui.upgrade_in_event_loop(|ui| {
                                show_main_window(&ui);
                            });
                        }
                        "quit" => {
                            let _ = tx.send(Cmd::Quit);
                        }
                        _ => {}
                    }
                }

                // 3) 托盘图标自身事件
                while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                    if let TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        // 需求：**左键双击**弹出主界面（单击不动作）。
                        let now = std::time::Instant::now();
                        let is_double = last_left_click
                            .map(|t| now.duration_since(t) < std::time::Duration::from_millis(500))
                            .unwrap_or(false);
                        last_left_click = Some(now);
                        if is_double {
                            last_left_click = None; // 复位，避免三击被当成两轮双击
                            let _ = ui.upgrade_in_event_loop(|ui| {
                                show_main_window(&ui);
                            });
                        }
                    }
                }

                // 4) 让出 CPU：既不 busy-loop，也不阻塞消息处理
                std::thread::sleep(std::time::Duration::from_millis(20));
            }

            // 保持 tray 存活到循环结束（循环是死循环，这里仅为语义完整）
            #[allow(unreachable_code)]
            {
                drop(_tray);
            }
        }

        #[cfg(not(windows))]
        {
            let _ = (ui, tx);
        }
    });
}

thread_local! {
    /// 窗口重建后补一次置前用的定时器。复用一个，别每次唤起都漏一个 `Timer`。
    static BRING_FRONT_TIMER: slint::Timer = slint::Timer::default();
}

/// 显示主窗口并置前（托盘「打开」、双击托盘图标、第二个实例要求显示时用）。
///
/// 隐藏时原生窗口是被销毁的（见 `init_destroy_on_hide`），所以 `show()` 之后
/// 那一帧 `with_winit_window` 还是 `None`：HWND 取不到，置前会静默失败。
/// 因此先试一次，取不到就用一次性定时器补一次——和启动时设图标同样的处理。
fn show_main_window(ui: &MainWindow) {
    WINDOW_SHOWN.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = ui.show();
    ui.window().set_minimized(false);
    if !bring_to_front(ui) {
        let ui_weak = ui.as_weak();
        BRING_FRONT_TIMER.with(|timer| {
            timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(120),
                move || {
                    if let Some(u) = ui_weak.upgrade() {
                        let _ = bring_to_front(&u);
                    }
                },
            );
        });
    }
}

/// 拿 HWND 把窗口恢复并拉到前台。窗口还没建出来时返回 false（调用方稍后重试）。
#[cfg(windows)]
fn bring_to_front(ui: &MainWindow) -> bool {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    // Window::window_handle 需要经 with_winit_window 访问
    let hwnd: Option<isize> = ui
        .window()
        .with_winit_window(|w| {
            w.window_handle().ok().and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(h) => Some(h.hwnd.get() as isize),
                _ => None,
            })
        })
        .flatten();
    let Some(v) = hwnd else {
        return false;
    };
    let h = windows::Win32::Foundation::HWND(v as *mut ::core::ffi::c_void);
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            h,
            windows::Win32::UI::WindowsAndMessaging::SW_RESTORE,
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(h);
    }
    true
}

#[cfg(not(windows))]
fn bring_to_front(_ui: &MainWindow) -> bool {
    true
}

/// 取当前主题的强调色（RGB），用于生成图标。
///
/// 与 Slint 的 `ThemeG.accent` 对齐：这里按主题模式 + 强调色索引算出同一份颜色。
fn accent_rgb(cfg: &crate::config::AppConfig) -> [u8; 3] {
    // (浅色, 深色) 两种主题下的强调色，与 theme.slint 的 accent 定义保持一致
    let dark = match cfg.theme_mode {
        1 => false, // 浅色
        2 => true,  // 深色
        _ => true,  // 跟随系统：默认按深色取（差异很小，不影响可辨识度）
    };
    let palette: [(u8, u8, u8, u8, u8, u8); 4] = [
        (0x58, 0xa6, 0xff, 0x09, 0x69, 0xda), // 蓝
        (0x3f, 0xb9, 0x50, 0x1a, 0x7f, 0x37), // 绿
        (0xd2, 0xa8, 0xff, 0x82, 0x50, 0xdf), // 紫
        (0xff, 0xa6, 0x57, 0xbc, 0x4c, 0x00), // 橙
    ];
    let p = palette[(cfg.accent_idx as usize).min(palette.len() - 1)];
    if dark {
        [p.0, p.1, p.2]
    } else {
        [p.3, p.4, p.5]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_rgb_matches_theme_palette() {
        let mut cfg = crate::config::AppConfig::default();
        cfg.theme_mode = 2; // 深色
        cfg.accent_idx = 0; // 蓝
        assert_eq!(accent_rgb(&cfg), [0x58, 0xa6, 0xff]);

        cfg.accent_idx = 1; // 绿
        assert_eq!(accent_rgb(&cfg), [0x3f, 0xb9, 0x50]);

        cfg.theme_mode = 1; // 浅色
        cfg.accent_idx = 0;
        assert_eq!(accent_rgb(&cfg), [0x09, 0x69, 0xda]);

        // 越界索引不能 panic
        cfg.accent_idx = 99;
        let _ = accent_rgb(&cfg);
    }

    /// 回归：`wait_mount_ready_rc` 必须有**绝对时间**的硬上限。
    ///
    /// 早期实现用 `waited += step` / `waited -= step` 控制预算：只要 rclone 每轮都报
    /// 「还挂着」，净增长恒为 0 → 盘符永远不出现时函数无限等待，调用方后面的
    /// 「成功/失败提示 + UI 刷新」都不会执行，界面卡在 spinner 上。
    /// 现在改为 `hard_deadline = start + timeout + EXTEND_CAP`，延长不可能越过上限。
    #[test]
    fn mount_ready_wait_has_hard_cap() {
        let budget = MOUNT_READY_TIMEOUT + MOUNT_READY_EXTEND_CAP;
        assert!(
            budget <= std::time::Duration::from_secs(180),
            "总等待预算必须有明确上限，实际 {budget:?}"
        );
        // 单轮切片必须小于总预算，否则第一轮就可能等到超限
        assert!(std::time::Duration::from_secs(3) < budget);
        // 预算必须大于「正常挂载耗时」，否则正常挂载会被误判失败
        assert!(budget >= std::time::Duration::from_secs(90));
    }

    /// 用挂钟模型复刻该循环：即使每轮都被「rclone 说还挂着」而继续，
    /// 迭代次数也必须有限 —— 因为判断依据是 deadline，而不是可被抵消的累加量。
    #[test]
    fn mount_ready_loop_terminates_under_continuous_extension() {
        let budget = MOUNT_READY_TIMEOUT + MOUNT_READY_EXTEND_CAP;
        let step = std::time::Duration::from_secs(3);
        // 模拟 3 秒一轮，rounds = ceil(budget / step)
        let rounds = (budget.as_secs_f64() / step.as_secs_f64()).ceil() as u64;
        assert!(
            rounds > 0 && rounds < 100,
            "等待轮次应有界且不为 0，实际 {rounds}"
        );

        // 关键对照：旧的累加模型在同一条件下永不收敛
        let mut waited = std::time::Duration::ZERO;
        let mut old_iterations = 0usize;
        loop {
            old_iterations += 1;
            waited += step;
            if waited >= budget {
                break;
            }
            waited = waited.saturating_sub(step); // 每轮都被延长 → 净增长为 0
            if old_iterations > 500 {
                break; // 确认它确实卡住了
            }
        }
        assert!(
            old_iterations > 500,
            "旧实现的净增长模型应表现为不收敛（这就是原 bug）"
        );
    }

    /// 回归：挂载刚发起时若 listmounts 还没收录该条目，不能立刻判定「未生效」。
    ///
    /// 旧实现在一次查询里发现「rclone 不认该挂载点」就直接 return false，
    /// 而实测 listmounts 在挂载刚完成的极短时间内可能仍为空/缺项（抖动），
    /// 于是正常挂载会被误报成「盘符未出现」。现在要求连续多次缺失才放弃。
    #[test]
    fn missing_streak_tolerates_transient_absence() {
        const LIMIT: u32 = 3;
        // 模拟：一次抖动（缺 1 次）之后恢复正常 —— 不应判失败
        let mut streak = 0u32;
        let samples = [false, true, true]; // false = 不在列表里
        let mut failed = false;
        for present in samples {
            if present {
                streak = 0;
            } else {
                streak += 1;
                if streak >= LIMIT {
                    failed = true;
                }
            }
        }
        assert!(!failed, "单次抖动不应判定为失败");

        // 模拟：持续缺失（用户卸载 / 真的没挂上）—— 应当判失败
        let mut streak = 0u32;
        let mut failed = false;
        for _ in 0..LIMIT {
            streak += 1;
            if streak >= LIMIT {
                failed = true;
            }
        }
        assert!(failed, "持续缺失应当判定为失败");
    }

    /// 回归：用户主动卸载后，仍在等待的挂载流程不能报「未生效」。
    ///
    /// 场景：点挂载 → 盘符出现但流程仍在等待循环里 → 用户点卸载 →
    /// rclone 不再列出该挂载点。若不区分「用户取消」，循环醒来会把它当成
    /// 挂载失败并弹错，这就是「卸载后约一秒弹出未生效」的来源。
    #[test]
    fn cancelled_mount_is_not_reported_as_failure() {
        // 用 key 归一化模拟 Ctx.cancelled_mounts 的匹配语义
        let cancelled: Vec<String> = vec![mount_key("Z:\\")];
        let hit = |p: &str| cancelled.iter().any(|k| k == &mount_key(p));
        assert!(hit("Z:"), "卸载时记录的是规范化 key，查询也必须归一化");
        assert!(hit("z"), "\"z\" 与 \"Z:\" 必须视为同一个盘符");
        assert!(!hit("T:"), "其它盘符不应被误命中");

        // 新一次挂载尝试必须清掉旧标记，否则等待循环会直接静默退出
        let mut list: Vec<String> = vec![mount_key("Z:")];
        list.retain(|k| k != &mount_key("Z:\\"));
        assert!(list.is_empty(), "重新挂载前应清掉该盘符的取消标记");
    }

    /// 回归：根目录服务不能给所有传输都打上「来自该服务」的标签。
    ///
    /// 曾经的实现用 `fs.starts_with(serve.fs)` 判断，而根目录服务的 fs 是 `teldrive:`，
    /// 任务的 srcFs 也是 `teldrive:` → 任何该远程的传输都被判定为来自服务，
    /// 且取列表里第一个匹配项（表现为所有任务都是同一个、最上方的服务标签）。
    #[test]
    fn root_serve_does_not_tag_unrelated_transfers() {
        let root_serve = ServeRow {
            id: "webdav-1".into(),
            name: "Serve_01".into(),
            stype: "webdav".into(),
            fs: "teldrive:".into(), // 根目录服务
            addr: "127.0.0.1:8080".into(),
        };
        let serves = vec![root_serve];
        // 普通复制/同步任务，与 serve 无关
        let svc = match_serve_for_task(&serves, "teldrive:", "/local/dir");
        assert!(
            svc.is_empty(),
            "根目录服务不应给普通传输打标签，实际得到 {svc:?}"
        );
        // 上传同理
        let svc = match_serve_for_task(&serves, "/local/file.png", "teldrive:图床/a.png");
        assert!(svc.is_empty(), "根目录服务不应给上传打标签");
    }

    /// 带子路径的服务应能正确匹配，且更具体的子路径优先。
    #[test]
    fn subpath_serve_matches_by_path_prefix() {
        let serves = vec![
            ServeRow {
                id: "s1".into(),
                name: "Media".into(),
                stype: "webdav".into(),
                fs: "teldrive:media".into(),
                addr: "127.0.0.1:9001".into(),
            },
            ServeRow {
                id: "s2".into(),
                name: "MediaMovies".into(),
                stype: "http".into(),
                fs: "teldrive:media/movies".into(),
                addr: "127.0.0.1:9002".into(),
            },
        ];
        // 落在 media/movies 下 → 命中更具体的那个
        assert_eq!(
            match_serve_for_task(&serves, "teldrive:media/movies/a.mkv", ""),
            "MediaMovies"
        );
        // 落在 media 下但不是 movies → 命中较短的
        assert_eq!(
            match_serve_for_task(&serves, "teldrive:media/songs/b.mp3", ""),
            "Media"
        );
        // 完全无关的路径 → 不打标签
        assert!(match_serve_for_task(&serves, "teldrive:other/c.txt", "").is_empty());
        // 不能误匹配前缀相同的兄弟目录："media2" 不是 "media" 的子路径
        assert!(match_serve_for_task(&serves, "teldrive:media2/d.txt", "").is_empty());
    }

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
            secret_set: false,
            secret_editing: false,
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

    /// 「启动时未自动连接」时仪表盘要显示配置里的地址，而不是占位默认值。
    ///
    /// 会话 11 实测：关掉自动连接后仪表盘显示 `:5572`（`Endpoint::loopback_default()`），
    /// 用户配的却是 `:5601` —— 界面在展示一个用户没配过的端口。
    #[test]
    fn startup_endpoint_prefers_configured_url() {
        assert_eq!(
            startup_endpoint_url("http://127.0.0.1:5601", "http://127.0.0.1:5572"),
            "http://127.0.0.1:5601"
        );
        // 前后空白要去掉：配置里手输时很容易带
        assert_eq!(
            startup_endpoint_url("  http://127.0.0.1:5602  ", "http://127.0.0.1:5572"),
            "http://127.0.0.1:5602"
        );
        // 没配 → 回落占位值（不显示空白）
        assert_eq!(
            startup_endpoint_url("", "http://127.0.0.1:5572"),
            "http://127.0.0.1:5572"
        );
        assert_eq!(
            startup_endpoint_url("   ", "http://127.0.0.1:5572"),
            "http://127.0.0.1:5572"
        );
    }

    /// 新建文件夹的重名处理：半角括号 + 空格后缀，大小写不敏感。
    #[test]
    fn unique_dir_name_uses_halfwidth_bracket_suffix() {
        let none: Vec<String> = Vec::new();
        assert_eq!(unique_dir_name("新建文件夹", &none), "新建文件夹");

        let one = vec!["新建文件夹".to_string()];
        assert_eq!(unique_dir_name("新建文件夹", &one), "新建文件夹 (1)");

        let two = vec!["新建文件夹".to_string(), "新建文件夹 (1)".to_string()];
        assert_eq!(unique_dir_name("新建文件夹", &two), "新建文件夹 (2)");
    }

    #[test]
    fn unique_dir_name_skips_taken_suffixes() {
        // (1) 被占但 (2) 空着：必须跳到 (2)，不能重复用 (1)
        let taken = vec!["新建文件夹".into(), "新建文件夹 (1)".into()];
        assert_eq!(unique_dir_name("新建文件夹", &taken), "新建文件夹 (2)");
    }

    #[test]
    fn unique_dir_name_is_case_insensitive() {
        // 部分后端不区分大小写，用 NewFolder 与 newfolder 撞名会让 mkdir 失败
        let taken = vec!["newfolder".into()];
        assert_eq!(unique_dir_name("NewFolder", &taken), "NewFolder (1)");
    }

    #[test]
    fn unique_dir_name_ignores_unrelated_names() {
        let taken = vec!["docs".into(), "新建文件夹 (1)".into()];
        assert_eq!(
            unique_dir_name("新建文件夹", &taken),
            "新建文件夹",
            "只有同名才算占用"
        );
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
        assert!(
            out["ratio"].is_number(),
            "expected number, got {}",
            out["ratio"]
        );
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

    /// 密码留空 = 不修改，即使必填也不能写空串。
    ///
    /// 写空串的后果实测过：rclone 会把空密码混淆后存进配置，于是「保存后
    /// 访问失败」。这里必须整条键都不发。
    #[test]
    fn empty_password_is_never_emitted() {
        let out = fields_to_params(&[
            field("pass", "password", "", true),
            field("pass2", "password", "   ", false),
        ]);
        assert!(
            out.get("pass").is_none(),
            "空密码不该发送，实际 {}",
            out["pass"]
        );
        assert!(out.get("pass2").is_none());
    }

    /// 用户真的输入了新密码时照常发送。
    #[test]
    fn typed_password_is_emitted() {
        let out = fields_to_params(&[field("pass", "password", " s3cret ", false)]);
        assert_eq!(out["pass"], Value::String("s3cret".into()));
    }

    /// 打开已有远程时，密码字段不能把 rclone 的混淆密文塞进输入框。
    #[test]
    fn existing_password_is_not_echoed() {
        let mut fields = vec![field("pass", "password", "", false)];
        let mut existing = serde_json::Map::new();
        // 形如 rclone obscure 产物的合成值，不是任何真实凭据。
        existing.insert(
            "pass".into(),
            Value::String("ObScUrEd-FiXtUrE-vAlUe-NoT-a-ReAl-SeCrEt".into()),
        );
        fill_fields_with_existing(&mut fields, &existing);
        assert_eq!(fields[0].value.as_str(), "", "密文不该回显到输入框");
        assert!(fields[0].secret_set, "应标记为已设置");
        assert!(!fields[0].secret_editing);
        // 未改动就保存 → 不发送 pass，rclone 保持原值
        let out = fields_to_params(&fields);
        assert!(out.get("pass").is_none());
    }

    /// 非密码的 Sensitive 字段（webdav 的 user、s3 的 api_key）必须原样回显：
    /// 它们在配置里是明文，隐藏起来用户就没法改。
    #[test]
    fn existing_sensitive_field_is_echoed() {
        let mut fields = vec![field("user", "text", "", false)];
        let mut existing = serde_json::Map::new();
        existing.insert("user".into(), Value::String("demo-user".into()));
        fill_fields_with_existing(&mut fields, &existing);
        assert_eq!(fields[0].value.as_str(), "demo-user");
        assert!(!fields[0].secret_set);
    }

    /// 掩码只看 IsPassword，不看 Sensitive。
    ///
    /// webdav 的 user 是 Sensitive（导出时抹掉）但不是 IsPassword（不是密码），
    /// pass 反过来。之前用 Sensitive 判断，导致用户名被遮、密码明文。
    #[test]
    fn password_kind_follows_is_password_only() {
        let providers = serde_json::json!({
            "providers": [{
                "Name": "webdav",
                "Description": "WebDAV",
                "Options": [
                    {"Name": "user", "Type": "string", "Sensitive": true, "IsPassword": false, "Default": ""},
                    {"Name": "pass", "Type": "string", "Sensitive": false, "IsPassword": true, "Default": ""},
                    {"Name": "bearer_token", "Type": "string", "Sensitive": true, "Default": ""},
                    {"Name": "url", "Type": "string", "Default": ""}
                ]
            }]
        });
        let (_desc, fields) = build_fields_from_provider(&providers, "webdav");
        let kind = |n: &str| {
            fields
                .iter()
                .find(|f| f.name.as_str() == n)
                .map(|f| f.kind.to_string())
                .unwrap_or_default()
        };
        assert_eq!(kind("pass"), "password");
        assert_eq!(kind("user"), "text");
        assert_eq!(kind("bearer_token"), "text");
        assert_eq!(kind("url"), "text");
    }

    /// Duration / SizeSuffix 必须与 int/float 分开成独立 kind：
    /// 混在 "number" 里会被当成 JSON 数字发出，rclone 用 fmt.Sprint 落盘成
    /// `1e+07`，回读时按秒/KiB 解释，数值被放大若干数量级。
    #[test]
    fn duration_and_size_are_not_number_kind() {
        let providers = serde_json::json!({
            "providers": [{
                "Name": "webdav",
                "Description": "WebDAV",
                "Options": [
                    {"Name": "pacer_min_sleep", "Type": "Duration", "Default": 10000000},
                    {"Name": "nextcloud_chunk_size", "Type": "SizeSuffix", "Default": 10485760},
                    {"Name": "page_size", "Type": "int", "Default": 200},
                    {"Name": "ratio", "Type": "float", "Default": 1.5}
                ]
            }]
        });
        let (_desc, fields) = build_fields_from_provider(&providers, "webdav");
        let get = |n: &str| fields.iter().find(|f| f.name.as_str() == n).cloned();
        assert_eq!(get("pacer_min_sleep").unwrap().kind.to_string(), "duration");
        assert_eq!(
            get("nextcloud_chunk_size").unwrap().kind.to_string(),
            "size"
        );
        assert_eq!(get("page_size").unwrap().kind.to_string(), "number");
        assert_eq!(get("ratio").unwrap().kind.to_string(), "number");
    }

    /// 默认值（纳秒）必须在建表单时就格式化成带单位文本，
    /// 否则用户看到的是裸数字，保存出去又变成科学计数法。
    #[test]
    fn duration_default_is_formatted_with_unit() {
        let providers = serde_json::json!({
            "providers": [{
                "Name": "webdav",
                "Description": "WebDAV",
                "Options": [
                    {"Name": "pacer_min_sleep", "Type": "Duration", "Default": 10000000}
                ]
            }]
        });
        let (_desc, fields) = build_fields_from_provider(&providers, "webdav");
        assert_eq!(fields[0].value.to_string(), "10ms");
    }

    /// Bits（位图）默认值 0 显示成 off：rclone 的 Help 写的是 `Use Off to
    /// disable`，配置文件里也存 off，给用户看一个 0 的数字框没有意义。
    #[test]
    fn bits_default_zero_shows_as_off() {
        let providers = serde_json::json!({
            "providers": [{
                "Name": "s3",
                "Description": "Amazon S3 Compliant Storage Providers",
                "Options": [
                    {"Name": "sdk_log_mode", "Type": "Bits", "Default": 0, "Advanced": true},
                    {"Name": "sdk_log_mode_set", "Type": "Bits", "Default": 3}
                ]
            }]
        });
        let (_desc, fields) = build_fields_from_provider(&providers, "s3");
        assert_eq!(fields[0].value.to_string(), "off");
        // 非 0 的位图保持数字，不能被一并改成 off。
        assert_eq!(fields[1].value.to_string(), "3");
    }

    /// 默认值（字节）同理，必须带 Mi/Gi 之类的单位后缀。
    #[test]
    fn size_default_is_formatted_with_unit() {
        let providers = serde_json::json!({
            "providers": [{
                "Name": "teldrive",
                "Description": "Teldrive",
                "Options": [
                    {"Name": "chunk_size", "Type": "SizeSuffix", "Default": 536870912}
                ]
            }]
        });
        let (_desc, fields) = build_fields_from_provider(&providers, "teldrive");
        assert_eq!(fields[0].value.to_string(), "512Mi");
    }

    /// 字段里的裸数字按基础单位（纳秒/字节）解释，并补上单位后发出。
    #[test]
    fn bare_numbers_are_emitted_with_units() {
        let out = fields_to_params(&[
            field("pacer_min_sleep", "duration", "10000000", false),
            field("nextcloud_chunk_size", "size", "10485760", false),
        ]);
        assert_eq!(out["pacer_min_sleep"], serde_json::json!("10ms"));
        assert_eq!(out["nextcloud_chunk_size"], serde_json::json!("10Mi"));
    }

    /// 已经带单位的写法原样透传，不要自作聪明再换算。
    #[test]
    fn values_with_units_pass_through() {
        let out = fields_to_params(&[
            field("idle_timeout", "duration", "1m0s", false),
            field("upload_cutoff", "size", "200Mi", false),
        ]);
        assert_eq!(out["idle_timeout"], serde_json::json!("1m0s"));
        assert_eq!(out["upload_cutoff"], serde_json::json!("200Mi"));
    }

    /// rclone 用 math.MaxInt64 / 负数表示 "off"，必须原样传回去。
    #[test]
    fn duration_off_is_preserved() {
        assert_eq!(fmt_duration_ns(i64::MAX), "off");
        assert_eq!(fmt_size_bytes(-1), "off");
        let out = fields_to_params(&[field("wait_archive", "duration", "off", false)]);
        assert_eq!(out["wait_archive"], serde_json::json!("off"));
    }

    /// 小于 1KiB 的字节数必须带 B：裸数字会被 rclone 当成 KiB 放大 1024 倍。
    #[test]
    fn small_size_keeps_byte_suffix() {
        assert_eq!(fmt_size_bytes(512), "512B");
        assert_eq!(fmt_size_bytes(0), "0");
    }

    /// 打开已有远程时，把历史遗留的被写坏的裸数字就地修成规范文本，
    /// 这样用户只要打开编辑器保存一次，配置就自愈了。
    #[test]
    fn poisoned_existing_values_are_repaired_on_load() {
        let mut fields = vec![
            field("pacer_min_sleep", "duration", "", false),
            field("nextcloud_chunk_size", "size", "", false),
        ];
        let existing = serde_json::json!({
            "pacer_min_sleep": "1e+07",
            "nextcloud_chunk_size": "1.048576e+07",
        });
        fill_fields_with_existing(&mut fields, existing.as_object().unwrap());
        assert_eq!(fields[0].value.to_string(), "10ms");
        assert_eq!(fields[1].value.to_string(), "10Mi");
    }

    // ---------------- 文件页：面包屑 / 下载 / 移动 ----------------

    /// 面包屑首项是远程名，逐级累积路径；只有最右一级标 current。
    #[test]
    fn file_crumbs_build_ancestor_chain() {
        let c = file_crumbs("drive", "a/b");
        assert_eq!(c.len(), 3);
        assert_eq!(
            (c[0].name.as_str(), c[0].path.as_str(), c[0].is_current),
            ("drive", "", false)
        );
        assert_eq!(
            (c[1].name.as_str(), c[1].path.as_str(), c[1].is_current),
            ("a", "a", false)
        );
        assert_eq!(
            (c[2].name.as_str(), c[2].path.as_str(), c[2].is_current),
            ("b", "a/b", true)
        );

        // 根目录：只有远程一项，且就是当前项
        let root = file_crumbs("drive", "");
        assert_eq!(root.len(), 1);
        assert!(root[0].is_current);
        assert_eq!(root[0].path.as_str(), "");

        // 多余斜杠不会产生空的一级
        assert_eq!(file_crumbs("drive", "/a//b/").len(), 3);
        // 没有远程时返回空（界面显示空路径框）
        assert!(file_crumbs("", "a").is_empty());
    }

    /// 宽度足够时全部显示，不做折叠。
    #[test]
    fn fit_crumbs_shows_all_when_wide() {
        let all = file_crumbs("drive", "a/b/c");
        let (shown, hidden) = fit_crumbs(&all, 2000.0);
        assert_eq!(shown.len(), all.len());
        assert!(hidden.is_empty());
        assert!(shown.iter().all(|c| !c.is_ellipsis));
    }

    /// 宽度不足：保留「远程根 + … + 当前」，中间祖先进隐藏列表。
    #[test]
    fn fit_crumbs_keeps_root_ellipsis_and_current() {
        let all = file_crumbs("drive", "a/b/c");
        let last = all.len() - 1;
        let ell = CrumbRow {
            name: "…".into(),
            path: "".into(),
            is_current: false,
            is_ellipsis: true,
        };
        let avail = crumb_row_width(&all[0])
            + 9.0
            + crumb_row_width(&ell)
            + 9.0
            + crumb_row_width(&all[last])
            + 24.0
            + 1.0;
        let (shown, hidden) = fit_crumbs(&all, avail);
        assert_eq!(shown.len(), 3, "应为 根 + … + 当前");
        assert_eq!(shown[0].name.as_str(), "drive");
        assert!(shown[1].is_ellipsis);
        assert_eq!(shown[2].name.as_str(), "c");
        assert_eq!(hidden.len(), 2, "a、b 应被折叠");
        assert_eq!(hidden[0].name.as_str(), "a");
        assert_eq!(hidden[1].name.as_str(), "b");
    }

    /// 极窄时当前目录也必须在；连省略号都放不下就只显示当前。
    #[test]
    fn fit_crumbs_never_hides_current() {
        let all = file_crumbs("drive", "a/b/c");
        let (shown, hidden) = fit_crumbs(&all, 60.0);
        assert_eq!(
            shown.last().map(|c| c.name.to_string()),
            Some("c".to_string()),
            "当前目录必须留在最右"
        );
        assert!(shown.len() <= 2, "极窄时只保留省略号 + 当前");
        assert!(!hidden.is_empty());

        let (shown, _) = fit_crumbs(&all, 0.0);
        assert_eq!(
            shown.last().map(|c| c.name.to_string()),
            Some("c".to_string()),
            "零宽度时也必须保留当前目录"
        );
        assert!(shown.len() <= 2, "零宽度时最多只剩「… + 当前」");
    }

    /// 只有远程根一级时，无论多窄都显示它。
    #[test]
    fn fit_crumbs_single_crumb_is_kept() {
        let all = file_crumbs("drive", "");
        let (shown, hidden) = fit_crumbs(&all, 10.0);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].name.as_str(), "drive");
        assert!(hidden.is_empty());
    }

    #[test]
    fn parent_path_handles_root_and_nested() {
        assert_eq!(parent_path("a/b/c"), "a/b");
        assert_eq!(parent_path("a"), "");
        assert_eq!(parent_path(""), "");
    }

    /// 文件夹不能移动到它自己或它的子目录里。
    #[test]
    fn move_target_cannot_be_inside_source() {
        assert!(is_same_or_inside("a", "a"));
        assert!(is_same_or_inside("a/b", "a"));
        assert!(is_same_or_inside("a/b/c", "a"));
        assert!(!is_same_or_inside("ab", "a"), "前缀相同但不是子目录");
        assert!(!is_same_or_inside("a", "a/b"));
        assert!(!is_same_or_inside("", "a"));
    }

    /// 下载到目录时同名文件自动加序号，绝不覆盖。
    #[test]
    fn unique_local_path_avoids_overwrite() {
        let dir = std::env::temp_dir().join(format!("rcgui-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let first = unique_local_path(&dir, "a.txt");
        assert_eq!(first.file_name().unwrap().to_string_lossy(), "a.txt");
        std::fs::write(&first, b"x").unwrap();

        let second = unique_local_path(&dir, "a.txt");
        assert_eq!(second.file_name().unwrap().to_string_lossy(), "a (1).txt");
        std::fs::write(&second, b"y").unwrap();

        let third = unique_local_path(&dir, "a.txt");
        assert_eq!(third.file_name().unwrap().to_string_lossy(), "a (2).txt");

        // 无扩展名同样处理
        let noext = unique_local_path(&dir, "README");
        std::fs::write(&noext, b"z").unwrap();
        assert_eq!(
            unique_local_path(&dir, "README")
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "README (1)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件夹下载同样不合并、不覆盖：同名时新建带序号目录。
    #[test]
    fn unique_local_dir_avoids_merge() {
        let dir = std::env::temp_dir().join(format!("rcgui-dldir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let first = unique_local_dir(&dir, "photos");
        assert_eq!(first.file_name().unwrap().to_string_lossy(), "photos");
        std::fs::create_dir_all(&first).unwrap();

        let second = unique_local_dir(&dir, "photos");
        assert_eq!(second.file_name().unwrap().to_string_lossy(), "photos (1)");

        // 同名的普通文件也算占用
        std::fs::write(dir.join("notes"), b"x").unwrap();
        assert_eq!(
            unique_local_dir(&dir, "notes")
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "notes (1)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- 插件列表：卸载后的乐观更新 ----------------

    fn plugin_row(id: &str) -> PluginRow {
        PluginRow {
            id: id.into(),
            name: id.into(),
            ..Default::default()
        }
    }

    /// 卸载成功后，被卸载的卡片必须立刻从列表里消失。
    ///
    /// 这条测试守的是一个「静默」缺陷：磁盘确实删了、配置也确实改了，
    /// 只有列表没刷新。用户看到的就是「点了卸载，卡片还在」，
    /// 而补点一下「刷新」又正常——很容易被当成偶发卡顿放过去。
    #[test]
    fn drop_row_removes_only_target() {
        let rows = vec![plugin_row("onerclone"), plugin_row("imagehost")];
        let kept = drop_row_by_id(rows, "onerclone");
        assert_eq!(kept.len(), 1, "只应摘掉被卸载的那一行");
        assert_eq!(kept[0].id.as_str(), "imagehost");
    }

    /// 摘掉不存在的 id 不能误删任何行。
    #[test]
    fn drop_row_ignores_unknown_id() {
        let rows = vec![plugin_row("imagehost")];
        assert_eq!(drop_row_by_id(rows, "nope").len(), 1);
    }

    /// 空列表摘行不能 panic（卸载最后一张卡片时就是这种情形）。
    #[test]
    fn drop_row_handles_empty_list() {
        let kept: Vec<PluginRow> = drop_row_by_id(Vec::new(), "onerclone");
        assert!(kept.is_empty());
    }

    // ---------------- 远程类型搜索 ----------------

    fn tchoice(display: &str, id: &str, backend: &str) -> TypeChoice {
        TypeChoice {
            display: display.to_string(),
            id: id.to_string(),
            backend: backend.to_string(),
            derived: false,
        }
    }

    /// 三条清单：默认的 Alias、插件类型的 K-Vault、内核的 S3。
    ///
    /// 这里用字面量 `"k-vault"` 而不是常量：主程序已经不再声明这个类型，
    /// 留着它是为了覆盖「老版本存下的远程还带着这个类型名」的情形。
    fn sample_types() -> Vec<TypeChoice> {
        vec![
            tchoice("Alias for an existing remote", "alias", "alias"),
            tchoice("K-Vault", "k-vault", "webdav"),
            tchoice("S3", "s3", "s3"),
        ]
    }

    /// 选中的类型还在过滤结果里：下标要落在同一项上，并且不能报「换类型了」。
    ///
    /// 这条守的是「改搜索词把用户已选好的类型偷偷换掉」——换了类型，参数表却按旧
    /// 类型渲染，用户看到的是自己没选过的东西。
    #[test]
    fn refilter_keeps_selection_when_it_survives() {
        let mut t = TypeChoices::default();
        let idx = t.reset(sample_types(), Some("k-vault"));
        assert_eq!(idx, 1);
        let r = t.refilter("vaul", Some("k-vault"));
        assert_eq!(r.idx, Some(0), "K-Vault 只剩一项，下标应指向它");
        assert!(!r.switched, "选中的类型还在，不能当成换了类型");
        assert_eq!(t.visible_at(0).unwrap().id, "k-vault");
    }

    /// 选中的类型被筛掉：界面必须知道「选中项变成别人了」，调用方要跟着换参数表。
    ///
    /// 这是父会话实测到的原始缺陷：搜 kvault 后列表只剩 K-Vault，下面却还是
    /// Alias 的 Remote/path 字段，点保存会把参数写到 webdav 上。
    #[test]
    fn refilter_reports_switch_when_selection_is_filtered_away() {
        let mut t = TypeChoices::default();
        t.reset(sample_types(), Some("alias"));
        let r = t.refilter("kvault", Some("alias"));
        assert_eq!(r.idx, Some(0));
        assert!(r.switched, "旧选中项被筛掉时必须报 switched");
        assert_eq!(
            t.visible_at(0).unwrap().id,
            "k-vault",
            "下标 0 在新清单里是 K-Vault，参数表必须按它换"
        );
    }

    /// 一条都匹配不上：不选中任何项，保存路径据此拒绝，而不是退回第 0 项。
    #[test]
    fn refilter_with_no_match_clears_selection() {
        let mut t = TypeChoices::default();
        t.reset(sample_types(), Some("s3"));
        let r = t.refilter("zzz", Some("s3"));
        assert_eq!(r.idx, None, "没有匹配项时不能假装选中了第 0 项");
        assert!(!r.switched);
        assert!(t.visible.is_empty());
        assert!(t.selected(-1).is_none(), "界面写 -1 时保存必须拿不到类型");
    }

    /// 之前根本没有选中项（刚打开下拉）：保留原来的第 0 项语义，不算换类型。
    ///
    /// 表单在打开对话框时就是按第 0 项铺的，所以这里不重铺也不会出现
    /// 「选中 A、表单是 B」。
    #[test]
    fn refilter_without_previous_selection_keeps_row_zero() {
        let mut t = TypeChoices::default();
        t.reset(sample_types(), None);
        let r = t.refilter("s3", None);
        assert_eq!(r.idx, Some(0));
        assert!(!r.switched);
        assert_eq!(t.selected(0).unwrap().id, "s3");
    }

    /// 负下标（界面上的「没有选中项」）一律当作没选，不能 `max(0)` 回落到第一项。
    #[test]
    fn selected_rejects_negative_index() {
        let mut t = TypeChoices::default();
        t.reset(sample_types(), None);
        assert!(t.selected(-1).is_none());
        assert_eq!(t.selected(0).unwrap().id, "alias");
    }

    /// 插件类型只是个名字：参数表要按它真正落盘时的后端取。
    ///
    /// K-Vault 的远程在 rclone 里就是 webdav，插件额外要的键由插件声明的默认值补上，
    /// 不是靠一张「k-vault 参数表」——主程序里已经没有这种表了。
    #[test]
    fn form_fields_type_follows_the_backend_for_plugin_types() {
        let kv = tchoice("K-Vault", "k-vault", "webdav");
        assert_eq!(form_fields_type(&kv), "webdav");
        let s3 = tchoice("S3", "s3", "s3");
        assert_eq!(form_fields_type(&s3), "s3");
    }

    // ---------------- 保存时插件默认参数不能丢 ----------------

    /// kvault 的 manifest（`../plugin_repos/rcgui-plugin-kvault/config.json`）声明的
    /// 默认参数。那个目录不在本仓库里，所以照抄一份，测试不依赖它。
    fn kvault_derived() -> remote_types::Derived {
        let mut d = tderived("kvault", "K-Vault", "webdav");
        d.defaults.insert("vendor".to_string(), "other".to_string());
        d.defaults.insert("kvault".to_string(), "1".to_string());
        d.defaults
            .insert("kvault_storage".to_string(), "telegram".to_string());
        d.defaults
            .insert("kvault_dedup".to_string(), "1".to_string());
        d
    }

    /// 向导里选到插件类型、只填了 URL 就保存：插件的默认参数必须一起写进配置。
    ///
    /// 这里故意走字段表那条路——webdav 模板只渲染 url/vendor，插件的键根本不在表里，
    /// 正是这样丢失的。
    #[test]
    fn new_remote_save_keeps_plugin_defaults() {
        let mut choice = tchoice("K-Vault", "kvault", "webdav");
        choice.derived = true;
        let fields = vec![
            field("url", "text", "http://127.0.0.1:49389/dav", true),
            field("vendor", "text", "other", false),
        ];
        let user = match fields_to_params(&fields) {
            Value::Object(m) => m,
            other => panic!("fields_to_params 应该给对象，实际 {other}"),
        };
        let params = params_for_save(&choice, &[kvault_derived()], user, true);
        let obj = params.as_object().expect("参数得是对象");
        assert_eq!(
            obj.get("url").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:49389/dav")
        );
        assert_eq!(obj.get("kvault").and_then(|v| v.as_str()), Some("1"));
        assert_eq!(
            obj.get("kvault_storage").and_then(|v| v.as_str()),
            Some("telegram")
        );
        assert_eq!(obj.get("kvault_dedup").and_then(|v| v.as_str()), Some("1"));
        assert_eq!(obj.get("vendor").and_then(|v| v.as_str()), Some("other"));
        // type 由调用方按选中的类型单独传，参数里再出现一份会和它打架。
        assert!(!obj.contains_key("type"));
    }

    /// 用户把某个键留空等于没填：不能拿空串把插件的默认值擦掉。
    #[test]
    fn blank_user_field_does_not_erase_plugin_default() {
        let mut choice = tchoice("K-Vault", "kvault", "webdav");
        choice.derived = true;
        let user = parse_kv_lines("url = https://kv.example.com/dav\nkvault_storage = \n");
        let params = params_for_save(&choice, &[kvault_derived()], user, true);
        let obj = params.as_object().unwrap();
        assert_eq!(
            obj.get("kvault_storage").and_then(|v| v.as_str()),
            Some("telegram"),
            "留空的字段是「没填」，不是「清空」"
        );
    }

    /// 改已有远程时不铺默认值：用户删掉的键不该被复活。
    #[test]
    fn editing_an_existing_remote_does_not_add_defaults() {
        let mut choice = tchoice("K-Vault", "kvault", "webdav");
        choice.derived = true;
        let user = parse_kv_lines("url = https://kv.example.com/dav\n");
        let params = params_for_save(&choice, &[kvault_derived()], user, false);
        let obj = params.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj.get("url").is_some());
    }

    /// 普通后端（直接来自内核）不受影响：不会凭空多出别的键。
    #[test]
    fn plain_backend_save_adds_nothing() {
        let choice = tchoice("s3", "s3", "s3");
        let user = parse_kv_lines("provider = AWS\n");
        let params = params_for_save(&choice, &[kvault_derived()], user, true);
        let obj = params.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert_eq!(obj.get("provider").and_then(|v| v.as_str()), Some("AWS"));
    }

    /// 界面上的原始文本也要跟着铺：用户看到的和真正保存的必须一致。
    #[test]
    fn raw_text_gets_missing_defaults_without_touching_user_values() {
        let mut choice = tchoice("K-Vault", "kvault", "webdav");
        choice.derived = true;
        let current = "url = http://127.0.0.1:49389/dav\nvendor = own";
        let seeded =
            seed_derived_defaults(&choice, &[kvault_derived()], current).expect("该补默认值");
        assert!(seeded.contains("kvault=1"));
        assert!(seeded.contains("kvault_storage=telegram"));
        assert!(seeded.contains("kvault_dedup=1"));
        assert_eq!(
            seeded.matches("vendor").count(),
            1,
            "用户自己写的 vendor 不能被默认值覆盖：{seeded}"
        );
        // 补过之后再补一次就没什么可补的了。
        assert!(seed_derived_defaults(&choice, &[kvault_derived()], &seeded).is_none());
    }

    /// 新建远程真正发出去的那一次请求：走 create，类型是 `base_type`，参数带插件默认值。
    ///
    /// 这条盯的是接线而不只是组装：`build_save_request` 就是保存回调用的那个函数，
    /// 内核算出来的 conf 里缺 `kvault`/`vendor` 时它会先红。
    #[test]
    fn create_request_keeps_plugin_defaults_and_uses_base_type() {
        let mut choice = tchoice("K-Vault", "kvault", "webdav");
        choice.derived = true;
        let mut user = serde_json::Map::new();
        user.insert(
            "url".to_string(),
            Value::String("http://127.0.0.1:49389/dav".to_string()),
        );
        match build_save_request("qa_plugin_vault", &choice, &[kvault_derived()], user, true) {
            SaveRequest::Create {
                name,
                rtype,
                params,
            } => {
                assert_eq!(name, "qa_plugin_vault");
                assert_eq!(rtype, "webdav", "插件派生类型落盘的是 base_type");
                let obj = params.as_object().expect("参数应当是对象");
                assert_eq!(obj.get("kvault").and_then(|v| v.as_str()), Some("1"));
                assert_eq!(obj.get("vendor").and_then(|v| v.as_str()), Some("other"));
                assert_eq!(
                    obj.get("kvault_storage").and_then(|v| v.as_str()),
                    Some("telegram")
                );
                assert_eq!(
                    obj.get("url").and_then(|v| v.as_str()),
                    Some("http://127.0.0.1:49389/dav")
                );
            }
            SaveRequest::Update { .. } => panic!("新建远程必须走 config/create"),
        }
    }

    /// 改已有远程走 update：`config/update` 没有类型这一位，参数里也不该多出默认值。
    #[test]
    fn update_request_omits_type_and_defaults() {
        let mut choice = tchoice("K-Vault", "kvault", "webdav");
        choice.derived = true;
        let mut user = serde_json::Map::new();
        user.insert(
            "url".to_string(),
            Value::String("http://127.0.0.1:1/dav".to_string()),
        );
        match build_save_request("qa_plugin_vault", &choice, &[kvault_derived()], user, false) {
            SaveRequest::Update { name, params } => {
                assert_eq!(name, "qa_plugin_vault");
                let obj = params.as_object().expect("参数应当是对象");
                assert!(!obj.contains_key("type"));
                assert!(!obj.contains_key("kvault"), "改已有远程不补默认值");
            }
            SaveRequest::Create { .. } => panic!("改已有远程不该走 config/create"),
        }
    }

    // ---------------- 类型清单只来自内核与插件 ----------------

    /// 插件声明的派生类型，只填清单测试要用到的字段。
    fn tderived(id: &str, name: &str, base: &str) -> remote_types::Derived {
        remote_types::Derived {
            name: name.to_string(),
            id: id.to_string(),
            base_type: base.to_string(),
            defaults: std::collections::BTreeMap::new(),
            plugin: "kvault".to_string(),
        }
    }

    /// 内核 `config/providers` 的应答形状（这里只要 `Name`）。
    fn providers_of(names: &[&str]) -> serde_json::Value {
        let arr: Vec<serde_json::Value> = names
            .iter()
            .map(|n| serde_json::json!({ "Name": n }))
            .collect();
        serde_json::json!({ "providers": arr })
    }

    /// 内核没连上、没有上次成功的缓存、也没有插件声明类型：一项都不列。
    ///
    /// 这里以前会塞进 15 个内置类型，用户选中后保存必然失败——那份"兜底"比空下拉框更糟，
    /// 因为它把「连接内核」这个真问题藏了起来。
    #[test]
    fn no_providers_and_no_plugin_lists_nothing() {
        assert!(build_type_choices(&serde_json::json!({}), &[]).is_empty());
        assert!(build_type_choices(&serde_json::json!({ "providers": [] }), &[]).is_empty());
    }

    /// 类型名与后端名都来自内核的 providers，按显示名排序。
    #[test]
    fn choices_come_from_kernel_providers() {
        let out = build_type_choices(&providers_of(&["s3", "alias"]), &[]);
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["alias", "s3"]);
        assert!(out.iter().all(|c| !c.derived && c.backend == c.id));
    }

    /// 没装或没启用 kvault 插件时，清单里不许出现 K-Vault 一类凭空的行。
    ///
    /// 父会话在 Release 上看到 `K-Vault` 与 `k-vault` 两行，就是内置注入造成的：
    /// 其中一行是内核根本不认识的后端，选中必然失败。
    #[test]
    fn no_builtin_kvault_row_without_plugin() {
        let out = build_type_choices(&providers_of(&["webdav"]), &[]);
        assert!(
            !out.iter()
                .any(|c| c.id == "k-vault" || c.display.eq_ignore_ascii_case("k-vault")),
            "没有插件声明时不能凭空出现 kvault 类型"
        );
        assert_eq!(out.len(), 1);
    }

    /// 启用插件后只多出它声明的那一项，落盘 backend 用 base_type。
    #[test]
    fn enabled_plugin_adds_exactly_one_row() {
        let out = build_type_choices(
            &providers_of(&["webdav"]),
            &[tderived("k-vault", "K-Vault", "webdav")],
        );
        let kv: Vec<&TypeChoice> = out.iter().filter(|c| c.id == "k-vault").collect();
        assert_eq!(kv.len(), 1, "插件声明的类型只能出现一次");
        assert_eq!(kv[0].display, "K-Vault");
        assert_eq!(kv[0].backend, "webdav", "落盘必须写插件声明的 base_type");
        assert!(kv[0].derived);
        assert_eq!(
            out.iter().filter(|c| c.id == "k-vault").count(),
            1,
            "插件声明的类型与内置类型同名，清单里只能出现一行"
        );
    }

    /// 只有插件、内核 provider 清单为空时，清单里只剩插件项，不补任何后端。
    #[test]
    fn plugin_only_list_has_no_fabricated_backends() {
        let out = build_type_choices(
            &serde_json::json!({}),
            &[tderived("k-vault", "K-Vault", "webdav")],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "k-vault");
    }

    /// 老版本存下的 K-Vault 远程：有插件就显示成插件类型，没插件就退回 webdav。
    ///
    /// 无论哪种情况，它的配置与数据都在本地，用户始终能重新打开改地址、换令牌；
    /// 换掉的只是「还叫不叫 K-Vault」和「站点写入功能在不在」。
    #[test]
    fn legacy_kvault_remote_shows_the_plugin_type_only_while_the_plugin_exists() {
        let params =
            serde_json::json!({"vendor": "other", "kvault": "1", "url": "https://kv.example"});
        let stored = "webdav";

        // 插件在：显示成插件声明的类型，落盘仍是 webdav。
        let owner = [tderived("kvault", "K-Vault", "webdav")];
        assert_eq!(display_type_of(&params, stored, &owner), "kvault");
        let c = edit_single_choice(&display_type_of(&params, stored, &owner), &owner);
        assert_eq!(c.backend, "webdav", "保存要写回真实后端");
        assert!(c.derived);

        // 插件不在：同一个远程退回 webdav，不再合成一个没人实现的类型。
        assert_eq!(display_type_of(&params, stored, &[]), "webdav");

        // 没有标记键的普通 WebDAV 永远不能被认成 K-Vault。
        let plain = serde_json::json!({"vendor": "other", "url": "https://dav.example"});
        assert_eq!(display_type_of(&plain, stored, &owner), "webdav");
    }

    /// 插件被停用/卸载后，带 K-Vault 标记的远程必须落进 `Orphan`，不能悄悄交给 rclone。
    ///
    /// 交给 rclone 的后果是站点 DAV 回一个 403「method not allowed」：用户既不知道
    /// 发生了什么，也不知道装回插件就能解决。没有标记的普通 WebDAV 则永远走 `NotKv`。
    #[test]
    fn a_marked_remote_never_falls_through_to_the_dav_path() {
        let params: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({"kvault": "1", "url": "https://kv.example/dav"}),
        )
        .unwrap();
        let cfg = kvault::KvCfg::from_params(&params);

        // 普通远程：插件在不在都与它无关。
        assert!(matches!(kv_write_kind(None, true, "site"), KvWrite::NotKv));
        assert!(matches!(kv_write_kind(None, false, "site"), KvWrite::NotKv));

        // 带标记 + 插件在：走站点 API。
        assert!(matches!(
            kv_write_kind(Some(cfg.clone()), true, "site"),
            KvWrite::Bridge(_)
        ));

        // 带标记 + 插件不在：拒绝，并且要点名是哪台远程、为什么、怎么办。
        match kv_write_kind(Some(cfg), false, "site") {
            KvWrite::Orphan(msg) => {
                assert!(msg.contains("site"), "要说出是哪台远程：{}", msg);
                assert!(msg.contains("插件"), "要说清是插件没启用：{}", msg);
                assert!(msg.contains("启用"), "要告诉用户启用就能恢复：{}", msg);
                assert!(msg.contains("还在"), "要说清数据没被动过：{}", msg);
            }
            KvWrite::NotKv => panic!("插件不在时不能落回 rclone 的 DAV 写"),
            KvWrite::Bridge(_) => panic!("插件不在时没有站点 API 可走"),
        }
    }

    /// 写路径不许再用 `kv_cfg_for`：它只回答「是不是 K-Vault」，插件不在时返回
    /// `None` 就等于放行给 rclone（403 + 用户看不懂）。剩下的合法调用点只有两个：
    /// 定义本身，和桥的读实现 `kv_list_rows`（**它前面已经有 `kv_access_blocked`
    /// 挡着**，插件不在时根本走不到这里，见用例
    /// `every_remote_entry_checks_the_plugin_gate`）。
    #[test]
    fn kv_cfg_for_is_only_used_by_the_bridge_read_path() {
        // 只扫生产代码：用例自己也写着这个词，扫进来会把结果算多。
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let prod = src
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap_or_default();
        assert!(
            prod.len() > 1000,
            "扫描基准不对（切分标记变了），先修这个用例"
        );
        let sites: Vec<&str> = prod
            .lines()
            .map(|l| l.trim())
            .filter(|l| l.contains("kv_cfg_for("))
            .collect();
        assert_eq!(
            sites.len(),
            2,
            "多出来的调用点会绕过写闸门，应改走 kv_write_target：{:?}",
            sites
        );
        assert!(sites[0].starts_with("async fn kv_cfg_for("));
        // 参数表可能被折成多行，比之前先压掉空白：这一条要守的是「第二处是桥的读
        // 实现里那句赋值」，不是它排成几行。
        let want = crate::source_scan::squash("let cfg = kv_cfg_for(ctx, remote).await?;");
        assert!(
            crate::source_scan::squash(sites[1]).starts_with(&want),
            "桥的读实现里那句调用变了：{}",
            sites[1]
        );
    }

    /// 插件不在时，一条带标记的远程在**每个**入口都要被挡下来。
    ///
    /// 用源码扫描而不是逐个调用：这些入口都是 `Cmd` 分支里的长函数，要有 `Ctx` 和
    /// 真的 rclone 才跑得起来，单元测试够不着。而漏掉一个入口的后果是「这里被拒、
    /// 换个入口照样能读能挂」—— 这种缺口正是验收时被抓到的那一条。
    ///
    /// 有意不在这里的：`MountRemove`（卸载是退出动作）、`RemoteDelete`（挡住会把
    /// 用户锁死在一条删不掉的远程上），它们的理由写在各自分支的注释里。
    #[test]
    fn every_remote_entry_checks_the_plugin_gate() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let prod = src
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap_or_default();
        assert!(
            prod.len() > 1000,
            "扫描基准不对（切分标记变了），先修这个用例"
        );

        // 每个入口都得先问闸门、再碰远程。只查「有没有调」不够：顺序错了等于没闸
        // ——先打了远程再问，插件不在时东西已经动过了。
        //
        // 比的是压掉空白之后的源码：`client.copyfile(` 这类带点的方法调用会被
        // rustfmt 在点处折成 `client` + `.copyfile(` 两行，按字面量找就查不到——
        // 这条用例当初就是在 CI 上这么红的（接线好着，只是排版变了）。要找的是
        // 一件「事」，不是某一行字。
        //
        // 命令分支：从 `Cmd::X => {` 起，到下一个同级 `        Cmd::` 止
        for marker in [
            "Cmd::FilesRefresh { seq } => {",
            "Cmd::FilesDownload { name, local } => {",
            "Cmd::FilesDownloadToDir { name, dir, is_dir } => {",
            "Cmd::FilesDownloadMany { items, dir } => {",
            "Cmd::FilesMoveList { path, seq } => {",
            "Cmd::MountAdd => {",
            // 服务只有 ServeStart/ServeStop 两个分支，没有 ServeAdd：
            // 标记写错名字会让这里 `找不到命令分支` 直接 panic，而不是静默少查一条。
            "Cmd::ServeStart => {",
            "Cmd::RemoteEditLoad { name } => {",
            "Cmd::RemoteSave => {",
        ] {
            let start = match prod.find(marker) {
                Some(i) => i + marker.len(),
                None => panic!("找不到命令分支 {marker}，是不是改名了"),
            };
            let rest = &prod[start..];
            let end = rest.find("\n        Cmd::").unwrap_or(rest.len());
            let body = crate::source_scan::squash(&rest[..end]);
            let gate_at = body.find("kv_access_blocked(").unwrap_or_else(|| {
                panic!("{marker} 没有问 kv_access_blocked：插件不在时这个入口就是旁路")
            });
            // 凡是会对远程动手的调用，都必须排在闸门之后。清单里放的是「读/写远程」
            // 而不是「读内存」的调用，所以多列几个不会误报；真漏一个，改的人会在
            // 这里看到是哪一次调用先跑出去了。
            for action in [
                "client.copyfile(",
                "client.sync_copy(",
                "client.list(",
                "client.mount(",
                "client.serve_start(",
                "client.config_dump(",
                "client.config_create(",
                "client.config_update(",
                "kv_list_rows(",
                "list_display_checked(",
                "apply_bundle_to_rclone(",
            ] {
                if let Some(at) = body.find(action) {
                    assert!(
                        gate_at < at,
                        "{marker} 先调 {action} 才问闸门：插件不在时这一下已经打到远程上了"
                    );
                }
            }
        }

        // 上面那轮只检查「这次调用排在闸门后面」，如果某个分支哪天不再做那件事
        // （比如下载改走了别的接口），它就悄悄变成空检查。这里逐个钉住每个入口
        // 的特征动作：找不到就说明这一臂不再是我们以为的那件事，标记该跟着改。
        for (marker, op) in [
            ("Cmd::MountAdd => {", "client.mount("),
            ("Cmd::ServeStart => {", "client.serve_start("),
            (
                "Cmd::FilesDownload { name, local } => {",
                "client.copyfile(",
            ),
            ("Cmd::FilesMoveList { path, seq } => {", "client.list("),
            ("Cmd::RemoteEditLoad { name } => {", "client.config_dump("),
        ] {
            let body = crate::source_scan::region(prod, marker, "\n        Cmd::")
                .or_else(|| {
                    // 最后一条分支后面可能没有下一个 `Cmd::`：切到文件尾
                    prod.find(marker).map(|i| &prod[i + marker.len()..])
                })
                .unwrap_or_else(|| panic!("找不到命令分支 {marker}，是不是改名了"));
            let body = crate::source_scan::squash(body);
            let want = crate::source_scan::squash(op);
            let gate_at = body
                .find("kv_access_blocked(")
                .unwrap_or_else(|| panic!("{marker} 里没有 kv_access_blocked"));
            let op_at = body
                .find(want.as_str())
                .unwrap_or_else(|| panic!("{marker} 里找不到 {op}，标记该更新了"));
            assert!(
                gate_at < op_at,
                "{marker} 先调 {op} 才问闸门：插件不在时这一下已经打到远程上了"
            );
        }

        // 独立函数：用 `source_scan::function_body` 切到函数收尾（顶格 `}`），
        // 不依赖缩进和下一个函数怎么写。
        for sig in [
            "async fn files_navigate(",
            "async fn startup_flow(",
            // 同步卡片的云端目录选择器：借它能把站点目录树翻一遍、还能把
            // 打不开的远程选成同步目标
            "async fn list_display_checked(",
            // 档案导入：里面也会挂载和起服务
            "async fn apply_bundle_to_rclone(",
        ] {
            let body = crate::source_scan::function_body(prod, sig)
                .unwrap_or_else(|| panic!("找不到函数 {sig}，是不是改名了"));
            assert!(
                crate::source_scan::squash(body).contains("kv_access_blocked("),
                "{sig} 没有问 kv_access_blocked：插件不在时这个入口就是旁路"
            );
        }

        // 自动恢复（挂载与服务）在另一个文件，那边同样是入口。
        // 计数也压掉空白：折行不该让「少查一处」和「排版变了」混成一个红。
        let persist = crate::source_scan::squash(&include_str!("persist.rs").replace("\r\n", "\n"));
        assert_eq!(
            persist.matches("crate::kv_access_blocked(").count(),
            2,
            "自动恢复的挂载与服务都要先问闸门"
        );

        // 用量列也走 about：插件不在时问出来的是个数，看起来像「能用」
        let refresh = crate::source_scan::squash(&include_str!("refresh.rs").replace("\r\n", "\n"));
        assert_eq!(
            refresh.matches("crate::kv_access_blocked(").count(),
            1,
            "用量抓取要先问闸门，别给打不开的远程填数字"
        );

        // 对账/自愈也是入口：它会把「档案里有、运行态没有」的挂载和服务重建，
        // 但它自己拿不到插件清单，只能靠调用方传进去的那张表。
        let rec = crate::source_scan::squash(&include_str!("reconcile.rs").replace("\r\n", "\n"));
        assert_eq!(
            rec.matches("blocked.get(remote_of(fs))").count(),
            2,
            "对账的挂载与服务两臂都要查 blocked 表"
        );
        // 调用方必须把表真的递进去，否则上面那两处永远拿到空表
        let prod_flat = crate::source_scan::squash(prod);
        assert_eq!(
            prod_flat.matches("kv_blocked_map(ctx,&draft)").count(),
            2,
            "两处 reconcile 调用都要先算出 blocked 表"
        );
    }

    /// 派生类型按插件显示名与 base_type 往返；内核自带后端原样透传。
    #[test]
    fn edit_choice_follows_plugin_and_passthrough_backends() {
        let d = edit_single_choice(
            "kdrive-lite",
            &[tderived("kdrive-lite", "KDrive Lite", "kdrive")],
        );
        assert_eq!(d.display, "KDrive Lite");
        assert_eq!(d.backend, "kdrive");
        assert!(d.derived);
        let p = edit_single_choice("s3", &[]);
        assert_eq!(p.backend, "s3");
        assert!(!p.derived);
    }

    /// 顶栏与连接卡的地址/版本/托管标记只由 `apply_conn_view` 投影写入。
    ///
    /// 换内核（新进程、新端口、新口令）后，启动时那段一次性赋值再也不会跑，
    /// 所以这里少了哪个 setter，界面就会长期显示上一代内核的端口和版本。
    /// 直接读真实源码：Slint 全局属性在无窗口的测试进程里建不出来。
    #[test]
    fn apply_conn_view_rewrites_the_dashboard_identity() {
        let src = include_str!("main.rs");
        // 两种行尾都测：CI 检出常见 CRLF、开发机是 LF，扫描只对一种成立就会一边绿一边红。
        let lf = src.replace("\r\n", "\n");
        let crlf = lf.replace('\n', "\r\n");
        for (label, text) in [("LF", &lf), ("CRLF", &crlf)] {
            let body = crate::source_scan::function_body(text, "fn apply_conn_view(")
                .unwrap_or_else(|| {
                    panic!("{label}: apply_conn_view 必须存在，且以独占一行的 }} 收尾")
                });
            for setter in ["set_url", "set_managed", "set_version"] {
                assert!(
                    body.contains(setter),
                    "{label}: apply_conn_view 必须写 DashboardG.{setter}，否则换内核后顶栏/连接卡继续显示旧端口与旧版本"
                );
            }
        }
    }

    /// 云端目录选完写回的值必须是「远程名:子路径」，而且来回折两遍不漂。
    ///
    /// 浏览窗走的是：字段值 → 显示路径 → 用户逐级点进 → 显示路径 → 字段值。
    /// 这条链上任何一处边界对不上，落下来的要么是 rclone 认不出的 `root://…`，
    /// 要么少一层子目录（同步就跑到上一级去）。四个纯函数就是这条链的全部换算。
    #[test]
    fn cloud_dir_value_survives_the_browse_round_trip() {
        // 存下来的形状；`sync_tasks::split_remote` 靠冒号认远程名
        let value = compose_fs("gdrive", "backup/photos");
        assert_eq!(value, "gdrive:backup/photos");
        assert_eq!(
            crate::sync_tasks::split_remote(&value).unwrap(),
            ("gdrive", "backup/photos")
        );

        // 弹窗里显示成 root://…
        let shown = fs_to_display(&value);
        assert_eq!(shown, "root://gdrive/backup/photos");

        // 选中返回：拆出的 fs 带冒号，拼回去必须与进来时一模一样
        let (fs, sub) = split_display(&shown).expect("非根路径必须拆得开");
        assert_eq!(fs, "gdrive:");
        assert_eq!(sub, "backup/photos");
        assert_eq!(compose_fs(fs.trim_end_matches(':'), &sub), value);

        // 选到远程根：`name:` 是合法值，空格子路径不能拼成 `name:/`
        let root_value = compose_fs("gdrive", "");
        assert_eq!(root_value, "gdrive:");
        let root_shown = fs_to_display(&root_value);
        let (fs, sub) = split_display(&root_shown).expect("远程根也是可以选的位置");
        assert_eq!(compose_fs(fs.trim_end_matches(':'), &sub), root_value);
        assert_eq!(crate::sync_tasks::split_remote(&root_value).unwrap().1, "");

        // 停在列表根（没进任何远程）时点确定要算「没选」，不能落成 root://
        assert!(split_display("root://").is_none());
        assert!(split_display("root:///").is_none());
        assert_eq!(display_parent("root://gdrive"), Some("root://".to_string()));
    }

    /// 两个「浏览…」按钮必须真的接到维护动作上，且列目录失败时不许写字段。
    ///
    /// 这些接线在无窗口的测试进程里跑不起来（`Ctx` 要插件宿、KV、tokio 全套），
    /// 所以读源码断言调用点存在；被调用的函数各自有上面那条行为测试。
    #[test]
    fn sync_browse_pickers_are_wired_and_keep_the_old_value_on_failure() {
        let maint = include_str!("maintenance.rs").replace("\r\n", "\n");
        for arm in ["\"sync-pick-local\" =>", "\"sync-pick-remote\" =>"] {
            assert!(maint.contains(arm), "maintenance::handle 少了分支 {arm}");
        }

        let local = crate::source_scan::function_body(&maint, "async fn sync_pick_local(")
            .expect("sync_pick_local 必须存在");
        assert!(local.contains("pick_folder()"), "本地目录要开系统选择框");
        // `let Some(..) = … else { return }` 是「取消就一个字都不写」的写法：
        // 改成 unwrap_or_default 之类就会把空串写进去，用户原来的路径就没了。
        assert!(
            local.contains("let Some(dir) = dlg.pick_folder() else {"),
            "取消时不能写 sync_local"
        );

        let remote = crate::source_scan::function_body(&maint, "async fn sync_pick_remote(")
            .expect("sync_pick_remote 必须存在");
        assert!(
            remote.contains("Cmd::BrowseOpen { scope: 2 }"),
            "云端目录要走浏览窗那条路（scope 2），否则只有文件没有目录过滤"
        );

        let ui = include_str!("../ui/main.slint");
        for hook in [
            "maintenance-action(\"sync-pick-local\"",
            "maintenance-action(\"sync-pick-remote\"",
        ] {
            assert!(ui.contains(hook), "同步卡片少了浏览按钮 {hook}");
        }

        let src = include_str!("main.rs").replace("\r\n", "\n");
        let ok = crate::source_scan::function_body(&src, "async fn browse_ok_sync(")
            .expect("browse_ok_sync 必须存在");
        assert!(
            ok.contains("list_display_checked("),
            "落值之前必须真去列一次目录，否则存的是打不开的位置"
        );
        assert_eq!(
            ok.matches("set_sync_remote(").count(),
            1,
            "写云端目录只能有一条路径（列目录成功那条）；失败分支也写一次，用户手填的旧值就被冲掉了"
        );
    }

    /// 本轮新增的界面文案必须在 `.po` 里有 msgid，否则英文界面会掉回中文。
    ///
    /// `crates/i18n/src/en.rs` 是生成文件（`_audit/gen_en.py`），这里只查 `.po`：
    /// 它是仓库里唯一手写的翻译来源，缺一条就是缺一条。
    #[test]
    fn new_sync_picker_strings_have_a_po_entry() {
        let po = include_str!("../ui/i18n/en/LC_MESSAGES/rclone-gui.po");
        for msgid in [
            "选择本地文件夹",
            "打开系统窗口，选择本机的源文件夹",
            "连到远程仓库，选一个已有的目录；打不开时会保留现在填的值",
            "正在读取远程目录…",
            "还没连上 rclone，等连上再选云端目录",
            "已选择本地文件夹：{}",
            "已选择云端目录：{}",
            "没选远程，云端目录没有改动",
            "这个位置现在打不开，云端目录保持原值：{}",
            "打不开这个位置：{}",
            "读不到远程列表：{}",
            "看不懂这个路径：{}",
            "列不出来：{}",
        ] {
            let entry = format!("msgid \"{msgid}\"");
            assert!(
                po.contains(entry.as_str()),
                "`.po` 缺少 msgid「{msgid}」，英文界面会显示中文"
            );
        }
    }

    /// 用户挑过的文件授权必须在三条退出路径上一起失效：插件停用、插件卸载、程序退出。
    ///
    /// 少任何一条，后果都不是「多留一条记录」：停用的插件仍能把那个文件发出去，
    /// 卸掉的 id 会在日后装回同名插件时变成一份来历不明的许可，退出前的那几秒
    /// 也可能被某个远程动作捡起来用。这里按源码位置断言，因为三处都在
    /// `handle_cmd` 的巨大 match 里，抽成函数反而会把「哪个命令清理什么」的关系藏起来。
    #[test]
    fn upload_grants_are_dropped_on_disable_uninstall_and_quit() {
        let src = include_str!("main.rs").replace("\r\n", "\n");
        let prod = src
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("文件里应当有测试模块");

        let toggle = prod
            .find("Cmd::PluginToggle { idx } => {")
            .expect("应当有启用/停用分支");
        let uninstall = prod
            .find("Cmd::PluginUninstall { idx } => {")
            .expect("应当有卸载分支");
        let quit = prod.find("Cmd::Quit => {").expect("应当有退出分支");

        let revokes: Vec<usize> = prod
            .match_indices("plugin_host::upload::revoke_plugin(&id);")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            revokes.len(),
            2,
            "停用与卸载各要撤一次授权，实际找到 {} 处",
            revokes.len()
        );
        assert!(
            revokes[0] > toggle && revokes[0] < uninstall,
            "第一处撤授权要在「停用」分支里（插件已经没有面板了）"
        );
        assert!(
            revokes[1] > uninstall,
            "第二处撤授权要在「卸载」分支里（这个 id 之后不存在了）"
        );

        let cancels: Vec<usize> = prod
            .match_indices("plugin_host::upload::cancel_all();")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(cancels.len(), 1, "退出路径要撤掉所有还在进行的上传");
        assert!(cancels[0] > quit, "cancel_all 要落在退出分支里");
        let core_stop = prod
            .find("ctx.maint.core.stop();")
            .expect("退出分支里应当停内核");
        assert!(
            cancels[0] < core_stop,
            "先撤上传授权再停内核：反过来的话，内核那一步卡住就没人撤了"
        );
    }
}
