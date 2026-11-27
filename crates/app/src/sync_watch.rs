//! 本机文件夹一变，就让同步提前跑一轮。
//!
//! 变化来自系统的文件通知（Windows 上是 `ReadDirectoryChangesW`，由 `notify` 包成同一套接口）。
//! 主循环里没有递归扫描：那是拿界面线程去读整块磁盘。
//!
//! 通知只从装上的那一刻开始，之前发生过什么它不知道。所以「刚接手」「通知漏了」「目录被删掉
//! 又重建」这些情况下，本模块不去猜有没有变化，而是直接记一条「要求跑一轮」——被改名的、
//! 关着程序时删掉的、复制时保留了时间戳的，都没法从「最新修改时间」看回来，猜错一次的代价是
//! 那次同步永远不会发生。跑的这一轮就是任务自己的 mode：只复制的任务不会删任何东西，镜像任务
//! 还要先过 [`refuses_an_empty_mirror`] 那一关。
//!
//! 真正执行同步仍然交给 [`crate::sync_tasks`]，任务的 mode 和 direction 一个字都不改。有两条底线：
//!
//! - 只顺手「本地 → 远程」的任务。反方向时本机文件的变化正是用户自己的编辑，拿它触发同步
//!   就是把编辑覆盖掉（见 [`watches`]）。
//! - 镜像模式（`sync`，会删对端）默认不跟着变化跑。事件误判一次就可能删一大片，那一步得用户
//!   自己打开，不该由一次改名决定。
//!
//! 「有变化待同步」是状态，不是一次性判断。有三处特别容易把它丢掉，这里都是显式处理的：
//!
//! - 同一拍里既到期又有事件，引擎已经在跑：那一轮结束后要接着跑一次，不能当作已经同步过。
//! - 通知队列满了：事件被丢掉，等于这段时间的变化不可知，记一条「要求跑一轮」。
//! - 目录消失、监视器被撤掉：待处理状态留着，重建监视器之后再跑一次。

use crate::config::SyncTask;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 最后一条变化之后还要安静多久才动手。一次保存常常连着冒出好几条事件，
/// 立刻跑会把一次编辑拆成好几次同步。
pub const SETTLE: Duration = Duration::from_secs(3);

/// 一轮失败之后隔多久再试。失败原因多半是网络或对端拒绝，紧跟着重试只会连着失败。
pub const RETRY: Duration = Duration::from_secs(60);

/// 通知队列最多积多少条。文件风暴时队列满掉是正常的，满了就记成「漏了事件」，
/// 反正后面也是跑一轮；无界队列则会把内存吃光。
pub const QUEUE: usize = 4096;

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

/// 变化触发的镜像任务，本机目录变成空的就不动手。
///
/// 盘符没挂上、网络盘掉线、目录被改名之后原地又建了个空的——这几种情况下系统
/// 通知告诉我们的是「里面的东西全没了」。镜像任务照做就是把云端整棵树删掉，
/// 一次误读的代价太大，所以这里只报告、不执行；用户确实清空了本机、想把删除
/// 同步下去，可以在那一行上按「运行」。
///
/// 只读一遍目录的第一项，不递归：这个判断每轮都要做，不能变成一次扫盘。
pub fn refuses_an_empty_mirror(task: &SyncTask) -> bool {
    if !crate::sync_tasks::is_mirror(task) {
        return false;
    }
    match std::fs::read_dir(root_of(task)) {
        Ok(mut entries) => entries.next().is_none(),
        // 打不开的情况由监视那边报「目录不在了」，这里不重复下结论。
        Err(_) => false,
    }
}

/// 一条任务「有没有变化等着同步」的状态。
///
/// 单独拆出来是为了能直接测：这些判断一旦错了，症状是「用户改了文件，同步没动」，
/// 而那在界面上看不出来。
#[derive(Debug, Default)]
pub struct Pending {
    dirty: bool,
    /// 最近一次「值得跑一轮」的时刻，用来算静默窗口。
    seen: Option<Instant>,
    running: bool,
    retry_at: Option<Instant>,
    problem: Option<String>,
}

