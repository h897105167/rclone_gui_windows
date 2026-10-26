//! rclone 状态快照：UI 的**唯一数据来源**，以及两条更新线路的门控。
//!
//! ## 为什么单独一个模块
//!
//! 旧架构是「用户指令 → 改全局配置 → 各界面按配置重画」。它和 rclone 的异步状态
//! 天然冲突：rclone 可以在任何时刻自己完成挂载、被外部停掉、或另一个实例接管端口，
//! 而界面只有人点按钮时才动。实测到的两个症状：
//!
//! - 挂了 T: 但仪表盘与状态栏仍显示「挂载 0」（两处计数来自不同调用，节奏也不同）；
//! - 用户操作与周期刷新并发，旧快照能覆盖掉刚发生的新状态。
//!
//! 现在改成：
//!
//! ```text
//!   rclone（真相） ──轮询/操作前后对齐──→ Snapshot ──→ 界面
//!   用户输入      ──只负责下发命令──────┘
//! ```
//!
//! ## 两条更新线路
//!
//! 1. **轮询**：窗口在前台且无用户输入时，每秒取一份完整快照（remote/mount/serve/stats
//!    一次取齐），内容没变就**不写 UI**，避免无谓重绘。
//! 2. **用户输入**：先暂停轮询，**动手前先对齐一次**（否则「点挂载」是在旧状态上做判断），
//!    执行，再对齐一次，然后恢复轮询。
//!
//! 窗口最小化 / 隐藏到托盘时两条线路都停（见 `main.rs` 的可见性采样）。
//!
//! ## 世代号（epoch）
//!
//! 请求是并发的，响应可能乱序返回。`Hub` 给每次取数发号、记录已提交号；旧号回来时
//! 直接丢弃 —— 这就是「旧响应永远不能覆盖新状态」的落点。取数期间如果用户开始了操作，
//! 这份数据也不再可信（它可能是操作前的），同样丢弃。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 一个 remote 的可展示状态。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteState {
    pub name: String,
    /// 后端类型；`config/dump` 取不到时为空（界面上显示为 `?`）
    pub rtype: String,
    /// 用量文本（`operations/about` 的异步结果，可能滞后于快照本身）
    pub usage: String,
}

/// 一个挂载点。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountState {
    pub fs: String,
    pub mount_point: String,
}

/// 一个运行中的服务（原始字段；显示名由调用方按档案映射）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeState {
    pub id: String,
    pub stype: String,
    pub fs: String,
    pub addr: String,
}

/// 传输统计（`core/stats` 的投影）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatsState {
    pub speed: f64,
    pub bytes: f64,
    pub errors: i64,
    pub transfers: usize,
    /// 正在传输的行（界面直接渲染，避免又一次请求）
    pub xfers: Vec<XferState>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct XferState {
    pub name: String,
    pub size: f64,
    pub speed: f64,
    pub percent: f64,
}

/// 一份完整状态。`PartialEq` 用来判断「和上次比有没有变」，没变就不碰 UI。
///
/// 注意：**不要**往里加时间戳这类每次都变的字段 —— 那会让「没变就不重绘」永远失效
/// （第一版就踩了这个坑：`at_epoch_ms` 让快照每秒都「不同」）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub remotes: Vec<RemoteState>,
    pub mounts: Vec<MountState>,
    pub serves: Vec<ServeState>,
    pub stats: StatsState,
    /// `core/version` 的版本串（空 = 还没取到）
    pub version: String,
    /// `os / arch`
    pub os_info: String,
}

impl Snapshot {
    /// 计数与列表同源：界面上的「挂载 N」就是列表长度，不存在第二条统计路径。
    ///
    /// `pending` = 正在挂载中的条目数（它们已经出现在列表里，所以也要算进去，
    /// 否则会出现「列表 2 条、计数 1 个」这种自相矛盾的画面）。
    pub fn mount_count(&self, pending: usize) -> usize {
        self.mounts.len() + pending
    }

    /// 运行中服务的数量（状态栏 / 测试用）。
    pub fn serve_count(&self) -> usize {
        self.serves.len()
    }
}

/// 各部分取数结果：`None` = 这一路失败或超时。
///
/// 失败时**保留上一份快照的对应部分**，而不是把列表清成 0 —— 否则一次瞬时超时
/// 就会让「挂载 1」闪成「挂载 0」，正是这次要修的那类误导。
#[derive(Debug, Default)]
pub struct Fetched {
    pub remotes: Option<Vec<RemoteState>>,
    pub mounts: Option<Vec<MountState>>,
    pub serves: Option<Vec<ServeState>>,
    pub stats: Option<StatsState>,
    pub version: Option<String>,
    pub os_info: Option<String>,
}

