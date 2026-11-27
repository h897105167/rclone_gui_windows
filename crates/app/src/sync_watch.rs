//! 本机文件夹一变，就让同步提前跑一轮。
//!
//! 变化来自系统的文件通知（Windows 上是 `ReadDirectoryChangesW`，由 `notify` 包成同一套接口），
//! 不是定时比较目录。比较目录只能看见「文件数、总字节、最新修改时间」，同尺寸改写一个旧文件、
//! 改名、改完再改回去都会漏过去——界面写着「自动：开」，实际上漏掉的那次永远不会同步。
//!
//! 通知本身也会断：缓冲区溢出、网络盘、目录被删掉再重建，都可能收不到事件。所以除了事件还有
//! 一条补救路径：重扫一次，只回答「这棵树里最新的修改时间是不是晚于上次同步」。它不是指纹，
//! 也不拿整棵树当判据；而且只在刚接手一个任务、或者通知报告了问题之后跑一次，跑在单独的线程里。
//! 主循环里没有递归扫描——那是拿界面线程去读整块磁盘。
//!
//! 真正执行同步仍然交给 [`crate::sync_tasks`]，任务的 mode 和 direction 一个字都不改。有两条底线：
//!
//! - 只顺手「本地 → 远程」的任务。反方向时本机文件的变化正是用户自己的编辑，拿它触发同步
//!   就是把编辑覆盖掉（见 [`watches`]）。
//! - 镜像模式（`sync`，会删对端）默认不跟着变化跑。事件误判一次就可能删一大片，那一步得用户
//!   自己打开，不该由一次改名决定。
//!
//! 「有变化待同步」是状态，不是一次性判断：同步正在跑的时候来的变化会留到这一轮结束之后再跑一轮，
//! 启动失败也一样，隔一会儿重试。

use crate::config::SyncTask;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

/// 最后一条变化之后还要安静多久才动手。一次保存常常连着冒出好几条事件，
/// 立刻跑会把一次编辑拆成好几次同步。
pub const SETTLE: Duration = Duration::from_secs(3);

/// 一轮失败之后隔多久再试。失败原因多半是网络或对端拒绝，紧跟着重试只会连着失败。
pub const RETRY: Duration = Duration::from_secs(60);

/// 补救扫描最多看多少项。超过就报错而不是静默放过——「说在监控其实没监控」比不监控更糟。
pub const MAX_ENTRIES: u64 = 200_000;

/// 补救扫描最多往下走多少层，同样超了就报错。
const MAX_DEPTH: usize = 64;

/// 这条任务要不要跟着本机变化跑。
///
/// 「没选过」（老配置文件没有这一项）时按模式给默认值：只复制的任务盯，镜像任务不盯。
pub fn watches(task: &SyncTask) -> bool {
    if !task.enabled {
        return false;
    }
    if !crate::sync_tasks::is_up(task) {
        return false;
    }
    match task.auto_sync {
        Some(flag) => flag,
        None => !crate::sync_tasks::is_mirror(task),
    }
}

/// 一条任务「有没有变化等着同步」的状态。
///
/// 单独拆出来是为了能直接测：这些判断一旦错了，症状是「用户改了文件，同步没动」，
/// 而那在界面上看不出来。
#[derive(Debug, Default)]
pub struct Pending {
    dirty: bool,
    last: Option<Instant>,
    running: bool,
    retry_at: Option<Instant>,
    problem: Option<String>,
}

impl Pending {
    /// 收到一次变化。事件抖动不用在这里去重：静默窗口本来就会把一串事件合成一轮。
    pub fn note_change(&mut self, now: Instant) {
        self.dirty = true;
        self.last = Some(now);
    }

    /// 记下盯不住的原因。返回 true 表示这是一条「新」消息，界面只需要说一次。
    pub fn note_problem(&mut self, why: impl Into<String>) -> bool {
        let why = why.into();
        let fresh = self.problem.as_deref() != Some(why.as_str());
        self.problem = Some(why);
        fresh
    }

