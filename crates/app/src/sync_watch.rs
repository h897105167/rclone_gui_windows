//! 本机文件夹变了就提前跑一轮同步。
//!
//! 这里只回答两个问题：这个任务的本地目录现在长什么样、这一轮变化停了没有。
//! 真正跑同步还是 `crate::sync_tasks::start`，`mode` 与 `direction` 一个字都不改——
//! 看见文件变了，不等于允许扩大同步范围，更不等于允许删东西。
//!
//! 为什么用轮询而不是系统文件通知：加一个监视库就要动 `Cargo.lock`，而本机不能跑
//! cargo（Rust 测试程序会被杀毒隔离），锁文件手改错一行整个构建就废了。轮一遍目录树
//! 是几十毫秒的事，而且判定过程能被单元测试完全盯住。

use crate::config::SyncTask;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// 多久扫一遍。太密会在同步大目录时白烧 CPU，太疏用户会觉得"改了没反应"。
pub const SCAN_TICK: Duration = Duration::from_secs(30);

/// 变化之后要安静这么久才真正开始，否则复制一个几百兆的目录会被每一轮扫描各触发一次。
pub const SETTLE: Duration = Duration::from_secs(20);

/// 文件数上限。超过就放弃自动比对：那种规模本来也不适合"改一下就立刻跑"。
pub const MAX_ENTRIES: u64 = 200_000;

/// 目录深度上限。目录树异常深时宁可少看几层，也不让一次扫描拖住整个后台循环。
const MAX_DEPTH: usize = 64;

/// 一棵目录树当前的"指纹"。
///
/// 只看文件的数量、总字节数和最新修改时刻：这三个值在常见的"加一个文件、改一个文件、
/// 删一个文件"里都会变，而它们的计算不依赖文件内容，几十万文件也扫得完。
/// 新建一个空目录不算变化（同步一次也不会搬运任何字节）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Print {
    pub files: u64,
    pub bytes: u64,
    /// 最新修改时刻，Unix 秒；没有文件时为 0。
    pub newest: i64,
}

/// 扫一遍目录树。
///
/// 目录不存在、没有读权限、或者文件太多都返回 `Err`：调用方据此这一轮什么都不做，
/// 下一轮再试。`root` 本身读不出来才可能永久失败，那种情况会一直报同一个原因。
/// 符号链接不跟：链接目标的变化不该算进这个文件夹的内容，跟进去还会绕成环。
pub fn scan(root: &Path) -> Result<Print, String> {
    let mut print = Print::default();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let kind = entry
                .file_type()
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                stack.push((entry.path(), depth + 1));
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let meta = entry
                .metadata()
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
            print.files += 1;
            print.bytes = print.bytes.saturating_add(meta.len());
            if print.files > MAX_ENTRIES {
                return Err(format!("文件超过 {MAX_ENTRIES} 个，暂不自动比对"));
            }
            if let Ok(secs) = meta
                .modified()
                .map_err(|_| ())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).map_err(|_| ()))
            {
                print.newest = print.newest.max(secs.as_secs() as i64);
            }
        }
    }
    Ok(print)
}

/// 一个任务这一轮该做什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// 没什么可做的（或者刚记下基线）。
    Idle,
    /// 读不到这个目录。调用方应该把原因告诉用户——只报一次，恢复后重新开始比对。
    Problem(String),
    /// 变化已经停稳，可以开始一轮。
    Fire,
}

/// 一个任务的观察状态。
#[derive(Debug, Default)]
pub struct Watch {
    last: Option<Print>,
    /// 最近一次看见变化（而不是"最近一次看见不同"）的时刻。
    churn: Option<Instant>,
    failing: bool,
}

impl Watch {
    /// 交给它一次扫描结果，拿回这一轮该做什么。
    ///
    /// 第一次拿到的一定只是基线：程序刚起来就把整个目录当"刚变过"跑一轮，重启几次就
    /// 会重复上传几次，对远端没有意义。
    pub fn observe(&mut self, seen: Result<Print, String>, now: Instant) -> Step {
        let print = match seen {
            Ok(p) => p,
            Err(e) => {
                if self.failing {
                    return Step::Idle;
                }
                self.failing = true;
                // 读不到的时候保留旧基线与 pending 计时：目录恢复后接着比，不重来。
                return Step::Problem(e);
            }
        };
        self.failing = false;
        match self.last {
            None => {
                self.last = Some(print);
                Step::Idle
            }
            Some(last) if last != print => {
                self.last = Some(print);
                self.churn = Some(now);
                Step::Idle
            }
            Some(_) => match self.churn {
                Some(t) if now.saturating_duration_since(t) >= SETTLE => {
                    self.churn = None;
                    Step::Fire
                }
                _ => Step::Idle,
            },
        }
    }
}

