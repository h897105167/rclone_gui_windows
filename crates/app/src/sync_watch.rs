//! 本机文件夹一变，就让同步提前跑一轮。
//!
//! 变化来自系统的文件通知（Windows 上是 `ReadDirectoryChangesW`，由 `notify` 包成同一套接口）。
//! 主循环里没有递归扫描：那是拿界面线程去读整块磁盘。
//!
//! 通知只从装上的那一刻开始，之前发生过什么它不知道。所以「刚接手」「通知漏了」「目录被删掉
//! 又重建」这些情况下，本模块不去猜有没有变化，而是直接记一条「要求跑一轮」——被改名的、
//! 关着程序时删掉的、复制时保留了时间戳的，都没法从「最新修改时间」看回来，猜错一次的代价是
//! 那次同步永远不会发生。跑的这一轮就是任务自己的 mode：只复制的任务不会删任何东西，镜像任务
//! 还要先过 [`refuses_a_mirror_round`] 那一关。
//!
//! 真正执行同步仍然交给 [`crate::sync_tasks`]，任务的 mode 和 direction 一个字都不改。有两条底线：
//!
//! - 只顺手「本地 → 远程」的任务。反方向时本机文件的变化正是用户自己的编辑，拿它触发同步
//!   就是把编辑覆盖掉（见 [`watches`]）。
//! - 镜像模式（`sync`，会删对端）默认不跟着变化跑。事件误判一次就可能删一大片，那一步得用户
//!   自己打开，不该由一次改名决定。
//!
//! 「有变化待同步」是状态，不是一次性判断。有一条变化等在那里没跑成时，[`Pending`] 会记住它
//! 为什么没跑成，并且**分三种情况**处理——把三种合成一种就会出现「每一拍都重试一次」：
//!
//! - 引擎里已经有一轮在跑（同一拍里既到期又有事件）：欠着，等那一轮真的结束，之后只补一次。
//!   这期间既不自燃也不打扰用户。
//! - 本机目录空着或者读不了、又是镜像任务：欠着，但要等到下一次变化、目录回来，或者用户自己
//!   按「运行」才动。没人改动它就永远不动，否则就是让一次误读把云端清空。
//! - 没能启动（内核没连上、rclone 拒绝）：欠着，隔一段时间再试，间隔逐次拉长但有上限。
//!
//! 另外三处也容易把这条变化丢掉，同样是显式处理的：通知队列满了（事件被丢，等于这段时间不可知）、
//! 通知后端报错（撤掉监视器等重建）、目录消失（待处理状态留着，重建监视器之后再跑一次）。

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

/// 连续失败时间隔最长拉到这里。再长就等于放弃了，用户会觉得「它不干活了」；
/// 而一直按 [`RETRY`] 重试又会在对端长时间不可用时每分钟打一次。
pub const MAX_RETRY: Duration = Duration::from_secs(15 * 60);

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

/// 变化触发的镜像任务，本机目录空着或读不了就不动手。
///
/// 盘符没挂上、网络盘掉线、目录被改名之后原地又建了个空的、权限被收走——这几种情况下
/// 系统通知告诉我们的是「里面的东西全没了」，或者干脆读不出来。镜像任务照做就是把云端
/// 整棵树删掉，一次误读的代价太大，所以这里只报告、不执行；用户确实清空了本机、想把删除
/// 同步下去，可以在那一行上按「运行」。
///
/// 读不出来也算「不能动手」：把读失败当成空目录，和真的空目录后果一模一样。
///
/// 只读一遍目录的第一项，不递归：这个判断每轮都要做，不能变成一次扫盘。
pub fn refuses_a_mirror_round(task: &SyncTask) -> bool {
    if !crate::sync_tasks::is_mirror(task) {
        return false;
    }
    match std::fs::read_dir(root_of(task)) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

/// 一条变化等着跑、却没能跑成时，卡在哪一步。
///
/// 三种情况必须分开，因为它们该等的东西不一样：等一轮结束是几秒到几小时，等目录回来可能
/// 永远等不到，等退避到点是自己会好的。合成一种（「过一会儿再试」）就会出现空目录每拍
/// 重报一次、忙碌任务每拍重启一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setback {
    /// 引擎里已经有一轮同样的同步在跑（到期和文件变化常常落在同一拍）。
    /// 等那一轮真的结束，之后再补一次。
    Busy,
    /// 本机目录空着或读不了，任务又是镜像：不能动。等下一次变化、目录回来，或者用户按「运行」。
    Guarded,
    /// 没能启动：内核没连上、rclone 拒绝。隔一段时间再试，间隔逐次拉长。
    Deferred,
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
    /// 有值时这一轮跑不了。`take_ready` 会一直答「不」，直到有人把它解掉——
    /// 这正是「不是所有没跑成的都该自己重试」的落点。
    blocked: Option<Setback>,
    /// 跑这一轮期间又来了变化。结束时据此再补一轮，而不是当作已经同步过。
    during: bool,
    retry_at: Option<Instant>,
    /// 连续失败次数，只用来算退避间隔。
    fails: u32,
    problem: Option<String>,
}