    /// 这一轮该不该现在跑；该跑就把状态切到「正在跑」，避免下一拍再触发一次。
    pub fn take_ready(&mut self, now: Instant) -> bool {
        if self.running || !self.dirty {
            return false;
        }
        if let Some(last) = self.last {
            if now.saturating_duration_since(last) < SETTLE {
                return false;
            }
        }
        if let Some(at) = self.retry_at {
            if now < at {
                return false;
            }
        }
        self.dirty = false;
        self.running = true;
        self.retry_at = None;
        true
    }

    /// 一轮结束。跑的时候来的变化留在 `dirty` 上，静默窗口过后会自己再起一轮。
    pub fn note_round_finished(&mut self, ok: bool, now: Instant) {
        self.running = false;
        if !ok {
            self.dirty = true;
            self.last = Some(now);
            self.retry_at = Some(now + RETRY);
        }
    }

    pub fn is_pending(&self) -> bool {
        self.dirty || self.running
    }
}

/// 一条任务的后台消息：要么是系统的变化通知，要么是补救扫描的结论。
enum Notice {
    Event(notify::Result<notify::Event>),
    CatchUp(Result<bool, String>),
}

/// 一拍里攒出来的结果。
#[derive(Debug, Default)]
pub struct Tick {
    /// 该跑的任务 id。
    pub ready: Vec<String>,
    /// 盯不住的任务与原因，每条只报一次。
    pub problems: Vec<(String, String)>,
}

struct Live {
    root: PathBuf,
    /// 上一次同步的时间（RFC3339），补救扫描拿它比最新修改时间。
    since: String,
    /// `_watcher` 必须活着：它一被丢掉，系统的监视就停了。
    _watcher: RecommendedWatcher,
    pending: Pending,
}

/// 所有正在被盯着的文件夹。
pub struct Watchers {
    tx: Sender<(String, Notice)>,
    rx: Receiver<(String, Notice)>,
    live: HashMap<String, Live>,
}

impl Default for Watchers {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            live: HashMap::new(),
        }
    }
}

impl Watchers {
    /// 按当前任务表接管/放手监视器。返回盯不住的原因（任务 id, 原因）。
    ///
    /// 每一拍都会调用；路径没变的任务不会重建监视器，所以它只做一次表比对，不碰磁盘内容。
    pub fn sync(&mut self, tasks: &[SyncTask]) -> Vec<(String, String)> {
        let mut wanted: HashMap<&str, PathBuf> = HashMap::new();
        for task in tasks.iter().filter(|t| watches(t)) {
            wanted.insert(task.id.as_str(), root_of(task));
        }
        // 任务被删、被停用、改了方向，或者本地目录换了位置，旧监视器都不该留着。
        self.live.retain(|id, live| match wanted.get(id.as_str()) {
            Some(root) => live.root == *root,
            None => false,
        });

        let mut problems = Vec::new();
        for task in tasks.iter().filter(|t| watches(t)) {
            if self.live.contains_key(&task.id) {
                continue;
            }
            match self.spawn(task) {
                Ok(live) => {
                    self.live.insert(task.id.clone(), live);
                }
                Err(why) => problems.push((task.id.clone(), why)),
            }
        }
        problems
    }