/// 按任务 id 存每个任务的观察状态。
#[derive(Debug, Default)]
pub struct Watchers {
    map: HashMap<String, Watch>,
}

impl Watchers {
    pub fn observe(&mut self, id: &str, seen: Result<Print, String>, now: Instant) -> Step {
        self.map
            .entry(id.to_string())
            .or_default()
            .observe(seen, now)
    }

    /// 只留下还需要盯的任务。删掉或改成"云端 → 本机"的任务不该一直占着内存。
    pub fn retain(&mut self, alive: &HashSet<String>) {
        self.map.retain(|id, _| alive.contains(id));
    }

    /// 只在测试里数一下还盯着几个任务。界面上没有这个数字，所以不进正式代码。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// 这个任务要不要盯着本机目录。
///
/// 「云端 → 本机」一律不盯：那时候本机文件的变化就是用户自己的编辑，拿它去触发同步
/// 正是"自动覆盖用户改动"的来源。停用的任务也不盯——用户按了暂停，自动就不该再启动它。
pub fn watches(task: &SyncTask) -> bool {
    task.enabled && task.auto_sync && crate::sync_tasks::is_up(task)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rg-watch-{}-{}-{tag}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, body: &[u8]) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn base_task(direction: &str, enabled: bool, auto_sync: bool) -> SyncTask {
        SyncTask {
            id: "t1".into(),
            name: "照片".into(),
            local: "D:\\Photos".into(),
            remote: "gdrive:backup/photos".into(),
            direction: direction.into(),
            mode: crate::sync_tasks::MODE_COPY.into(),
            enabled,
            auto_sync,
            interval_hours: 6,
            last_run: String::new(),
            last_status: String::new(),
        }
    }

