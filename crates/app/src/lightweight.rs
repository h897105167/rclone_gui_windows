//! 轻量模式：窗口藏进托盘一段时间后，把「不看了」变成「真的放掉」。
//!
//! 为什么不是只调一个 Windows API：`EmptyWorkingSet` 只是把工作集里的页面挪到
//! 页面文件/待机列表，进程的内存分配一块都没归还。真要降下来，得先把占内存的
//! 东西本身扔掉——几千行的文件列表、传输任务行、插件面板里那些**已经解码好的位图**
//! （`PanelRow.img` 是宿主解出来的像素），以及 wasm 编译后的机器码缓存。
//! 所以这里分三步：丢模型 → 丢缓存 → 最后才顺手收拾工作集。
//!
//! 窗口本身也是真放掉的：`main` 在创建界面前打开了 `SLINT_DESTROY_WINDOW_ON_HIDE`，
//! 所以 `hide()` 会让 Slint 走 `suspend()`——原生窗口和软件渲染表面一起归还，
//! 只留下组件的状态（属性、模型）。重新显示时 Slint 按记下的尺寸和位置重建窗口，
//! 所以我们不需要自己保存窗口几何。
//!
//! 不能承诺的部分（写在这里免得以后有人把它当 bug 报）：
//! * 组件的属性和模型仍然常驻。这里只清「看着大」的那几份：列表行、插件面板里
//!   已解码的位图、wasm 编译缓存。编辑器里的表单不清——清掉省不下多少内存，
//!   却会让用户填到一半的内容消失。
//! * 插件列表那几十行元数据（id、名称、几个状态布尔）不清：退出轻量模式时要靠它
//!   把「当前打开的是哪个插件」换回行号才能重新拉面板。真正占地方的是面板里的位图。
//! * 位图交给 Slint 后由渲染器持有引用，`set_panel_rows([])` 只是让界面不再引用它们；
//!   是否立刻归还取决于渲染后端的缓存策略，本模块不做假设、也不虚报数字。
//! * 进程常驻。定时同步、内核连接、传输任务都在后台继续跑，不归界面管。
//!
//! 恢复时窗口由 Slint 重建，我们只把清掉的列表按当前页重新拉一遍（`Cmd` 走原来的通道）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use slint::{ComponentHandle as _, Model as _, ModelRc, VecModel, Weak};

use crate::{
    Cmd, Ctx, FieldRow, LocalG, MainWindow, MaintenanceG, MountsG, PluginsG, RemotesG, ServesG,
    TransfersG,
};

/// 默认空闲阈值（分钟）。设置页把它改成别的值时以配置为准。
pub const DEFAULT_MINUTES: u32 = 10;
/// 阈值下限/上限：小于 1 分钟等于藏起来就清，用户点开托盘反而更慢；上限一天。
pub const MIN_MINUTES: u32 = 1;
pub const MAX_MINUTES: u32 = 24 * 60;
/// 后台检查的节拍。这个循环只读一个 AtomicBool，开销可以忽略。
const TICK: Duration = Duration::from_secs(30);

static ENTERED: AtomicBool = AtomicBool::new(false);
/// 窗口最后一次「不可见」的时刻；可见时清空。用 `Instant` 而不是时间戳，
/// 因为系统时钟被改不该影响「藏了多久」。
static HIDDEN_SINCE: Mutex<Option<Instant>> = Mutex::new(None);
/// 命令通道。轻量模式恢复时要让原来的取数逻辑重跑一遍，而那条通道由 UI 注册
/// 函数持有，所以启动时把它交给这里存一份。
static TX: Mutex<Option<mpsc::UnboundedSender<Cmd>>> = Mutex::new(None);
/// 进轻量模式时的表单快照，退出时写回。None 表示当前没有待恢复的快照
/// （没进过轻量模式，或者已经恢复过了）。
static DRAFT: Mutex<Option<Drafts>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // 中毒只说明之前某次持锁时 panic 过；这类状态没有不变量可言，直接用里面的值。
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 由 UI 注册函数调用一次，交出发送端。
pub fn install_tx(tx: mpsc::UnboundedSender<Cmd>) {
    *lock(&TX) = Some(tx);
}

fn send(cmd: Cmd) {
    if let Some(tx) = lock(&TX).as_ref() {
        let _ = tx.send(cmd);
    }
}

/// 把配置里的分钟数收进合法区间。0 或超大值都是配置被人手改坏的迹象，
/// 这里纠正而不是报错——轻量模式的参数不值得让整个设置页打不开。
pub fn clamp_minutes(minutes: u32) -> u32 {
    minutes.clamp(MIN_MINUTES, MAX_MINUTES)
}