    fn spawn(&self, task: &SyncTask) -> Result<Live, String> {
        let root = root_of(task);
        let root = root.as_path();
        if !root.is_dir() {
            return Err(format!("{} 现在不是一个能打开的目录", root.display()));
        }
        let tx = self.tx.clone();
        let id = task.id.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let _ = tx.send((id.clone(), Notice::Event(res)));
        })
        .map_err(|e| format!("这台机器上没法监视文件变化：{e}"))?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| format!("盯不住 {}：{e}", root.display()))?;
        // 通知只从这一刻开始，程序关着的那段时间不会补报，所以接手时补一次
        // 「最新修改时间是否晚于上次同步」。这一步在后台线程里，不在主循环。
        self.catch_up(&task.id, root, &task.last_run);
        Ok(Live {
            root: root.to_path_buf(),
            since: task.last_run.clone(),
            _watcher: watcher,
            pending: Pending::default(),
        })
    }

    fn catch_up(&self, id: &str, root: &Path, since: &str) {
        let tx = self.tx.clone();
        let id = id.to_string();
        let root = root.to_path_buf();
        let since = since.to_string();
        std::thread::spawn(move || {
            let out = match newest_under(&root) {
                Ok(newest) => Ok(changed_since(newest, &since)),
                Err(why) => Err(why),
            };
            let _ = tx.send((id, Notice::CatchUp(out)));
        });
    }

    /// 收一拍事件，返回该跑的任务和该说的问题。不阻塞，不扫盘。
    pub fn pump(&mut self, now: Instant) -> Tick {
        let mut out = Tick::default();
        let mut broken: Vec<String> = Vec::new();
        let mut rescan: Vec<String> = Vec::new();

        while let Ok((id, notice)) = self.rx.try_recv() {
            let Some(live) = self.live.get_mut(&id) else {
                // 已经不盯这条任务了，迟到的事件直接丢掉。
                continue;
            };
            match notice {
                Notice::CatchUp(found) => match found {
                    Ok(true) => live.pending.note_change(now),
                    Ok(false) => {}
                    Err(why) => {
                        if live.pending.note_problem(why.clone()) {
                            out.problems.push((id.clone(), why));
                        }
                    }
                },
                Notice::Event(event) => match event {
                    Ok(event) => {
                        live.pending.note_change(now);
                        // 按引用判断：`EventKind` 带数据，取走一次后面就用不了了。
                        let root_gone = matches!(&event.kind, EventKind::Remove(_))
                            && event.paths.iter().any(|p| p == &live.root);
                        if root_gone {
                            // 目录没了（可能是改名或删除后重建）：监视器已经跟着失效，
                            // 下一拍重新接手，接手时的补救扫描会补上漏掉的变化。
                            broken.push(id.clone());
                        } else if matches!(&event.kind, EventKind::Any | EventKind::Other) {
                            // 后端明确说「我不保证没漏」，就补一次。
                            rescan.push(id.clone());
                        }
                    }
                    Err(e) => {
                        if live.pending.note_problem(format!("文件变化通知中断：{e}")) {
                            out.problems
                                .push((id.clone(), format!("文件变化通知中断：{e}")));
                        }
                        rescan.push(id.clone());
                    }
                },
            }
        }

        for id in broken {
            self.live.remove(&id);
        }
        // 目录被删掉或被整个改名时，系统不保证只报一条「根目录没了」。
        // 直接看一眼根还在不在，比猜事件类型可靠；不在了就放手，下一拍由 sync() 报出来。
        self.live.retain(|_, live| live.root.is_dir());
        for id in rescan {
            if let Some(live) = self.live.get(&id) {
                self.catch_up(&id, &live.root, &live.since);
            }
        }

        for (id, live) in self.live.iter_mut() {
            if live.pending.take_ready(now) {
                out.ready.push(id.clone());
            }
        }
        out.ready.sort();
        out
    }

    /// 任务真的开始跑了（或者没能开始）。没能开始时，只有"这次是变化欠下的一轮"才留着重试：
    /// 按间隔到期的那一轮失败了不该变成每分钟重试一次。
    pub fn note_started(&mut self, id: &str, started: bool, now: Instant) {
        if let Some(live) = self.live.get_mut(id) {
            if !started && live.pending.is_pending() {
                live.pending.note_round_finished(false, now);
            }
        }
    }

    /// 一轮结束。`ok` 为假时会把这次变化留着重试。
    pub fn note_round_finished(&mut self, id: &str, ok: bool, now: Instant) {
        if let Some(live) = self.live.get_mut(id) {
            live.pending.note_round_finished(ok, now);
        }
    }
}

fn root_of(task: &SyncTask) -> PathBuf {
    PathBuf::from(task.local.trim())
}