    #[test]
    fn a_quiet_folder_never_fires() {
        let dir = temp_dir("quiet");
        write(&dir, "a.txt", b"aaa");
        let t0 = Instant::now();
        let mut w = Watch::default();
        assert_eq!(w.observe(scan(&dir), t0), Step::Idle, "第一次只该记基线");
        assert_eq!(
            w.observe(scan(&dir), t0 + SCAN_TICK),
            Step::Idle,
            "没变过的目录不该跑"
        );
        assert_eq!(w.observe(scan(&dir), t0 + SETTLE * 4), Step::Idle);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_change_fires_only_after_it_settles() {
        let dir = temp_dir("oneshot");
        write(&dir, "a.txt", b"aaa");
        let t0 = Instant::now();
        let mut w = Watch::default();
        assert_eq!(w.observe(scan(&dir), t0), Step::Idle);
        write(&dir, "b.txt", b"bbb");
        assert_eq!(
            w.observe(scan(&dir), t0 + SCAN_TICK),
            Step::Idle,
            "刚变完就开始会把半个目录复制过去"
        );
        assert_eq!(
            w.observe(scan(&dir), t0 + SCAN_TICK + SETTLE / 2),
            Step::Idle,
            "还没安静够久"
        );
        assert_eq!(
            w.observe(scan(&dir), t0 + SCAN_TICK + SETTLE + Duration::from_secs(1)),
            Step::Fire
        );
        assert_eq!(
            w.observe(
                scan(&dir),
                t0 + SCAN_TICK + SETTLE + SCAN_TICK + Duration::from_secs(1)
            ),
            Step::Idle,
            "同一份变化只能触发一轮"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn more_changes_restart_the_wait() {
        let dir = temp_dir("restart");
        let t0 = Instant::now();
        let mut w = Watch::default();
        assert_eq!(w.observe(scan(&dir), t0), Step::Idle);
        write(&dir, "a.txt", b"aaa");
        assert_eq!(w.observe(scan(&dir), t0 + SCAN_TICK), Step::Idle);
        // 边传边改：正在复制的目录不该被判定为"停稳了"。
        write(&dir, "b.txt", b"bbb");
        let second = t0 + SCAN_TICK + Duration::from_secs(10);
        assert_eq!(w.observe(scan(&dir), second), Step::Idle);
        assert_eq!(
            w.observe(scan(&dir), second + SETTLE / 2),
            Step::Idle,
            "后一次变化应把等待重新计时"
        );
        assert_eq!(
            w.observe(scan(&dir), second + SETTLE + Duration::from_secs(1)),
            Step::Fire
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_restart_records_a_baseline_instead_of_firing() {
        let dir = temp_dir("restart-base");
        write(&dir, "a.txt", b"aaa");
        // 程序刚启动：文件是上次运行期间改的，不该被当成"刚变过"再传一遍。
        let mut w = Watch::default();
        let t0 = Instant::now();
        assert_eq!(w.observe(scan(&dir), t0), Step::Idle);
        assert_eq!(w.observe(scan(&dir), t0 + SETTLE * 3), Step::Idle);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_folder_is_reported_once_and_then_quiet() {
        let dir = temp_dir("missing").join("gone");
        let t0 = Instant::now();
        let mut w = Watch::default();
        match w.observe(scan(&dir), t0) {
            Step::Problem(msg) => assert!(msg.contains("gone"), "报错里要能看出是哪个目录：{msg}"),
            other => panic!("目录不存在该报一次原因，实际 {other:?}"),
        }
        assert_eq!(
            w.observe(scan(&dir), t0 + SCAN_TICK),
            Step::Idle,
            "同一个原因不该每轮都刷一遍"
        );
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, "a.txt", b"aaa");
        assert_eq!(
            w.observe(scan(&dir), t0 + SCAN_TICK * 2),
            Step::Idle,
            "恢复后先记基线"
        );
        write(&dir, "b.txt", b"bbb");
        assert_eq!(w.observe(scan(&dir), t0 + SCAN_TICK * 3), Step::Idle);
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn a_folder_recovers_after_the_problem_clears() {
        let dir = temp_dir("recover").join("gone");
        let t0 = Instant::now();
        let mut w = Watch::default();
        assert!(matches!(w.observe(scan(&dir), t0), Step::Problem(_)));
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, "a.txt", b"aaa");
        assert_eq!(w.observe(scan(&dir), t0 + SCAN_TICK), Step::Idle);
        write(&dir, "b.txt", b"bbb");
        assert_eq!(w.observe(scan(&dir), t0 + SCAN_TICK * 2), Step::Idle);
        assert_eq!(
            w.observe(
                scan(&dir),
                t0 + SCAN_TICK * 2 + SETTLE + Duration::from_secs(1)
            ),
            Step::Fire,
            "目录恢复正常后应该接着工作"
        );
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn a_new_file_changes_the_print() {
        let dir = temp_dir("print");
        let before = scan(&dir).unwrap();
        write(&dir, "a.txt", b"12345");
        let after = scan(&dir).unwrap();
        assert_ne!(before, after);
        assert_eq!(after.files, 1);
        assert_eq!(after.bytes, 5);
        assert_eq!(scan(&dir).unwrap(), after, "同一份内容两次扫描必须一致");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nested_folders_are_counted() {
        let dir = temp_dir("nested");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        write(&dir, "a/b/c.txt", b"123");
        let p = scan(&dir).unwrap();
        assert_eq!((p.files, p.bytes), (1, 3), "子目录里的文件也算这棵树的指纹");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_enabled_auto_local_to_remote_tasks_are_watched() {
        use crate::sync_tasks::{DIR_DOWN, DIR_UP};
        assert!(watches(&base_task(DIR_UP, true, true)));
        assert!(
            !watches(&base_task(DIR_DOWN, true, true)),
            "云端 → 本机时，本机变化是用户自己的编辑，不能拿它触发覆盖"
        );
        assert!(
            !watches(&base_task(DIR_UP, false, true)),
            "用户暂停了就别自动跑"
        );
        assert!(!watches(&base_task(DIR_UP, true, false)), "这一项关掉了");
    }

    #[test]
    fn watchers_forget_tasks_that_are_gone() {
        let dir = temp_dir("retain");
        write(&dir, "a.txt", b"a");
        let t0 = Instant::now();
        let mut ws = Watchers::default();
        ws.observe("keep", scan(&dir), t0);
        ws.observe("drop", scan(&dir), t0);
        assert_eq!(ws.len(), 2);
        let alive: HashSet<String> = ["keep".to_string()].into_iter().collect();
        ws.retain(&alive);
        assert_eq!(ws.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