impl Pending {
    /// 收到一次变化。事件抖动不用在这里去重：静默窗口本来就会把一串事件合成一轮。
    pub fn note_change(&mut self, now: Instant) {
        self.dirty = true;
        self.seen = Some(now);
        self.during = self.during || self.running;
        // 目录里又有东西了，镜像那关可以重新判；真还是空的，下一拍会再挡一次。
        // 「等一轮结束」那种阻塞不在这里解：那一轮还没结束。
        if self.blocked == Some(Setback::Guarded) {
            self.blocked = None;
        }
    }

    /// 记一条「这一段历史不可知，跑一轮确认」：刚接手、通知漏了、目录重建。
    pub fn expect_a_round(&mut self, now: Instant) {
        self.dirty = true;
        self.seen = Some(now);
        self.during = self.during || self.running;
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
        // 被挡住的愿望只能由事件、目录恢复或用户动作解开，不能靠自己在这里放行。
        if self.blocked.is_some() {
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
        self.during = false;
        self.retry_at = None;
        true
    }

    /// 这一轮没能真的开始，把愿望还回去，并记下卡在哪一步。
    pub fn note_setback(&mut self, setback: Setback, now: Instant) {
        self.running = false;
        self.dirty = true;
        match setback {
            // 等引擎里那一轮结束：结束时会调 `note_round_finished`，那里把阻塞解掉。
            Setback::Busy => {
                self.blocked = Some(Setback::Busy);
                self.retry_at = None;
            }
            // 等外面的事情变：新事件、目录回来、用户按「运行」。自己重试只会变成打扰。
            Setback::Guarded => {
                self.blocked = Some(Setback::Guarded);
                self.retry_at = None;
            }
            // 自己会好的那种：退避到点再来，间隔逐次拉长，上限见 [`MAX_RETRY`]。
            Setback::Deferred => {
                self.blocked = None;
                self.fails = self.fails.saturating_add(1);
                self.retry_at = Some(now + backoff(self.fails));
            }
        }
    }

    /// 一轮结束。跑的时候来的变化、以及被这一轮挡住的那条愿望，都留着，静默窗口过后再起一轮。
    pub fn note_round_finished(&mut self, ok: bool, now: Instant) {
        self.running = false;
        // 结束之后不管哪种阻塞都该解开：条件已经变了（要么这一轮动手了，要么它可以再试了）。
        let owed = self.during || self.blocked.is_some();
        self.blocked = None;
        self.during = false;
        if ok {
            self.fails = 0;
            self.dirty = owed;
        } else {
            self.fails = self.fails.saturating_add(1);
            self.dirty = true;
            self.seen = Some(now);
            self.retry_at = Some(now + backoff(self.fails));
        }
    }

    /// 监视器重新装上了。目录回来了，「空/读不了」那关可以先解开。
    pub fn note_watching(&mut self) {
        if self.blocked == Some(Setback::Guarded) {
            self.blocked = None;
        }
    }

    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.dirty || self.running
    }

    #[cfg(test)]
    pub fn blocked_by(&self) -> Option<Setback> {
        self.blocked
    }
}