impl Pending {
    /// 收到一次变化。事件抖动不用在这里去重：静默窗口本来就会把一串事件合成一轮。
    pub fn note_change(&mut self, now: Instant) {
        self.dirty = true;
        self.seen = Some(now);
    }

    /// 记一条「这一段历史不可知，跑一轮确认」：刚接手、通知漏了、目录重建。
    pub fn expect_a_round(&mut self, now: Instant) {
        self.dirty = true;
        self.seen = Some(now);
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
        if let Some(seen) = self.seen {
            if now.saturating_duration_since(seen) < SETTLE {
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

    /// 这一轮没能真的开始（引擎在忙、内核没连上、镜像空目录被挡下）。
    ///
    /// 把这次愿望还回去：`true` 表示稍后自动重试，`false` 表示等下一次变化再说——
    /// 空目录那种情况如果每分钟重试一次，就变成了对用户的持续打扰。
    pub fn note_not_started(&mut self, retry: bool, now: Instant) {
        self.running = false;
        self.dirty = true;
        self.retry_at = if retry { Some(now + RETRY) } else { None };
    }

    /// 一轮结束。跑的时候来的变化留在 `dirty` 上，静默窗口过后会自己再起一轮。
    pub fn note_round_finished(&mut self, ok: bool, now: Instant) {
        self.running = false;
        if !ok {
            self.dirty = true;
            self.seen = Some(now);
            self.retry_at = Some(now + RETRY);
        }
    }

    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.dirty || self.running
    }
}

/// 监视器发回来的消息。
///
/// 带上监视器的代次：目录消失又重建之后代次会变，旧监视器迟到的消息不能算到新目录头上。
enum Notice {
    Event {
        gen: u64,
        id: String,
    },
    /// 通知层自己说漏了（缓冲区溢出、后端出错）。
    Missed {
        gen: u64,
        id: String,
    },
    /// 盯着的那个目录不见了。
    Gone {
        gen: u64,
        id: String,
    },
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
    gen: u64,
    root: PathBuf,
    /// 队列满过。回调里只能置个位，具体怎么处理留给主循环。
    flooded: Arc<AtomicBool>,
    /// `_watcher` 必须活着：它一被丢掉，系统的监视就停了。
    _watcher: RecommendedWatcher,
}

/// 所有正在被盯着的文件夹。
pub struct Watchers {
    tx: SyncSender<Notice>,
    rx: Receiver<Notice>,
    gen: u64,
    live: HashMap<String, Live>,
    /// 待处理状态单独存：目录不见了就把监视器撤掉，但欠着的那一轮不能跟着丢。
    pending: HashMap<String, Pending>,
}

impl Default for Watchers {
    fn default() -> Self {
        let (tx, rx) = sync_channel(QUEUE);
        Self {
            tx,
            rx,
            gen: 0,
            live: HashMap::new(),
            pending: HashMap::new(),
        }
    }
}

impl Watchers {
    /// 按当前任务表接管/放手监视器。返回盯不住的原因（任务 id, 原因）。
    ///
    /// 每一拍都会调用；路径没变的任务不会重建监视器，所以它只做一次表比对，不碰目录内容。
    pub fn sync(&mut self, tasks: &[SyncTask], now: Instant) -> Vec<(String, String)> {
        let mut wanted: HashMap<String, PathBuf> = HashMap::new();
        for task in tasks.iter().filter(|t| watches(t)) {
            wanted.insert(task.id.clone(), root_of(task));
        }
        // 任务被删、被停用、改了方向：待处理状态和监视器都不该留着。
        self.pending.retain(|id, _| wanted.contains_key(id));
        // 本地目录换了位置，旧监视器盯着的是别人家，撤掉重建。
        self.live.retain(|id, live| match wanted.get(id) {
            Some(root) => live.root == *root,
            None => false,
        });

        let mut problems = Vec::new();
        for (id, root) in wanted {
            if self.live.contains_key(&id) {
                continue;
            }
            let gen = self.next_gen();
            match self.attach(&id, root, gen) {
                Ok(live) => {
                    self.live.insert(id.clone(), live);
                    // 刚接手：接手之前发生过什么没人知道，跑一轮确认。
                    self.pending.entry(id).or_default().expect_a_round(now);
                }
                Err(why) => {
                    let fresh = self
                        .pending
                        .entry(id.clone())
                        .or_default()
                        .note_problem(why.clone());
                    if fresh {
                        problems.push((id, why));
                    }
                }
            }
        }
        problems
    }

    fn next_gen(&mut self) -> u64 {
        self.gen += 1;
        self.gen
    }

    fn attach(&self, id: &str, root: PathBuf, gen: u64) -> Result<Live, String> {
        if !root.is_dir() {
            return Err(format!("{} 现在不是一个能打开的目录", root.display()));
        }
        let tx = self.tx.clone();
        let flooded = Arc::new(AtomicBool::new(false));
        let flag = flooded.clone();
        let name = id.to_string();
        let watched = root.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let note = match res {
                Ok(event) => {
                    // 按引用判断：`EventKind` 带数据，取走一次后面就用不了了。
                    let gone = matches!(&event.kind, EventKind::Remove(_))
                        && event.paths.iter().any(|p| *p == watched);
                    if gone {
                        Notice::Gone {
                            gen,
                            id: name.clone(),
                        }
                    } else {
                        Notice::Event {
                            gen,
                            id: name.clone(),
                        }
                    }
                }
                Err(_) => Notice::Missed {
                    gen,
                    id: name.clone(),
                },
            };
            // 队列满了不能再等：把「漏了事件」记在这条任务的标志位上，主循环会看到。
            if let Err(TrySendError::Full(_)) = tx.try_send(note) {
                flag.store(true, Ordering::SeqCst);
            }
        })
        .map_err(|e| format!("这台机器上没法监视文件变化：{e}"))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| format!("盯不住 {}：{e}", root.display()))?;
        Ok(Live {
            gen,
            root,
            flooded,
            _watcher: watcher,
        })
    }

    /// 收一拍事件，返回该跑的任务和该说的问题。不阻塞，不扫盘。
    pub fn pump(&mut self, now: Instant) -> Tick {
        let mut out = Tick::default();

        while let Ok(note) = self.rx.try_recv() {
            let (gen, id) = match &note {
                Notice::Event { gen, id }
                | Notice::Missed { gen, id }
                | Notice::Gone { gen, id } => (*gen, id.clone()),
            };
            // 旧监视器的话：它盯的可能是已经被换掉的那个路径，不能算数。
            // 用 matches! 而不是拿着 `live` 往下走，免得后面的 `&mut self` 撞上借用。
            if !matches!(self.live.get(&id), Some(live) if live.gen == gen) {
                continue;
            }
            match note {
                Notice::Event { .. } => {
                    if let Some(p) = self.pending.get_mut(&id) {
                        p.note_change(now);
                    }
                }
                Notice::Missed { .. } => {
                    let why = "文件变化通知漏掉了事件".to_string();
                    self.expect_round(&id, why, now, &mut out);
                }
                Notice::Gone { .. } => {
                    // 监视器跟着失效：撤掉，下一拍 sync() 重建，待处理状态留着。
                    let Some(live) = self.live.remove(&id) else {
                        continue;
                    };
                    let why = format!("{} 现在不见了，等它回来再重新盯", live.root.display());
                    self.expect_round(&id, why, now, &mut out);
                }
            }
        }

        // 队列满过的任务：事件被丢掉了，这段时间的变化不可知。
        let flooded: Vec<String> = self
            .live
            .iter()
            .filter(|(_, live)| live.flooded.load(Ordering::SeqCst))
            .map(|(id, _)| id.clone())
            .collect();
        for id in flooded {
            if let Some(live) = self.live.get(&id) {
                live.flooded.store(false, Ordering::SeqCst);
            }
            let why = "变化太多，通知队列满了".to_string();
            self.expect_round(&id, why, now, &mut out);
        }

        // 目录被整个删掉或改名时，系统不保证只报一条「根目录没了」，偶尔一条都不报。
        // 直接看一眼根还在不在，比猜事件类型可靠；不在了就放手，下一拍由 sync() 重建。
        let vanished: Vec<String> = self
            .live
            .iter()
            .filter(|(_, live)| !live.root.is_dir())
            .map(|(id, _)| id.clone())
            .collect();
        for id in vanished {
            let Some(live) = self.live.remove(&id) else {
                continue;
            };
            // 不能悄悄放手：用户的目录不见了（拔了移动硬盘、改了盘符）得看出来。
            let why = format!("{} 现在不见了，等它回来再重新盯", live.root.display());
            self.expect_round(&id, why, now, &mut out);
        }

        for (id, pending) in self.pending.iter_mut() {
            if self.live.contains_key(id) && pending.take_ready(now) {
                out.ready.push(id.clone());
            }
        }
        out.ready.sort();
        out
    }

    fn expect_round(&mut self, id: &str, why: String, now: Instant, out: &mut Tick) {
        if let Some(p) = self.pending.get_mut(id) {
            p.expect_a_round(now);
            if p.note_problem(why.clone()) {
                out.problems.push((id.to_string(), why));
            }
        }
    }

    /// 这一轮没能真的开始。必须把愿望还回去，否则这次变化就永远丢了。
    ///
    /// `retry` 为假是「等下一次变化」：引擎正忙或者本机目录还是空的，不是错误，
    /// 立刻重试只会空转。
    pub fn note_not_started(&mut self, id: &str, retry: bool, now: Instant) {
        if let Some(pending) = self.pending.get_mut(id) {
            pending.note_not_started(retry, now);
        }
    }

    /// 一轮结束。`ok` 为假时会把这次变化留着重试。
    pub fn note_round_finished(&mut self, id: &str, ok: bool, now: Instant) {
        if let Some(pending) = self.pending.get_mut(id) {
            pending.note_round_finished(ok, now);
        }
    }

    #[cfg(test)]
    fn is_pending(&self, id: &str) -> bool {
        self.pending
            .get(id)
            .map(Pending::is_pending)
            .unwrap_or(false)
    }

    #[cfg(test)]
    fn wants_round(&self, id: &str) -> bool {
        self.pending.get(id).map(|p| p.dirty).unwrap_or(false)
    }

    #[cfg(test)]
    fn problem_of(&self, id: &str) -> Option<String> {
        self.pending.get(id).and_then(|p| p.problem.clone())
    }

    #[cfg(test)]
    fn gen_of(&self, id: &str) -> Option<u64> {
        self.live.get(id).map(|live| live.gen)
    }
}