/// 是否已经进入轻量模式（界面侧据此决定要不要显示提示）。
pub fn is_light() -> bool {
    ENTERED.load(Ordering::SeqCst)
}

/// 这次「写入行模型」要不要放弃：轻量模式里且要写的不是空表，就放弃。
///
/// 进入轻量模式时已经发出的取数请求还会陆续回来。它们在界面不在屏幕上时写进模型，
/// 唯一效果就是把刚清掉的几千行重新装回内存——用户看不到，下次进入还得再清一遍。
/// 空表必须放行：`clear_ui` 自己就是靠写入空表来清空的。
/// 退出轻量模式后 `leave()` 会重发取数命令，丢掉的这批会重新取回来。
pub fn suppress_rows(n: usize) -> bool {
    n > 0 && is_light()
}

/// 纯判定：藏了 `idle` 这么久、阈值 `minutes` 分钟，该不该清。
///
/// 单独拆出来是因为这是本模块唯一能用单元测试断言的部分——其余都要真窗口。
pub fn should_enter(idle: Duration, minutes: u32) -> bool {
    let m = clamp_minutes(minutes);
    idle >= Duration::from_secs(u64::from(m) * 60)
}

/// UI 线程每 500ms 采样窗口可见性，调用这里记录。
///
/// 参数为 `true`（可见且未最小化）时清空计时；为 `false` 时只在第一次记下时刻，
/// 反复调用不会把计时往后推。
pub fn note_visible(visible: bool) {
    let mut g = lock(&HIDDEN_SINCE);
    if visible {
        *g = None;
    } else if g.is_none() {
        *g = Some(Instant::now());
    }
}

/// 藏了多久；从没藏起来过返回 0。
pub fn hidden_for() -> Duration {
    lock(&HIDDEN_SINCE).map(|t| t.elapsed()).unwrap_or_default()
}

/// 该不该进出轻量模式。把它从 `watch` 里拆出来是为了能用单元测试断言完整的
/// 状态流转（藏起来→进入、用户回来→恢复、已经进入就不再重复进入），
/// 而不是只测一个布尔表达式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gate {
    /// 窗口当前可见（且未最小化）
    pub visible: bool,
    /// 当前是否已经在轻量模式里
    pub entered: bool,
}

/// `Gate::step` 给出的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Enter,
    Leave,
}

/// 窗口重新可见这一拍该做什么：还在轻量模式里就退出，不看藏了多久、也不等巡检。
///
/// `Cmd::Foregrounded` 对应的动作就是直接调 `leave`，本函数是它和 `Gate::step`
/// 共用的判断，两条路径因此不会给出相反的答案。返回 `None` 表示不必做事
/// （本来就没进轻量模式），调 `leave` 在这种情况下也只是空转。
pub fn step_when_visible(visible: bool, entered: bool) -> Option<Step> {
    if visible && entered {
        Some(Step::Leave)
    } else {
        None
    }
}

impl Gate {
    /// 依据当前状态与「藏了多久」给出这一步该做什么；无事可做返回 `None`。
    ///
    /// 已经在轻量模式时唯一的出口是窗口重新可见，且立即退出（`watch` 每 30 秒
    /// 才醒一次，用户等不起）；还没进入时只在窗口不可见且藏够阈值才进入——
    /// 用户正看着界面时清空列表只会让人眼前一空。
    pub fn step(&self, hidden: Duration, minutes: u32) -> Option<Step> {
        if self.entered {
            step_when_visible(self.visible, self.entered)
        } else if !self.visible && should_enter(hidden, minutes) {
            Some(Step::Enter)
        } else {
            None
        }
    }
}

/// 后台循环：到了阈值就进轻量，重新可见就恢复。
///
/// 由 `main` 在 tokio 运行时里 spawn 一次，程序活多久它跑多久。
pub async fn watch(ctx: Arc<Ctx>, ui: Weak<MainWindow>) {
    loop {
        tokio::time::sleep(TICK).await;
        let cfg = { lock(&ctx.cfg).light.clone() };
        let gate = Gate {
            visible: ctx.visible.load(Ordering::SeqCst),
            entered: is_light(),
        };
        if !cfg.enabled {
            // 用户关掉开关时，如果已经在轻量模式就地恢复，否则会一直停在被清空的状态。
            if gate.entered && gate.visible {
                leave(&ctx, &ui);
            }
            continue;
        }
        match gate.step(hidden_for(), cfg.minutes) {
            Some(Step::Enter) => {
                enter(&ctx, &ui);
            }
            Some(Step::Leave) => {
                // 用户回来了：先恢复再放开轮询，避免界面空白地跳几帧。
                leave(&ctx, &ui);
            }
            None => {}
        }
    }
}