impl Fetched {
    /// 把新取到的部分合进旧快照；`prev` 为空时缺项按空列表处理（首次还没数据）。
    pub fn merge_into(self, prev: Option<&Snapshot>) -> Snapshot {
        let prev = prev.cloned().unwrap_or_default();
        Snapshot {
            remotes: self.remotes.unwrap_or(prev.remotes),
            mounts: self.mounts.unwrap_or(prev.mounts),
            serves: self.serves.unwrap_or(prev.serves),
            stats: self.stats.unwrap_or(prev.stats),
            version: self.version.unwrap_or(prev.version),
            os_info: self.os_info.unwrap_or(prev.os_info),
        }
    }
}

/// 一次取数的凭据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchToken {
    /// 本次取数的序号（递增）
    pub seq: u64,
    /// 取数开始时的操作序号。取数期间用户开始了操作 → 这份数据不可信
    pub op_seq: u64,
}

impl FetchToken {
    /// 本次取数的序号（测试用，便于断言先后关系）。
    #[cfg(test)]
    pub fn seq(self) -> u64 {
        self.seq
    }
}

/// 轮询是否允许写 UI / 发请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStatus {
    /// 可以轮询
    Running,
    /// 用户在操作：轮询让位，由操作方在前后各对齐一次
    UserBusy,
    /// 窗口不可见：完全停止（不发请求、不写 UI）
    Hidden,
    /// 没连上：离线模式，不发 RC 请求
    Offline,
}

/// 两条线路的仲裁与世代号。全部同步接口，便于单测。
#[derive(Debug, Default)]
pub struct Hub {
    /// 已发出的取数序号
    fetch_seq: AtomicU64,
    /// 已提交到 UI 的最大序号
    commit_seq: AtomicU64,
    /// 用户操作序号（开始一次操作 +1）
    op_seq: AtomicU64,
    /// 正在进行的用户操作数
    ops_in_flight: AtomicU64,
    /// 正在进行的取数数（用于「不叠加同类取数」）
    fetches_in_flight: AtomicU64,
    /// 上一次已应用的快照（用于「没变就不重绘」与失败合并）
    applied: Mutex<Option<Snapshot>>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开始一次取数。占用期间 `fetches_in_flight` > 0。
    pub fn begin_fetch(self: &std::sync::Arc<Self>) -> FetchGuard {
        self.fetches_in_flight.fetch_add(1, Ordering::SeqCst);
        FetchGuard {
            hub: self.clone(),
            token: FetchToken {
                seq: self.fetch_seq.fetch_add(1, Ordering::SeqCst) + 1,
                op_seq: self.op_seq.load(Ordering::SeqCst),
            },
        }
    }

    /// 是否已有同类取数在跑（轮询用：不叠加，避免 rclone 慢时任务堆积）。
    pub fn fetch_in_flight(&self) -> bool {
        self.fetches_in_flight.load(Ordering::SeqCst) > 0
    }

    /// 提交结果：返回 true 表示可以写 UI。
    ///
    /// 丢弃的两种情形：
    /// - 取数期间用户开始了操作（数据可能早于操作）；
    /// - 已经有更新的结果提交过（乱序回来）。
    pub fn try_commit(&self, token: FetchToken) -> bool {
        if token.op_seq != self.op_seq.load(Ordering::SeqCst) {
            return false;
        }
        if token.seq <= self.commit_seq.load(Ordering::SeqCst) {
            return false;
        }
        self.commit_seq.store(token.seq, Ordering::SeqCst);
        true
    }

    /// 记下「已应用的快照」，并告诉调用方内容是否变化。
    ///
    /// 返回 true = 与上次不同，需要写 UI；false = 一模一样，跳过重绘。
    pub fn note_applied(&self, snap: &Snapshot, force: bool) -> bool {
        let mut g = self.applied.lock().unwrap();
        if !force {
            if let Some(prev) = g.as_ref() {
                if prev == snap {
                    return false;
                }
            }
        }
        *g = Some(snap.clone());
        true
    }

    pub fn applied(&self) -> Option<Snapshot> {
        self.applied.lock().unwrap().clone()
    }