/// 连续失败 `fails` 次之后等多久再试。第一次用 [`RETRY`]，之后翻倍，封顶 [`MAX_RETRY`]。
fn backoff(fails: u32) -> Duration {
    let step = fails.saturating_sub(1).min(6);
    RETRY.saturating_mul(1 << step).min(MAX_RETRY)
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
        /// 后端给的原话。撤掉重建要说清为什么，不能只说「漏了」。
        why: String,
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
                    let p = self.pending.entry(id).or_default();
                    // 监视器装上了：如果之前是因为「目录空着/读不了」被挡，现在可以重新判。
                    p.note_watching();
                    // 刚接手：接手之前发生过什么没人知道，跑一轮确认。
                    p.expect_a_round(now);
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
                Err(e) => Notice::Missed {
                    gen,
                    id: name.clone(),
                    why: format!("这台机器上盯文件变化出错了：{e}"),
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
                | Notice::Missed { gen, id, .. }
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
                Notice::Missed { why, .. } => {
                    // 后端自己说漏了：这个监视器已经不可信，撤掉，下一拍由 sync() 重装。
                    // 也不能悄悄重装——用户得知道有一段时间的变化是靠「补一轮」补回来的。
                    self.live.remove(&id);
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
    /// 卡在哪一步决定它什么时候再试：见 [`Setback`]。
    pub fn note_setback(&mut self, id: &str, setback: Setback, now: Instant) {
        if let Some(pending) = self.pending.get_mut(id) {
            pending.note_setback(setback, now);
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

    /// 测试要看「它卡在哪一步」。界面上没有这个数字，所以不进正式代码。
    #[cfg(test)]
    fn blocked_by(&self, id: &str) -> Option<Setback> {
        self.pending.get(id).and_then(Pending::blocked_by)
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
    fn a_busy_task_waits_for_the_running_round_then_runs_once() {
        let dir = temp_dir("busy");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);

        // 引擎里已经有一轮在跑：这一轮没能开始，愿望必须还回来。
        ws.note_setback("t1", Setback::Busy, later(t0));
        assert!(ws.wants_round("t1"), "没开始的这一轮不能算同步过了");
        assert_eq!(ws.blocked_by("t1"), Some(Setback::Busy));

        // 那一轮还没结束之前，多少拍都不许自己再起一次：那正是「每拍重启一次」的老毛病。
        for _ in 0..20 {
            assert!(
                ws.pump(later(t0)).ready.is_empty(),
                "等它结束期间不许自己再跑"
            );
        }

        // 那一轮真的结束了：只补一次，之后不再重复。
        ws.note_round_finished("t1", true, later(t0));
        assert_eq!(ws.blocked_by("t1"), None, "结束了就该解开");
        assert_eq!(
            ws.pump(later(later(t0))).ready,
            vec!["t1".to_string()],
            "那一轮结束后要补一次"
        );
        assert!(
            ws.pump(later(later(t0))).ready.is_empty(),
            "补过一次就够，不许再来"
        );
    }

    #[test]
    fn a_failed_start_backs_off_further_each_time() {
        let dir = temp_dir("backoff");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task], t0);
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);

        // 第一次没能启动：等一分钟后重试。
        let first = later(t0);
        ws.note_setback("t1", Setback::Deferred, first);
        assert!(
            ws.pump(first + Duration::from_secs(5)).ready.is_empty(),
            "刚失败过就别马上重试"
        );
        assert_eq!(
            ws.pump(first + RETRY + Duration::from_secs(1)).ready,
            vec!["t1".to_string()],
            "歇够了要自己再试一次"
        );

        // 又失败：这一次的间隔要更长，不能每分钟打一次。
        let second = first + RETRY + Duration::from_secs(1);
        ws.note_setback("t1", Setback::Deferred, second);
        assert!(
            ws.pump(second + RETRY + Duration::from_secs(1))
                .ready
                .is_empty(),
            "第二次失败要等更久"
        );
        assert_eq!(
            ws.pump(second + RETRY * 2 + Duration::from_secs(1)).ready,
            vec!["t1".to_string()],
            "退避加长了"
        );

        // 一直失败也不会超过上限：上限之前不跑，到点照样要试。
        let long = second + RETRY * 3;
        for _ in 0..10 {
            ws.note_setback("t1", Setback::Deferred, long);
        }
        assert!(
            ws.pump(long + MAX_RETRY - Duration::from_secs(1))
                .ready
                .is_empty(),
            "退避有上限，也不许提前跑"
        );
        assert_eq!(
            ws.pump(long + MAX_RETRY + Duration::from_secs(1)).ready,
            vec!["t1".to_string()],
            "上限到点照样要试"
        );
    }

    #[test]
    fn a_blocked_mirror_folder_waits_for_a_change_then_runs_once() {
        let dir = temp_dir("mirror-blocked");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        task.mode = MODE_SYNC.to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task.clone()], t0);
        assert_eq!(ws.pump(later(t0)).ready, vec!["t1".to_string()]);

        // 空目录被挡下：这一轮不跑，愿望着；而且不许自己重来。
        assert!(refuses_a_mirror_round(&task));
        ws.note_setback("t1", Setback::Guarded, later(t0));
        assert!(ws.wants_round("t1"), "被挡下的那一轮不能当作已经同步过");
        assert_eq!(ws.blocked_by("t1"), Some(Setback::Guarded));
        for _ in 0..20 {
            assert!(
                ws.pump(later(t0)).ready.is_empty(),
                "没人动过就不许自己再起：每分钟重报一次只是打扰"
            );
        }

        // 里面有东西了（真实世界里会先来一条变化通知）。
        write(&dir, "a.txt", b"a");
        assert!(!refuses_a_mirror_round(&task));
        let gen = ws.gen_of("t1").expect("监视器");
        ws.tx
            .try_send(Notice::Event {
                gen,
                id: "t1".to_string(),
            })
            .expect("队列没满");
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

        // 队列满记下的「欠一轮」和普通变化一样要等静默窗口过去，所以这一次 pump 只报告、不跑。
        let tick = ws.pump(later(t0));
        assert!(tick.ready.is_empty(), "刚记下的愿望要等静默窗口");
        assert_eq!(tick.problems.len(), 1, "只报一次");
        assert!(tick.problems[0].1.contains("队列满"));
        // 窗口过去了：补一轮，而且只补一轮。
        assert_eq!(
            ws.pump(later(later(t0))).ready,
            vec!["t1".to_string()],
            "满一次只该补一轮"
        );
        // 队列清空了，标志也复位了：再 pump 不该重复报告，也不该再跑一轮。
        let again = ws.pump(later(later(t0)));
        assert!(again.problems.is_empty(), "同一个问题只说一次");
        assert!(again.ready.is_empty(), "一轮就够");
    }

    #[test]
    fn a_backend_error_rebuilds_the_watcher_and_says_why() {
        let dir = temp_dir("backend-error");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        let mut ws = Watchers::default();
        let t0 = Instant::now();
        ws.sync(&[task.clone()], t0);
        ws.pump(later(t0));
        ws.note_round_finished("t1", true, later(t0));
        let old = ws.gen_of("t1").expect("监视器");

        // 后端自己报错：这个监视器已经不可信，必须撤掉等重建，而且要把原因说给用户。
        ws.tx
            .try_send(Notice::Missed {
                gen: old,
                id: "t1".to_string(),
                why: "这台机器上盯文件变化出错了：系统给的原因".to_string(),
            })
            .expect("队列没满");
        let tick = ws.pump(later(t0));
        assert_eq!(tick.problems.len(), 1, "要告诉用户，不能自己吞掉");
        assert!(
            tick.problems[0].1.contains("盯文件变化出错了"),
            "要带上后端原话：{}",
            tick.problems[0].1
        );
        assert!(ws.gen_of("t1").is_none(), "不可信的监视器要撤掉");
        assert!(ws.wants_round("t1"), "撤掉不等于这一轮不用补");

        // 下一拍 sync() 重新装上：代号变了，旧代号的迟到消息不能再影响它。
        assert!(
            ws.sync(&[task], later(t0)).is_empty(),
            "目录还在，重装不该报错"
        );
        let fresh = ws.gen_of("t1").expect("重装后的监视器");
        assert_ne!(fresh, old, "重建要用新的代号");
        ws.tx
            .try_send(Notice::Event {
                gen: old,
                id: "t1".to_string(),
            })
            .expect("队列没满");
        assert!(
            ws.pump(later(t0)).ready.is_empty(),
            "旧代号的迟到消息要丢掉"
        );
        assert_eq!(
            ws.pump(later(later(t0))).ready,
            vec!["t1".to_string()],
            "重新盯上之后补一轮"
        );
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

        // 目录回来了：重新接手，并且再确认一轮（接手记下的愿望同样要等静默窗口）。
        std::fs::create_dir_all(&dir).expect("重建目录");
        assert!(ws.sync(&[task], later(t0)).is_empty());
        assert!(ws.pump(later(t0)).ready.is_empty(), "刚接手也要等静默窗口");
        assert_eq!(
            ws.pump(later(later(t0))).ready,
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
    fn a_mirror_round_is_refused_when_the_folder_is_empty_or_unreadable() {
        let dir = temp_dir("empty-mirror");
        let mut task = base_task(DIR_UP, true, Some(true));
        task.local = dir.display().to_string();
        task.mode = MODE_SYNC.to_string();
        assert!(
            refuses_a_mirror_round(&task),
            "空目录大概是盘没挂上，不是用户把文件删光了"
        );

        write(&dir, "a.txt", b"x");
        assert!(!refuses_a_mirror_round(&task), "有东西了就照常跑");

        // 目录不见了：读不出来，和空目录后果一样——都是「云端会被清空」。
        std::fs::remove_dir_all(&dir).expect("删掉目录");
        assert!(
            refuses_a_mirror_round(&task),
            "读不出来的目录要当空目录挡下来"
        );

        task.mode = MODE_COPY.to_string();
        assert!(
            !refuses_a_mirror_round(&task),
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