/// 进入轻量模式。返回是否真的进入了（已进入、窗口可见、窗口已销毁都会返回 false）。
pub fn enter(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) -> bool {
    let visible = ctx.visible.load(Ordering::SeqCst);
    let already = is_light();
    if visible {
        // 可见时清理就等于让正在看的人眼前一空，宁可不省这点内存。
        // 万一之前是在不可见时进来的（例如最小化后刚被恢复），顺手补一次恢复。
        if already {
            leave(ctx, ui);
        }
        return false;
    }
    if already {
        return false;
    }
    if ENTERED.swap(true, Ordering::SeqCst) {
        return false;
    }
    drop_caches(ctx);
    // 界面模型的清理必须在 UI 线程做：Slint 的模型是弱引用共享的，跨线程改会撞车。
    let scheduled = ui
        .upgrade_in_event_loop(|u| {
            // 表单刻意不清（见 `clear_ui` 的注释）。快照仍然留下来：
            // 一是有"填了一半没提交"时那行日志要靠它判断，二是退出时按快照把
            // 类型下拉和同步任务那几项写回去——后端参数模板被清过、重新拉取会
            // 把类型选择重置成默认，用户刚选的那一项就没了。
            let draft = Drafts::capture(&u);
            if draft.has_input() {
                crate::logging::event(
                    "info",
                    "lightweight",
                    "进入轻量模式时界面里还有未提交的输入，已原样保留",
                );
            }
            *lock(&DRAFT) = Some(draft);
            clear_ui(&u)
        })
        .is_ok();
    if !scheduled {
        ENTERED.store(false, Ordering::SeqCst);
        return false;
    }
    trim_working_set();
    crate::logging::event(
        "info",
        "lightweight",
        "进入轻量模式：清空列表与插件面板位图，释放 wasm 模块缓存",
    );
    true
}

/// 恢复。返回是否真的恢复过（本来就没进轻量模式返回 false）。
pub fn leave(ctx: &Arc<Ctx>, ui: &Weak<MainWindow>) {
    if !ENTERED.swap(false, Ordering::SeqCst) {
        return;
    }
    // 被清掉的列表不猜内容，按当前页重新取一遍。取数逻辑只有一份，
    // 在这里复制一份"恢复用"的版本才是真正会出错的地方。
    let _ = ui.upgrade_in_event_loop(|ui| {
        // 先把进轻量模式时拍的快照写回去，再让取数逻辑跑。写回的是同一份表单，
        // 内容没变时就是一次无害的赋同样值；变过的地方（类型下拉被下拉数据重建、
        // 同步任务的方向/周期被默认值覆盖）靠这一步还原。
        if let Some(draft) = lock(&DRAFT).take() {
            draft.restore(&ui);
        }
        // 插件面板是唯一需要额外一条命令才能回来的：清掉 panel_rows 后
        // 面板还开着但只剩空壳，按插件 id 找回行号再重新打开一次，
        // 让宿主重建面板（重建会重新读一遍 config.json 的字段表）。
        let g = ui.global::<PluginsG>();
        let open_id = g.get_panel_plugin().to_string();
        if !open_id.is_empty() {
            let rows = g.get_rows();
            let idx = (0..rows.row_count())
                .find(|i| rows.row_data(*i).map(|r| r.id == open_id).unwrap_or(false))
                .map(|i| i as i32);
            match idx {
                Some(idx) => send(Cmd::PluginPanelOpen { idx }),
                // 列表被谁清空了。不说一声的话，面板会安静地停在空壳上。
                None => crate::logging::event(
                    "warn",
                    "lightweight",
                    &format!("退出轻量模式时找不到面板所属的插件（{open_id}），面板保持空白"),
                ),
            }
        }
    });
    send(Cmd::RefreshDashboard);
    send(Cmd::FilesInit);
    send(Cmd::LocalRefresh { path: None });
    send(Cmd::RemotesRefresh);
    send(Cmd::MountsRefresh);
    send(Cmd::ServesRefresh);
    send(Cmd::TransfersEnter);
    crate::logging::event("info", "lightweight", "退出轻量模式，重新拉取界面数据");
    let _ = ctx;
}