    /// 用户操作开始：轮询让位。
    pub fn begin_user_op(&self) {
        self.ops_in_flight.fetch_add(1, Ordering::SeqCst);
        self.op_seq.fetch_add(1, Ordering::SeqCst);
    }

    /// 用户操作结束。
    pub fn end_user_op(&self) {
        // 用 saturating 语义，防止不配对的调用把计数减到负数（u64 回绕）
        let _ = self
            .ops_in_flight
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                Some(n.saturating_sub(1))
            });
    }

    pub fn user_busy(&self) -> bool {
        self.ops_in_flight.load(Ordering::SeqCst) > 0
    }

    /// 当前是否应该轮询（纯判定，不做 IO）。
    pub fn status(&self, visible: bool, live: bool) -> PollStatus {
        if !visible {
            PollStatus::Hidden
        } else if self.user_busy() {
            PollStatus::UserBusy
        } else if !live {
            PollStatus::Offline
        } else {
            PollStatus::Running
        }
    }
}

/// 一次取数的守卫：Drop 时释放 in-flight 计数。
pub struct FetchGuard {
    hub: std::sync::Arc<Hub>,
    token: FetchToken,
}

impl FetchGuard {
    /// 本次取数的凭据（测试用；正式路径直接 `commit()`）。
    #[cfg(test)]
    pub fn token(&self) -> FetchToken {
        self.token
    }

    /// 消费守卫并提交（失败时也释放计数）。
    pub fn commit(self) -> bool {
        let ok = self.hub.try_commit(self.token);
        ok
    }
}

impl Drop for FetchGuard {
    fn drop(&mut self) {
        let _ = self
            .hub
            .fetches_in_flight
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                Some(n.saturating_sub(1))
            });
    }
}

/// 用户操作守卫：构造即暂停轮询，Drop 恢复。
///
/// 持有 `Arc<Hub>`（而不是借用）是为了能跨 `tokio::spawn` 移动 —— 命令是被
/// spawn 出去执行的，守卫必须跟着命令一起活着。用 Drop 而不是手动配对，
/// 是为了让提前 `return`、`?`、panic 都不会让轮询永久停摆。
pub struct UserOpGuard {
    hub: std::sync::Arc<Hub>,
}

impl UserOpGuard {
    pub fn begin(hub: &std::sync::Arc<Hub>) -> Self {
        hub.begin_user_op();
        Self { hub: hub.clone() }
    }
}

impl Drop for UserOpGuard {
    fn drop(&mut self) {
        self.hub.end_user_op();
    }
}

// ---------------- 本机盘符（实例不匹配的提示） ----------------

/// 本机上由 rclone 挂载出来的盘符（卷文件系统名为 `FUSE-rclone`）。
///
/// 用途：当用户看到「挂了 T: 但界面显示挂载 0」时，说明这个盘属于**另一个**
/// rclone 实例（RC 端点不同）。界面只反映当前连接的实例，所以要明确说出来，
/// 而不是让用户以为界面错了。
///
/// 约束：不扫描端口、不探测未知服务，只读本机卷信息（低风险、只读）。
#[cfg(windows)]
pub fn local_rclone_drives() -> Vec<String> {
    use windows::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    // Win32 的 DRIVE_FIXED 常量，在 `Win32::System::WindowsProgramming` 里；
    // 为省一个 feature 依赖，这里用它的字面值（微软文档固定为 3）。
    const DRIVE_FIXED: u32 = 3;
    let mut out = Vec::new();
    // SAFETY: 两个 API 都只读系统状态；缓冲区由本函数持有。
    unsafe {
        let mask = GetLogicalDrives();
        if mask == 0 {
            return out;
        }
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let root_w: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            let root_pc = windows::core::PCWSTR(root_w.as_ptr());
            if GetDriveTypeW(root_pc) != DRIVE_FIXED {
                continue;
            }
            let mut label = [0u16; 256];
            let mut fs_name = [0u16; 64];
            let mut serial = 0u32;
            let mut max_comp = 0u32;
            let mut flags = 0u32;
            if GetVolumeInformationW(
                root_pc,
                Some(&mut label),
                Some(&mut serial),
                Some(&mut max_comp),
                Some(&mut flags),
                Some(&mut fs_name),
            )
            .is_err()
            {
                continue;
            }
            let len = fs_name.iter().position(|c| *c == 0).unwrap_or(0);
            let fs = String::from_utf16_lossy(&fs_name[..len]);
            if fs.to_ascii_lowercase().contains("rclone") {
                out.push(format!("{letter}:"));
            }
        }
    }
    out
}