/// 这棵树里最新的修改时间。
///
/// 只回答一个问题，不用整棵树当指纹。它在单独的线程里跑，主循环不调用。
/// 超过上限时返回错误而不是悄悄跳过：跳过的意思是「看着像在监控，其实没监控」。
pub fn newest_under(root: &Path) -> Result<Option<SystemTime>, String> {
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut newest: Option<SystemTime> = None;
    let mut seen: u64 = 0;
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(format!(
                "目录层数超过 {MAX_DEPTH} 层，没法可靠判断有没有变化"
            ));
        }
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let kind = entry
                .file_type()
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
            // 符号链接可能指回上层，跟下去会绕圈。
            if kind.is_symlink() {
                continue;
            }
            seen += 1;
            if seen > MAX_ENTRIES {
                return Err(format!("文件夹里超过 {MAX_ENTRIES} 项，不再自动判断变化"));
            }
            if kind.is_dir() {
                stack.push((entry.path(), depth + 1));
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    newest = Some(match newest {
                        Some(prev) => prev.max(modified),
                        None => modified,
                    });
                }
            }
        }
    }
    Ok(newest)
}

fn unix_ms(at: SystemTime) -> Option<i64> {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// 上次同步之后有没有动过。
///
/// `last_run` 读不懂时按「动过」处理：多跑一轮只是多确认一次，漏一轮才是用户看到的
/// 「我改了它没同步」；而且只复制模式本来就不删东西。
pub fn changed_since(newest: Option<SystemTime>, last_run: &str) -> bool {
    let Some(mtime) = newest.and_then(unix_ms) else {
        return false;
    };
    match chrono::DateTime::parse_from_rfc3339(last_run.trim()) {
        Ok(last) => mtime > last.timestamp_millis(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_tasks::{DIR_DOWN, DIR_UP, MODE_SYNC};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rg-watch-{}-{n}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write(dir: &Path, name: &str, body: &[u8]) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("建父目录");
        }
        std::fs::write(&path, body).expect("写文件");
    }

    fn base_task(direction: &str, enabled: bool, auto_sync: Option<bool>) -> SyncTask {
        SyncTask {
            id: "t1".to_string(),
            name: "照片".to_string(),
            local: "D:\\Photos".to_string(),
            remote: "gdrive:backup/photos".to_string(),
            direction: direction.to_string(),
            mode: crate::sync_tasks::MODE_COPY.to_string(),
            enabled,
            auto_sync,
            interval_hours: 6,
            last_run: String::new(),
            last_status: String::new(),
        }
    }

    /// 等到 `cond` 成立，最多等 10 秒。真实文件通知是异步的，只能这样等。
    fn until(mut cond: impl FnMut() -> bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    /// 走掉「刚接手」时那次补救扫描欠下的一轮。
    ///
    /// 测试里的任务 `last_run` 是空的，`changed_since` 必然回答「有变化」，
    /// 所以接手后第一轮一定会跑。不先把它过掉，下面「改动被看见」的断言就会
    /// 被这次补扫顶成通过，等于什么都没验。
    fn settle_initial_round(ws: &mut Watchers) {
        assert!(
            until(|| ws.pump(Instant::now()).ready.contains(&"t1".to_string())),
            "接手时应当补跑一轮"
        );
        ws.note_round_finished("t1", true, Instant::now());
        // 让静默窗口从这一刻重新起算，免得下一轮被上一轮的尾巴推着走。
        std::thread::sleep(Duration::from_millis(400));
    }

    #[test]
    fn a_quiet_folder_never_fires() {
        let mut p = Pending::default();
        let now = Instant::now();
        assert!(!p.take_ready(now));
        assert!(!p.take_ready(now + Duration::from_secs(600)));
    }

    #[test]
    fn one_change_fires_only_after_it_settles() {
        let mut p = Pending::default();
        let t0 = Instant::now();
        p.note_change(t0);
        assert!(
            !p.take_ready(t0 + Duration::from_millis(100)),
            "刚改完就动手会把一次编辑拆成两次同步"
        );
        assert!(p.take_ready(t0 + SETTLE));
        assert!(
            !p.take_ready(t0 + SETTLE + Duration::from_secs(1)),
            "同一轮不该被触发两次"
        );
    }

    #[test]
    fn more_changes_restart_the_wait() {
        let mut p = Pending::default();
        let t0 = Instant::now();
        p.note_change(t0);
        p.note_change(t0 + Duration::from_secs(2));
        assert!(!p.take_ready(t0 + SETTLE + Duration::from_millis(1)));
        assert!(p.take_ready(t0 + Duration::from_secs(2) + SETTLE));
    }

    #[test]
    fn many_events_still_fire_once() {
        let mut p = Pending::default();
        let t0 = Instant::now();
        for i in 0..200 {
            p.note_change(t0 + Duration::from_millis(i * 5));
        }
        let at = t0 + Duration::from_secs(1) + SETTLE;
        assert!(p.take_ready(at));
        assert!(!p.take_ready(at + Duration::from_secs(1)));
    }

    #[test]
    fn a_change_during_a_round_brings_another_round() {
        let mut p = Pending::default();
        let t0 = Instant::now();
        p.note_change(t0);
        assert!(p.take_ready(t0 + SETTLE));
        // 正在跑的时候用户又改了一次。
        let during = t0 + SETTLE + Duration::from_secs(1);
        p.note_change(during);
        assert!(
            !p.take_ready(during + Duration::from_millis(500)),
            "还在跑，不能叠着开第二份"
        );
        p.note_round_finished(true, during + Duration::from_secs(2));
        assert!(
            !p.take_ready(during + Duration::from_secs(2)),
            "刚跑完，静默窗口还没过"
        );
        assert!(
            p.take_ready(during + SETTLE + Duration::from_secs(1)),
            "跑的时候来的变化不能丢"
        );
    }

    #[test]
    fn a_failed_round_waits_before_retrying() {
        let mut p = Pending::default();
        let t0 = Instant::now();
        p.note_change(t0);
        assert!(p.take_ready(t0 + SETTLE));
        let failed = t0 + SETTLE + Duration::from_secs(1);
        p.note_round_finished(false, failed);
        assert!(
            !p.take_ready(failed + Duration::from_secs(1)),
            "失败之后紧跟着重试只会连着失败"
        );
        assert!(p.take_ready(failed + RETRY));
    }

    #[test]
    fn a_problem_is_reported_once() {
        let mut p = Pending::default();
        assert!(p.note_problem("目录打不开"));
        assert!(!p.note_problem("目录打不开"), "同样的问题说一遍就够");
        assert!(p.note_problem("另一个问题"));
    }

    #[test]
    fn only_enabled_auto_local_to_remote_tasks_are_watched() {
        assert!(watches(&base_task(DIR_UP, true, Some(true))));
        assert!(
            !watches(&base_task(DIR_DOWN, true, Some(true))),
            "反方向的本机变化是用户自己的编辑"
        );
        assert!(!watches(&base_task(DIR_UP, false, Some(true))));
        assert!(!watches(&base_task(DIR_UP, true, Some(false))));
    }

    #[test]
    fn a_mirror_task_is_not_watched_until_it_is_asked() {
        let mut task = base_task(DIR_UP, true, None);
        assert!(watches(&task), "只复制的任务默认跟着变化跑");
        task.mode = MODE_SYNC.to_string();
        assert!(!watches(&task), "镜像会删对端，默认不能由一次改名决定");
        task.auto_sync = Some(true);
        assert!(watches(&task), "用户明确打开了就按用户的");
    }

    #[test]
    fn newest_under_finds_the_latest_file_in_nested_folders() {
        let dir = temp_dir("newest");
        write(&dir, "a.txt", b"a");
        write(&dir, "deep/b.txt", b"b");
        let newest = newest_under(&dir).expect("扫得动").expect("有文件");
        let a = std::fs::metadata(dir.join("a.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(newest >= a);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newest_under_reports_a_missing_folder() {
        let dir = temp_dir("missing").join("没有这个目录");
        assert!(
            newest_under(&dir).is_err(),
            "打不开要说出来，不能当成没变化"
        );
    }

    #[test]
    fn changed_since_compares_against_the_last_run() {
        let dir = temp_dir("since");
        write(&dir, "a.txt", b"a");
        let newest = newest_under(&dir).expect("扫得动");
        assert!(changed_since(newest, ""), "从没跑过就算有变化");
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(
            !changed_since(newest, &future),
            "上次同步在一小时之后，说明没动过"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_same_size_edit_to_an_old_file_is_seen() {
        // 这一条正是旧做法漏掉的：文件数、总字节、最新时间都可能不变，
        // 只有系统通知能看见「这个文件被写了」。
        let dir = temp_dir("same-size");
        write(&dir, "new.txt", b"0123456789");
        std::thread::sleep(Duration::from_millis(50));
        write(&dir, "old.txt", b"AAAAAAAAAA");
        let mut ws = Watchers::default();
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.to_string_lossy().to_string();
        assert!(ws.sync(&[task]).is_empty(), "监视器该建起来");
        settle_initial_round(&mut ws);

        // 同尺寸改写那个不是最新的文件。
        write(&dir, "old.txt", b"BBBBBBBBBB");
        assert!(
            until(|| ws.pump(Instant::now()).ready.contains(&"t1".to_string())),
            "同尺寸改写旧文件也必须算变化"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rename_is_seen() {
        let dir = temp_dir("rename");
        write(&dir, "before.txt", b"x");
        let mut ws = Watchers::default();
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.to_string_lossy().to_string();
        assert!(ws.sync(&[task]).is_empty(), "监视器该建起来");
        settle_initial_round(&mut ws);

        std::fs::rename(dir.join("before.txt"), dir.join("after.txt")).expect("改名");
        assert!(
            until(|| ws.pump(Instant::now()).ready.contains(&"t1".to_string())),
            "改名要算变化"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_folder_that_disappears_is_picked_up_again_when_it_comes_back() {
        let dir = temp_dir("recreate");
        write(&dir, "a.txt", b"x");
        let mut ws = Watchers::default();
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.to_string_lossy().to_string();
        ws.sync(&[task]);
        std::thread::sleep(Duration::from_millis(300));

        std::fs::remove_dir_all(&dir).expect("整个目录删掉");
        assert!(
            until(|| {
                ws.pump(Instant::now());
                !ws.live.contains_key("t1")
            }),
            "目录没了，监视器要摘掉，不能假装还在盯"
        );
        assert!(!ws.sync(&[task.clone()]).is_empty(), "目录不在了，要报出来");
        // 删掉再建回来（比如用户先把文件夹挪走整理一阵）。
        std::fs::create_dir_all(&dir).expect("重建目录");
        assert!(
            until(|| ws.sync(&[task.clone()]).is_empty()),
            "目录回来了就重新接手"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn watchers_let_go_of_tasks_that_are_gone() {
        let dir = temp_dir("retain");
        write(&dir, "a.txt", b"x");
        let mut ws = Watchers::default();
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.to_string_lossy().to_string();
        ws.sync(&[task.clone()]);
        assert!(ws.live.contains_key("t1"));
        assert!(ws.sync(&[]).is_empty());
        assert!(!ws.live.contains_key("t1"), "任务没了，监视器也该放手");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dropping_the_watchers_stops_delivery() {
        let dir = temp_dir("drop");
        write(&dir, "a.txt", b"x");
        let mut ws = Watchers::default();
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.to_string_lossy().to_string();
        ws.sync(&[task]);
        std::thread::sleep(Duration::from_millis(300));
        drop(ws);

        // 关掉之后再改，也不该有任何东西被触发；这里能验证的只是「丢掉不炸」，
        // 监视线程随句柄一起走掉。
        write(&dir, "a.txt", b"y");
        std::thread::sleep(Duration::from_millis(100));
        std::fs::remove_dir_all(&dir).ok();
    }
}