/// 丢工作线程侧的缓存。这些是纯内存副本，丢了下次请求会重新取。
fn drop_caches(ctx: &Ctx) {
    ctx.remotes_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    ctx.mounts_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    ctx.serves_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    ctx.tasks_done
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    *ctx.tasks_live.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // providers 是完整的后端参数模板 JSON，几十 KB 起步；清掉后下一次刷新会重新
    // `config/providers`（离线时走已有的失败提示路径）。
    *ctx.providers_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    // wasm 编译产物最占内存，插件调用又低频，重编译的代价换来的是真释放。
    ctx.host.clear_cache();
}

/// 在 UI 线程清空大模型。
fn clear_ui(ui: &MainWindow) {
    crate::set_file_entries(ui, Vec::new());
    ui.global::<LocalG>()
        .set_entries(ModelRc::new(VecModel::<crate::FileRow>::from(Vec::new())));
    ui.global::<LocalG>()
        .set_checked(ModelRc::new(VecModel::<bool>::from(Vec::new())));
    ui.global::<LocalG>().set_checked_count(0);
    ui.global::<crate::DashboardG>()
        .set_xfers(ModelRc::new(VecModel::from(Vec::new())));
    ui.global::<TransfersG>()
        .set_tasks(ModelRc::new(VecModel::from(Vec::new())));
    ui.global::<MountsG>()
        .set_mounts(ModelRc::new(VecModel::from(Vec::new())));
    ui.global::<ServesG>()
        .set_rows(ModelRc::new(VecModel::from(Vec::new())));
    ui.global::<RemotesG>()
        .set_rows(ModelRc::new(VecModel::from(Vec::new())));
    // 插件列表（PluginsG.rows）故意保留：每条只有 id/名称/几个状态布尔，几十条加起来
    // 也就几 KB；而 `leave()` 要靠它把 `panel_plugin` 的 id 换回行号，才能重新打开面板。
    // 真正占内存的是 panel_rows 里那些解码后的位图，下面照清不误。
    // panel_rows 里带解码后的位图，这一条是界面侧真正的大头。
    // 页签和行一起清：留着上一个插件的页签，用户点进去只会看到空白。
    crate::plugins::clear_panel(ui);
    // 表单相关的小对象保留：编辑器的字段表、类型下拉都是几十条字符串，
    // 清掉省不下内存，却可能让用户正在填的东西消失。
    let _ = ui.global::<MaintenanceG>().get_sync_name();
    let _ = ui.global::<RemotesG>().get_edit_fields();
}

/// 让系统把本进程的工作集里能挪的页面挪出去。
///
/// 这不是"释放内存"，只是让任务管理器的数字好看——真正的释放发生在上面的模型与
/// 缓存清理。调用它是因为用户判断"轻量模式生效了没"看的就是这个数字。
#[cfg(windows)]
fn trim_working_set() {
    use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    // 失败也无所谓：清理已经做完了，这只是锦上添花。
    unsafe {
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }
}

#[cfg(not(windows))]
fn trim_working_set() {}

/// 供目标 7 的测试与状态提示使用：说明这次轻量模式到底清了什么。
pub fn what_is_cleared() -> &'static [&'static str] {
    &[
        "远程/本地文件列表与勾选状态",
        "仪表盘与传输页的任务行",
        "挂载与服务列表",
        "远程列表",
        "插件面板（含已解码的位图）与页签",
        "wasm 模块编译缓存与后端参数模板缓存",
    ]
}

/// 不会被轻量模式清掉的东西，写出来是为了不给用户"省电模式省掉了一切"的错觉。
pub fn what_is_kept() -> &'static [&'static str] {
    &[
        "内核连接与正在运行的传输/挂载/服务",
        "定时同步任务与它们的下一次运行时间",
        "正在编辑的远程表单（字段表、类型下拉、代理）",
        // 这几十行只是 id 和几个状态布尔，占不了多少地方；退出时得靠它把面板找回来。
        "插件列表本身（只有元数据，占地方的是面板位图）",
        "设置项、主题、语言",
    ]
}

/// 字段表快照。当前没有清空字段表，这个结构留给需要"先存再清"的调用方，
/// 免得以后有人为了省几个字节又把用户填了一半的表单丢掉。
#[derive(Clone, Debug, Default)]
pub struct Drafts {
    pub fields: Vec<FieldRow>,
    pub edit_name: String,
    pub type_idx: i32,
    pub type_query: String,
    pub sync_name: String,
    pub sync_local: String,
    pub sync_remote: String,
    pub sync_direction: i32,
    pub sync_hours: i32,
    /// 插件面板上选中的页签。面板本身会被清掉（`plugins::clear_panel` 连页签一起清），
    /// 但重开面板时 `apply_panel` 会尽量保留当前页——所以退出轻量模式前先把页签放回去。
    pub plugin_page: String,
}