#[cfg(not(windows))]
pub fn local_rclone_drives() -> Vec<String> {
    Vec::new()
}

/// 本机的**固定**盘符（`C:`、`D:` …），用于「搜索本机程序」时限定扫描范围。
///
/// 只返回 `DRIVE_FIXED`：网络盘（`DRIVE_REMOTE`）和可移动盘不扫 —— 搜索一个
/// 可执行文件没有理由去挂载点或 U 盘上逐个目录翻，那会把一次点击变成几分钟的等待。
#[cfg(windows)]
pub fn local_fixed_drives() -> Vec<String> {
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    const DRIVE_FIXED: u32 = 3;
    let mut out = Vec::new();
    // SAFETY: 只读系统状态，无缓冲区。
    unsafe {
        let mask = GetLogicalDrives();
        if mask == 0 {
            return out;
        }
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root: Vec<u16> = format!("{letter}:\\")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            if GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) == DRIVE_FIXED {
                out.push(format!("{letter}:"));
            }
        }
    }
    out
}

#[cfg(not(windows))]
pub fn local_fixed_drives() -> Vec<String> {
    Vec::new()
}

/// 盘符提示的节流：卷信息查询本身很便宜，但网络盘在断线时可能变慢，因此限频。
pub struct DriveProbe {
    last: Mutex<Option<(Instant, Vec<String>)>>,
}

impl Default for DriveProbe {
    fn default() -> Self {
        Self {
            last: Mutex::new(None),
        }
    }
}

impl DriveProbe {
    pub fn cached(&self, max_age: Duration) -> Vec<String> {
        let mut g = self.last.lock().unwrap();
        if let Some((at, v)) = g.as_ref() {
            if at.elapsed() < max_age {
                return v.clone();
            }
        }
        let v = local_rclone_drives();
        *g = Some((Instant::now(), v.clone()));
        v
    }

    /// 生成挂载页的提示：rclone 列表为空、但本机存在 rclone 盘符时才有内容。
    pub fn mismatch_hint(&self, endpoint_url: &str, listed: &[MountState]) -> String {
        if !listed.is_empty() {
            return String::new();
        }
        let drives = self.cached(Duration::from_secs(10));
        hint_text(endpoint_url, listed, &drives)
    }
}