fn root_of(task: &SyncTask) -> PathBuf {
    PathBuf::from(task.local.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_tasks::{DIR_DOWN, DIR_UP, MODE_COPY, MODE_SYNC};
    use std::path::Path;
    use std::sync::atomic::AtomicU64;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rg-watch-{}-{n}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write(dir: &Path, name: &str, body: &[u8]) {
        std::fs::write(dir.join(name), body).expect("写文件");
    }

    fn base_task(direction: &str, enabled: bool, auto_sync: Option<bool>) -> SyncTask {
        SyncTask {
            id: "t1".to_string(),
            name: "照片".to_string(),
            local: "D:\\Photos".to_string(),
            remote: "gdrive:backup/photos".to_string(),
            direction: direction.to_string(),
            mode: MODE_COPY.to_string(),
            enabled,
            auto_sync,
            interval_hours: 6,
            last_run: String::new(),
            last_status: String::new(),
        }
    }

    /// 静默窗口过后的时刻。测试不用真的等三秒。
    fn later(base: Instant) -> Instant {
        base + SETTLE + Duration::from_millis(10)
    }

    /// 等一条真事件到达并过了静默窗口。文件通知是异步的，只能这样等。
    fn wait_ready(ws: &mut Watchers, ms: u64) -> Vec<String> {
        let rounds = (ms / 20).max(1);
        for _ in 0..rounds {
            std::thread::sleep(Duration::from_millis(20));
            let tick = ws.pump(Instant::now() + SETTLE);
            if !tick.ready.is_empty() {
                return tick.ready;
            }
        }
        Vec::new()
    }

    #[test]
    fn a_quiet_folder_runs_one_round_after_takeover_then_stays_quiet() {
        let dir = temp_dir("quiet");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        assert!(ws.sync(&[task], t0).is_empty(), "目录在，监视器该建起来");
        // 接手之前发生过什么没人知道，所以先跑一轮确认。
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);
        ws.note_round_finished("t1", true, later(t0));
        assert!(
            ws.pump(later(t0)).ready.is_empty(),
            "什么都不动就不该再来一轮"
        );
    }

    #[test]
    fn one_change_fires_after_a_quiet_moment() {
        let dir = temp_dir("change");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        ws.pump(later(t0));
        ws.note_round_finished("t1", true, later(t0));

        write(&dir, "a.txt", b"a");
        assert_eq!(
            wait_ready(&mut ws, 5_000),
            vec!["t1".to_string()],
            "改了文件就该来一轮"
        );
        ws.note_round_finished("t1", true, Instant::now());
    }

    #[test]
    fn many_events_still_make_one_round() {
        let dir = temp_dir("burst");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        ws.pump(later(t0));
        ws.note_round_finished("t1", true, later(t0));

        for i in 0..20 {
            write(&dir, &format!("f{i}.txt"), b"x");
        }
        let ready = wait_ready(&mut ws, 5_000);
        assert_eq!(ready, vec!["t1".to_string()], "一串事件只该合成一轮");
        ws.note_round_finished("t1", true, Instant::now());
    }

    #[test]
    fn a_change_during_a_running_round_brings_another_round() {
        let dir = temp_dir("during");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);

        // 这一轮还在跑，文件又变了。
        write(&dir, "a.txt", b"a");
        let mut seen = false;
        for _ in 0..250 {
            std::thread::sleep(Duration::from_millis(20));
            ws.pump(Instant::now() + SETTLE);
            if ws.wants_round("t1") {
                seen = true;
                break;
            }
        }
        assert!(seen, "跑的时候变的东西也要记下来");
        ws.note_round_finished("t1", true, Instant::now());
        assert_eq!(
            wait_ready(&mut ws, 5_000),
            vec!["t1".to_string()],
            "这一轮结束后要补跑一次"
        );
    }

    #[test]
    fn a_round_that_could_not_start_keeps_the_wish() {
        let dir = temp_dir("not-started");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);

        // 引擎正在跑别的：这一轮没能开始，愿望必须还回来。
        ws.note_not_started("t1", false, later(t0));
        assert!(ws.wants_round("t1"), "没开始的这一轮不能算同步过了");
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);

        // 连着被挡几次也不能丢。
        ws.note_not_started("t1", false, later(t0));
        ws.note_not_started("t1", false, later(t0));
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);
    }

    #[test]
    fn a_blocked_empty_mirror_folder_keeps_the_wish_and_recovers() {
        let dir = temp_dir("mirror-blocked");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        task.mode = MODE_SYNC.to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task.clone()], t0);
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);

        // 空目录被挡下：这一轮不跑，但愿望留着。
        assert!(refuses_an_empty_mirror(&task));
        ws.note_not_started("t1", false, later(t0));
        assert!(ws.wants_round("t1"), "被挡下的那一轮不能当作已经同步过");

        // 里面有东西了，下一次变化照常跑。
        write(&dir, "a.txt", b"a");
        assert!(!refuses_an_empty_mirror(&task));
        assert_eq!(
            wait_ready(&mut ws, 5_000),
            vec!["t1".to_string()],
            "空目录保护过后要能自己恢复"
        );
    }

    #[test]
    fn a_failed_round_waits_before_retrying() {
        let dir = temp_dir("retry");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);

        let failed_at = later(t0);
        ws.note_round_finished("t1", false, failed_at);
        assert!(
            ws.pump(failed_at + Duration::from_secs(5)).ready.is_empty(),
            "刚失败过就别马上重试"
        );
        assert_eq!(
            ws.pump(failed_at + RETRY + Duration::from_secs(1)).ready,
            vec!["t1".to_string()],
            "歇够了要自己再试一次"
        );
    }

    #[test]
    fn late_messages_from_an_older_watcher_are_ignored() {
        let first = temp_dir("gen-a");
        let second = temp_dir("gen-b");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = first.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task.clone()], t0);
        ws.pump(later(t0));
        ws.note_round_finished("t1", true, later(t0));
        let old = ws.gen_of("t1").expect("第一代监视器");

        // 用户把本地目录改到了别处：旧监视器撤掉，换新的。
        task.local = second.display().to_string();
        ws.sync(&[task.clone()], t0);
        assert_ne!(ws.gen_of("t1"), Some(old), "换代了");
        ws.pump(later(t0));
        ws.note_round_finished("t1", true, later(t0));

        // 旧监视器迟到的一条消息：不能算到新目录头上。
        ws.tx
            .try_send(Notice::Event {
                gen: old,
                id: "t1".to_string(),
            })
            .expect("队列没满");
        ws.pump(later(t0));
        assert!(!ws.wants_round("t1"), "旧监视器的消息要丢掉");

        // 新一代的消息照常算。
        let now_gen = ws.gen_of("t1").expect("第二代监视器");
        ws.tx
            .try_send(Notice::Event {
                gen: now_gen,
                id: "t1".to_string(),
            })
            .expect("队列没满");
        ws.pump(later(t0));
        assert!(ws.wants_round("t1"));
    }

    #[test]
    fn a_flooded_queue_leaves_one_round_and_a_notice() {
        let dir = temp_dir("flood");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        ws.pump(later(t0));
        ws.note_round_finished("t1", true, later(t0));

        // 回调发现队列满了只能置个位，主循环看到了就当作「漏了事件」。
        let now_gen = ws.gen_of("t1").expect("监视器");
        for _ in 0..QUEUE {
            let _ = ws.tx.try_send(Notice::Event {
                gen: now_gen,
                id: "t1".to_string(),
            });
        }
        ws.live
            .get("t1")
            .expect("监视器")
            .flooded
            .store(true, Ordering::SeqCst);

        let tick = ws.pump(later(t0));
        assert_eq!(tick.ready, vec!["t1".to_string()], "满一次只该补一轮");
        assert_eq!(tick.problems.len(), 1, "只报一次");
        assert!(tick.problems[0].1.contains("队列满"));
        // 队列清空了，标志也复位了：再 pump 不该重复报告。
        let again = ws.pump(later(t0));
        assert!(again.problems.is_empty(), "同一个问题只说一次");
    }

    #[test]
    fn pending_survives_the_root_disappearing() {
        let dir = temp_dir("vanish");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task.clone()], t0);
        ws.pump(later(t0));
        ws.note_round_finished("t1", true, later(t0));

        std::fs::remove_dir_all(&dir).expect("删掉目录");
        // 目录没了：由 pump 发现并报出来（系统不保证发事件），但欠着的那一轮不能跟着丢。
        let tick = ws.pump(later(t0));
        assert_eq!(tick.problems.len(), 1, "目录不在了要报");
        assert!(ws.is_pending("t1"), "待处理状态要留着");

        // 目录回来了：重新接手，并且再确认一轮。
        std::fs::create_dir_all(&dir).expect("重建目录");
        assert!(ws.sync(&[task], later(t0)).is_empty());
        assert_eq!(
            ws.pump(later(t0)).ready,
            vec!["t1".to_string()],
            "目录回来后要补一轮"
        );
    }

    #[test]
    fn only_enabled_auto_local_to_remote_tasks_are_watched() {
        let up = base_task(DIR_UP, true, Some(true));
        assert!(watches(&up));
        let down = base_task(DIR_DOWN, true, Some(true));
        assert!(!watches(&down), "云端 → 本机不是我们能触发的那一头");
        let off = base_task(DIR_UP, false, Some(true));
        assert!(!watches(&off));
        let unset = base_task(DIR_UP, true, None);
        assert!(watches(&unset), "没选过时只复制的任务默认盯");
    }

    #[test]
    fn a_mirror_task_is_not_watched_until_it_is_asked() {
        let mut mirror = base_task(DIR_UP, true, None);
        mirror.mode = MODE_SYNC.to_string();
        assert!(!watches(&mirror), "镜像任务默认不跟着变化跑");

        let mut asked = mirror.clone();
        asked.auto_sync = Some(true);
        assert!(watches(&asked), "用户自己打开了就照办");

        let mut refused = base_task(DIR_UP, true, Some(false));
        refused.mode = MODE_SYNC.to_string();
        assert!(!watches(&refused));
    }

    #[test]
    fn an_empty_folder_does_not_drive_a_mirror_task() {
        let dir = temp_dir("empty-mirror");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        task.mode = MODE_SYNC.to_string();
        assert!(
            refuses_an_empty_mirror(&task),
            "空目录大概是盘没挂上，不是用户把文件删光了"
        );

        write(&dir, "a.txt", b"x");
        assert!(!refuses_an_empty_mirror(&task), "有东西了就照常跑");

        std::fs::remove_file(dir.join("a.txt")).expect("删掉文件");
        task.mode = MODE_COPY.to_string();
        assert!(
            !refuses_an_empty_mirror(&task),
            "只复制的任务不会删东西，空目录没有危险"
        );
    }

    #[test]
    fn a_same_size_edit_to_an_old_file_is_seen() {
        let dir = temp_dir("same-size");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        // 先有一个旧文件：它的修改时间比谁都早，按时间比较是看不见这次改动的。
        write(&dir, "old.txt", b"aaaa");
        write(&dir, "newer.txt", b"zzzz");
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        ws.pump(later(t0));
        ws.note_round_finished("t1", true, later(t0));

        write(&dir, "old.txt", b"bbbb");
        assert_eq!(
            wait_ready(&mut ws, 5_000),
            vec!["t1".to_string()],
            "同尺寸改写旧文件也要看得见"
        );
    }

    #[test]
    fn a_rename_is_seen() {
        let dir = temp_dir("rename");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        write(&dir, "a.txt", b"a");
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        ws.pump(later(t0));
        ws.note_round_finished("t1", true, later(t0));

        std::fs::rename(dir.join("a.txt"), dir.join("b.txt")).expect("改名");
        assert_eq!(
            wait_ready(&mut ws, 5_000),
            vec!["t1".to_string()],
            "改名也要看得见"
        );
    }

    #[test]
    fn watchers_let_go_of_tasks_that_are_gone() {
        let one = temp_dir("keep");
        let two = temp_dir("drop");
        let mut keep = base_task(DIR_UP, true, Some(true));
        keep.id = "keep".to_string();
        keep.local = one.display().to_string();
        let mut drop = base_task(DIR_UP, true, Some(true));
        drop.id = "drop".to_string();
        drop.local = two.display().to_string();

        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[keep.clone(), drop], t0);
        assert_eq!(ws.live.len(), 2);
        assert!(ws.is_pending("drop"));

        ws.sync(&[keep], t0);
        assert_eq!(ws.live.len(), 1, "被删掉的任务不该再盯着");
        assert!(!ws.is_pending("drop"), "被删掉的任务不用留状态");
        assert!(ws.problem_of("keep").is_none());
    }

    #[test]
    fn dropping_the_watchers_stops_delivery() {
        let dir = temp_dir("drop-all");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        ws.sync(&[task], Instant::now());
        drop(ws);
        // 监视器跟着走了：再改文件也不能把进程弄崩。
        write(&dir, "a.txt", b"a");
        std::thread::sleep(Duration::from_millis(200));
    }
}