impl Drafts {
    /// 从界面读一份快照。必须在 UI 线程调用。
    pub fn capture(ui: &MainWindow) -> Self {
        let r = ui.global::<RemotesG>();
        let m = ui.global::<MaintenanceG>();
        let fields = r.get_edit_fields();
        let mut out = Drafts {
            fields: Vec::with_capacity(fields.row_count()),
            edit_name: r.get_edit_name().to_string(),
            type_idx: r.get_type_idx(),
            type_query: r.get_type_query().to_string(),
            sync_name: m.get_sync_name().to_string(),
            sync_local: m.get_sync_local().to_string(),
            sync_remote: m.get_sync_remote().to_string(),
            sync_direction: m.get_sync_direction(),
            sync_hours: m.get_sync_hours(),
            plugin_page: ui.global::<PluginsG>().get_panel_page().to_string(),
        };
        for i in 0..fields.row_count() {
            if let Some(f) = fields.row_data(i) {
                out.fields.push(f);
            }
        }
        out
    }

    /// 把快照写回界面。必须在 UI 线程调用。
    ///
    /// 只补「界面上这一格现在是空的」的位置：已经有值就说明它跟快照一样、或者比快照
    /// 更新（后台任务写的），直接保留，免得把用户之后的编辑盖掉。现在进入轻量模式
    /// 并不清空表单，于是这条路通常一处都不用写；它的价值是在将来真去清表单时仍然
    /// 只补不回滚。
    ///
    /// `plugin_page` 一定要在这里放回去：退出轻量模式后紧接着重开插件面板，而
    /// `plugins::apply_panel` 会「当前页还在新面板里就保留」。先恢复页签，重开时才
    /// 能选中用户原来那页，而不是退回第一页。
    pub fn restore(&self, ui: &MainWindow) {
        let r = ui.global::<RemotesG>();
        if r.get_edit_fields().row_count() == 0 && !self.fields.is_empty() {
            r.set_edit_fields(ModelRc::new(VecModel::from(self.fields.clone())));
        }
        fill_text(r.get_edit_name().as_str(), &self.edit_name, |v| {
            r.set_edit_name(v.into())
        });
        fill_number(r.get_type_idx(), self.type_idx, |v| r.set_type_idx(v));
        fill_text(r.get_type_query().as_str(), &self.type_query, |v| {
            r.set_type_query(v.into())
        });
        let m = ui.global::<MaintenanceG>();
        fill_text(m.get_sync_name().as_str(), &self.sync_name, |v| {
            m.set_sync_name(v.into())
        });
        fill_text(m.get_sync_local().as_str(), &self.sync_local, |v| {
            m.set_sync_local(v.into())
        });
        fill_text(m.get_sync_remote().as_str(), &self.sync_remote, |v| {
            m.set_sync_remote(v.into())
        });
        fill_number(m.get_sync_direction(), self.sync_direction, |v| {
            m.set_sync_direction(v)
        });
        fill_number(m.get_sync_hours(), self.sync_hours, |v| m.set_sync_hours(v));
        let g = ui.global::<PluginsG>();
        fill_text(g.get_panel_page().as_str(), &self.plugin_page, |v| {
            g.set_panel_page(v.into())
        });
    }

    /// 有没有用户输入过东西（用来决定要不要提醒）。
    pub fn has_input(&self) -> bool {
        !self.edit_name.is_empty()
            || !self.sync_name.is_empty()
            || !self.sync_local.is_empty()
            || !self.sync_remote.is_empty()
            || !self.type_query.is_empty()
            || self.fields.iter().any(|f| !f.value.is_empty())
    }
}

/// 只在界面上这一格还空着时写入快照里的文字。
///
/// 已经有值就不动：那可能和快照一样，也可能是进轻量模式之后后台补上的更新，
/// 用旧快照盖掉会丢东西。
fn fill_text<F: FnOnce(String)>(current: &str, snapshot: &str, set: F) {
    if current.trim().is_empty() && !snapshot.is_empty() {
        set(snapshot.to_string());
    }
}