/// 提示文案（纯函数，便于单测）。
///
/// 只有「当前端点报告 0 个挂载」且「本机确实有 rclone 盘符」时才给提示 ——
/// 否则要么界面没问题，要么没有证据，都不该打扰用户。
pub fn hint_text(endpoint_url: &str, listed: &[MountState], drives: &[String]) -> String {
    if !listed.is_empty() || drives.is_empty() {
        return String::new();
    }
    rcgui_i18n::f("本机存在 rclone 挂载盘 {}，但当前连接的 {} 报告没有挂载。该盘属于另一个 rclone 实例；\
         界面只反映当前端点的状态。", &[&(drives.join("、")), &(if endpoint_url.is_empty() {
            rcgui_i18n::t("端点")
        } else {
            endpoint_url
        })])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_with(mounts: usize) -> Snapshot {
        Snapshot {
            mounts: (0..mounts)
                .map(|i| MountState {
                    fs: "teldrive:".into(),
                    mount_point: format!("T{i}:"),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn count_matches_list_length() {
        let s = snap_with(3);
        assert_eq!(s.mount_count(0), 3, "计数必须就是列表长度");
        assert_eq!(s.mount_count(0), s.mounts.len());
    }

    #[test]
    fn count_includes_pending_mounts() {
        // 正在挂载的行已经出现在列表里（spinner），计数不加上它就会出现
        // 「列表 2 条、标题写 1 个」这种自相矛盾的画面
        let s = snap_with(1);
        assert_eq!(s.mount_count(1), 2);
    }

    #[test]
    fn fetch_seq_increases_monotonically() {
        let hub = std::sync::Arc::new(Hub::new());
        let a = hub.begin_fetch();
        let b = hub.begin_fetch();
        assert!(b.token().seq() > a.token().seq(), "取数序号必须单调递增");
    }

    #[test]
    fn older_fetch_is_discarded_after_newer_commit() {
        let hub = std::sync::Arc::new(Hub::new());
        let a = hub.begin_fetch();
        let b = hub.begin_fetch();
        assert!(b.commit(), "较新的结果先到：可以提交");
        assert!(!a.commit(), "较旧的结果后到：必须丢弃");
    }

    #[test]
    fn fetch_started_before_user_op_is_discarded() {
        let hub = std::sync::Arc::new(Hub::new());
        let a = hub.begin_fetch();
        hub.begin_user_op();
        assert!(!a.commit(), "取数期间用户开始操作 → 数据可能早于操作，不可用");
        hub.end_user_op();
        let b = hub.begin_fetch();
        assert!(b.commit(), "操作结束后重新取的数据可以提交");
    }

    #[test]
    fn fetch_guard_releases_in_flight_even_on_drop() {
        let hub = std::sync::Arc::new(Hub::new());
        {
            let g = hub.begin_fetch();
            assert!(hub.fetch_in_flight(), "取数期间必须标为在跑（轮询不叠加）");
            let _ = g.token();
        }
        assert!(!hub.fetch_in_flight(), "Drop 后必须放开，否则轮询永久停摆");
    }

    #[test]
    fn user_op_pauses_polling_until_drop() {
        let hub = std::sync::Arc::new(Hub::new());
        assert_eq!(hub.status(true, true), PollStatus::Running);
        {
            let _op = UserOpGuard::begin(&hub);
            assert_eq!(hub.status(true, true), PollStatus::UserBusy);
        }
        assert_eq!(hub.status(true, true), PollStatus::Running, "守卫 Drop 后必须恢复");
    }

    #[test]
    fn hidden_wins_over_user_busy() {
        let hub = std::sync::Arc::new(Hub::new());
        let _op = UserOpGuard::begin(&hub);
        assert_eq!(hub.status(false, true), PollStatus::Hidden);
        assert_eq!(hub.status(true, false), PollStatus::UserBusy);
    }

    #[test]
    fn offline_when_not_live() {
        let hub = std::sync::Arc::new(Hub::new());
        assert_eq!(hub.status(true, false), PollStatus::Offline);
    }

    #[test]
    fn unchanged_snapshot_is_not_applied() {
        let hub = Hub::new();
        let s = snap_with(1);
        assert!(hub.note_applied(&s, false), "首次必须应用");
        assert!(!hub.note_applied(&s, false), "一模一样时不应重绘");
        assert!(hub.note_applied(&s, true), "force 时即使相同也要应用");
        let changed = snap_with(2);
        assert!(hub.note_applied(&changed, false), "内容变化时必须应用");
    }

    #[test]
    fn failed_section_keeps_previous_values() {
        let prev = snap_with(1);
        let mut prev = prev;
        prev.remotes.push(RemoteState {
            name: "teldrive".into(),
            rtype: "teldrive".into(),
            usage: "1 GB".into(),
        });
        // 只有 stats 成功、mounts 失败
        let fetched = Fetched {
            stats: Some(StatsState {
                speed: 1.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = fetched.merge_into(Some(&prev));
        assert_eq!(merged.mounts.len(), 1, "取数失败不能让挂载数变成 0");
        assert_eq!(merged.remotes.len(), 1);
        assert_eq!(merged.stats.speed, 1.0, "成功的部分要更新");
    }

    #[test]
    fn unpaired_end_does_not_underflow() {
        let hub = std::sync::Arc::new(Hub::new());
        hub.end_user_op();
        assert!(!hub.user_busy(), "多调一次 end 也不能把计数绕成天文数字");
    }

    #[test]
    fn mismatch_hint_only_when_list_empty() {
        let p = DriveProbe::default();
        // 有条目时永远不提示（不依赖本机盘符探测结果）
        let hint = p.mismatch_hint("http://127.0.0.1:5572", &snap_with(1).mounts);
        assert!(hint.is_empty());
    }

    #[test]
    fn hint_text_names_endpoint_and_drive() {
        let drives = vec!["T:".to_string()];
        let hint = hint_text("http://127.0.0.1:5572", &[], &drives);
        assert!(hint.contains("T:"), "必须说出是哪个盘：{hint}");
        assert!(hint.contains("127.0.0.1:5572"), "必须说出当前端点：{hint}");
        assert!(hint.contains("另一个 rclone 实例"), "要解释清楚归属：{hint}");
    }

    #[test]
    fn hint_text_empty_without_evidence() {
        assert!(hint_text("http://x", &[], &[]).is_empty(), "没有盘符就不提示");
        assert!(
            hint_text("http://x", &snap_with(1).mounts, &["T:".into()]).is_empty(),
            "当前端点明明有挂载时不该提示"
        );
    }
}