/// 同上，用于下拉下标与小时数：`0`（以及负数）表示「还没选 / 还没填」。
fn fill_number<F: FnOnce(i32)>(current: i32, snapshot: i32, set: F) {
    if current <= 0 && snapshot > 0 {
        set(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_are_clamped_into_range() {
        assert_eq!(clamp_minutes(0), MIN_MINUTES);
        assert_eq!(clamp_minutes(10), 10);
        assert_eq!(clamp_minutes(99_999), MAX_MINUTES);
    }

    #[test]
    fn enters_only_after_full_threshold() {
        let ten = 10;
        assert!(!should_enter(Duration::from_secs(599), ten));
        assert!(should_enter(Duration::from_secs(600), ten));
        assert!(should_enter(Duration::from_secs(601), ten));
    }

    #[test]
    fn zero_minutes_still_means_one_minute() {
        // 0 是坏配置，不该解释成"立刻清空"。
        assert!(!should_enter(Duration::from_secs(59), 0));
        assert!(should_enter(Duration::from_secs(60), 0));
    }

    #[test]
    fn clearing_and_keeping_lists_are_not_empty() {
        assert!(!what_is_cleared().is_empty());
        assert!(!what_is_kept().is_empty());
    }

    #[test]
    fn hidden_window_enters_light_mode() {
        let ten = 10;
        let hidden_long = Duration::from_secs(600);
        assert_eq!(
            Gate {
                visible: false,
                entered: false
            }
            .step(hidden_long, ten),
            Some(Step::Enter)
        );
    }

    #[test]
    fn visible_window_never_enters() {
        // 用户正看着界面时清空列表只会让人眼前一空——这是 m01374 指出的原始 bug。
        let ten = 10;
        assert_eq!(
            Gate {
                visible: true,
                entered: false
            }
            .step(Duration::from_secs(86_400), ten),
            None
        );
    }

    #[test]
    fn short_hide_does_not_enter() {
        assert_eq!(
            Gate {
                visible: false,
                entered: false
            }
            .step(Duration::from_secs(60), 10),
            None
        );
    }

    #[test]
    fn coming_back_leaves_and_staying_hidden_keeps_light() {
        assert_eq!(
            Gate {
                visible: true,
                entered: true
            }
            .step(Duration::ZERO, 10),
            Some(Step::Leave)
        );
        // 已经在轻量模式且仍然隐藏：不该反复"重新进入"（否则每 30 秒清一遍）。
        assert_eq!(
            Gate {
                visible: false,
                entered: true
            }
            .step(Duration::from_secs(86_400), 10),
            None
        );
    }

    #[test]
    fn round_trip_hide_enter_show_leave() {
        // 走一遍完整流转：可见 → 藏起来 → 进入 → 回来 → 退出 → 再藏 → 再进入。
        // 用内嵌函数而不是闭包：闭包会把 entered 一直借走，测试后面就没法再读它的值
        // （cargo check 报 E0503 的原因）。显式传 &mut bool 两边都能看。
        fn step(visible: bool, entered: &mut bool, hidden: Duration, minutes: u32) -> &'static str {
            let gate = Gate {
                visible,
                entered: *entered,
            };
            match gate.step(hidden, minutes) {
                Some(Step::Enter) => {
                    *entered = true;
                    "enter"
                }
                Some(Step::Leave) => {
                    *entered = false;
                    "leave"
                }
                None => "none",
            }
        }
        let ten = 10;
        let mut visible = true;
        let mut entered = false;
        let long = Duration::from_secs(600);
        assert_eq!(step(visible, &mut entered, long, ten), "none");
        visible = false;
        assert_eq!(
            step(visible, &mut entered, Duration::from_secs(60), ten),
            "none"
        );
        assert_eq!(step(visible, &mut entered, long, ten), "enter");
        visible = true;
        assert_eq!(step(visible, &mut entered, Duration::ZERO, ten), "leave");
        assert!(!entered);
        visible = false;
        assert_eq!(step(visible, &mut entered, long, ten), "enter");
        assert!(entered);
    }

    #[test]
    fn hide_timer_records_first_hide_and_resets_on_show() {
        note_visible(true);
        assert_eq!(hidden_for(), Duration::ZERO);
        note_visible(false);
        std::thread::sleep(Duration::from_millis(20));
        let first = hidden_for();
        assert!(first > Duration::ZERO);
        // 反复采样不该把起始时刻往后推
        std::thread::sleep(Duration::from_millis(20));
        assert!(hidden_for() > first);
        note_visible(true);
        assert_eq!(hidden_for(), Duration::ZERO);
    }
}
