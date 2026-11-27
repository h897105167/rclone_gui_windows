//! 定时同步任务：把本地目录和 rclone 远程目录对齐。
//!
//! 这一层只做三件事：算出「什么时候该跑」、向 rclone 提交一次作业、把运行中的作业
//! 轮询到结束。真正的文件搬运由 rclone 内核完成，本模块不碰文件系统。
//!
//! 两个容易踩的坑，代码里专门处理了：
//!
//! 1. 网络抖动不能算作业结束。查询作业状态的请求超时，只说明这一次没问到；
//!    如果把这种错误当成「作业没了」，下一刻钟就会把同一个目录再传一遍。
//! 2. rclone 的 jobid 只在当前内核进程内有效，内核重启后 3 号作业可能是别人的。
//!    所以每个作业都记下「属于哪一代内核」，跨代直接判中断，不拿数字去猜；
//!    另外 rclone 自己会给每次内核启动一个 `executeId`，轮询时顺带比对它——外部接进来的
//!    rclone 重启没人通知我们，只有这个值能看出来。

use crate::config::SyncTask;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration as ChronoDuration, Local};
use rcgui_rc::{JobStatus, RcClient, RcError, RcErrorKind};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 上行：本地 → 远程。
pub const DIR_UP: &str = "up";
/// 下行：远程 → 本地。
pub const DIR_DOWN: &str = "down";
/// 只增不删（rclone sync/copy）。默认行为。
pub const MODE_COPY: &str = "copy";
/// 镜像（rclone sync/sync）：对端多余的会被删掉，必须由用户显式选择。
pub const MODE_SYNC: &str = "sync";
/// 新任务的默认周期，小时。
pub const DEFAULT_HOURS: u32 = 6;
/// 周期上限；超过这个值说明填错了，不是「一个月同步一次」的意思。
pub const MAX_HOURS: u32 = 720;

/// 方向是否为上行。空值和未知值都按上行处理（历史上 direction 默认是空串）。
pub fn is_up(task: &SyncTask) -> bool {
    task.direction != DIR_DOWN
}

/// 是否为镜像模式（会删除对端多余文件）。
pub fn is_mirror(task: &SyncTask) -> bool {
    task.mode == MODE_SYNC
}

/// 给界面用的方向标签。
pub fn ui_direction(task: &SyncTask) -> &'static str {
    if is_up(task) {
        "local-to-remote"
    } else {
        "remote-to-local"
    }
}

/// 把 `gdrive:backup/photos` 拆成（远程名，远程内路径）。
///
/// 远程名不能为空，至少要有一个 `:`；路径前导 `/` 会去掉，因为 rclone 两种写法等价。
/// 路径可以为空，表示远程根目录。
pub fn split_remote(remote: &str) -> Result<(&str, &str)> {
    let (name, path) = remote
        .split_once(':')
        .ok_or_else(|| anyhow!("远程路径缺少「远程名:」，例如 gdrive:backup"))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("远程名为空，冒号前要写远程配置的名字"));
    }
    Ok((name, path.trim_start_matches('/')))
}

/// 按方向给出（源，目标）。提交给 rclone 的 srcFs / dstFs 就是这两个。
pub fn pair(task: &SyncTask) -> (&str, &str) {
    if is_up(task) {
        (task.local.as_str(), task.remote.as_str())
    } else {
        (task.remote.as_str(), task.local.as_str())
    }
}

/// 提交前检查。这里拒绝的每一种情况，都是「跑起来一定出问题」的。
pub fn validate(task: &SyncTask) -> Result<()> {
    if task.id.trim().is_empty() {
        return Err(anyhow!("任务缺少 id"));
    }
    // 只认 up / down 两个值。写错方向后如果按上行兜底，会把本地目录推上去。
    if !matches!(task.direction.as_str(), "" | DIR_UP | DIR_DOWN) {
        return Err(anyhow!(
            "方向只能是 {} 或 {}，收到「{}」",
            DIR_UP,
            DIR_DOWN,
            task.direction
        ));
    }
    if !matches!(task.mode.as_str(), "" | MODE_COPY | MODE_SYNC) {
        return Err(anyhow!(
            "模式只能是 {}（只增不删）或 {}（镜像删除），收到「{}」",
            MODE_COPY,
            MODE_SYNC,
            task.mode
        ));
    }
    if task.local.trim().is_empty() {
        return Err(anyhow!("本地目录为空"));
    }
    let local = std::path::Path::new(&task.local);
    if !local.is_absolute() {
        return Err(anyhow!("本地目录要写绝对路径：{}", task.local));
    }
    if local
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(anyhow!("本地目录不能包含 ..：{}", task.local));
    }
    if task.remote.trim().is_empty() {
        return Err(anyhow!("远程路径为空"));
    }
    split_remote(&task.remote)?;
    if task.interval_hours > MAX_HOURS {
        return Err(anyhow!(
            "周期 {} 小时太大，最多 {} 小时",
            task.interval_hours,
            MAX_HOURS
        ));
    }
    // 源和目标落在同一处：上传会把目录搬进自己里面，下载会把远程写回自己。
    if same_place(&task.local, split_remote(&task.remote)?.1, &task.remote) {
        return Err(anyhow!(
            "源和目标指向同一处（{} ↔ {}），这样会自己套自己",
            task.local,
            task.remote
        ));
    }
    Ok(())
}

/// 粗判本地目录和远程路径是不是同一处，或者一个在另一个里面。
///
/// 远程是别的机器上的目录，正常情况下无法和本地比较；能比出来的只有两种明显错误：
/// 字符串完全一样（用户把远程路径抄成本地路径），以及远程路径写成绝对盘符且是本地的子目录。
fn same_place(local: &str, remote_path: &str, remote: &str) -> bool {
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    let l = norm(local);
    let r = norm(remote_path);
    if l.is_empty() || r.is_empty() {
        return false;
    }
    if l == r {
        return true;
    }
    // 只有远程路径本身像本地绝对路径时才做包含判断，避免把 gdrive:photos 误判。
    let absolute = r.len() > 2 && r.as_bytes()[1] == b':' || remote.starts_with("\\\\");
    absolute && (l.starts_with(&format!("{r}\\")) || r.starts_with(&format!("{l}\\")))
}

/// 读上一次运行时间。格式不对就当没跑过，宁可早跑一次也不要永远不跑。
fn parse_last(task: &SyncTask) -> Option<DateTime<Local>> {
    let raw = task.last_run.trim();
    if raw.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Local))
}

/// 下次该跑的时间。None 表示不会自动跑（已停用，或周期为 0 且已经跑过一次）。
///
/// 周期 0 的含义是「启动时跑一次」：从没跑过时立刻到期，跑过之后就不再自动触发。
pub fn due_at(task: &SyncTask, now: DateTime<Local>) -> Option<DateTime<Local>> {
    if !task.enabled {
        return None;
    }
    let last = parse_last(task);
    if task.interval_hours == 0 {
        return match last {
            Some(_) => None,
            None => Some(now),
        };
    }
    let hours = task.interval_hours.min(MAX_HOURS) as i64;
    match last {
        Some(t) => Some(t + ChronoDuration::hours(hours)),
        None => Some(now),
    }
}

/// 现在是不是该跑了。
pub fn is_due(task: &SyncTask, now: DateTime<Local>) -> bool {
    matches!(due_at(task, now), Some(t) if t <= now)
}

/// 界面上的「下次/等待中」文案。
pub fn next_text(task: &SyncTask, now: DateTime<Local>) -> String {
    if !task.enabled {
        return rcgui_i18n::t("已停用").to_string();
    }
    match due_at(task, now) {
        None => rcgui_i18n::t("不再自动同步（周期 0 小时，只在启动时跑一次）").to_string(),
        Some(t) if t <= now => rcgui_i18n::t("等待执行").to_string(),
        Some(t) => {
            let mins = (t - now).num_minutes().max(1);
            if mins < 60 {
                rcgui_i18n::f("{} 分钟后", &[&mins])
            } else {
                rcgui_i18n::f("{} 小时后", &[&(mins / 60)])
            }
        }
    }
}

/// 引擎里一个任务占用的槽位。
///
/// 对外只用 [`Active`] 和下面的查询方法；`generation` 记录这个 jobid 属于哪一代内核，
/// `reserved` 标记「有人先 claim 了、还没交给 start」，`waiting` 表示上次查询遇到暂时性错误。
/// `execute_id` 是 rclone 自己的「这次内核启动」编号：作业号在重启后会被重用，而它不会，
/// 所以判断某个 jobid 是不是还属于当初那个内核，要看它。
struct Slot {
    jobid: i64,
    started: DateTime<Local>,
    generation: u64,
    reserved: bool,
    waiting: bool,
    execute_id: Option<String>,
}

/// 对外可见的运行状态。
#[derive(Debug, Clone, Copy)]
pub struct Active {
    /// rclone 返回的作业号；0 表示已占位但还没提交成功。
    pub jobid: i64,
    pub started: DateTime<Local>,
}

/// 谁在跑。进程内唯一，通过 `&Engine` 传给各个调用点。
#[derive(Default)]
pub struct Engine {
    active: Mutex<HashMap<String, Slot>>,
    generation: AtomicU64,
}

/// `start` 占下的槽位。没 `commit` 就被丢弃（包括提交出错、或被中途取消）时会自动释放，
/// 否则任务会永远停在「运行中」，再也不会重试。
pub struct Claim<'a> {
    engine: &'a Engine,
    id: String,
    committed: bool,
}

impl<'a> Claim<'a> {
    /// 记下提交成功拿到的作业号。
    pub fn commit(mut self, jobid: i64) {
        self.engine.set_job(&self.id, jobid);
        self.committed = true;
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.engine.release(&self.id);
        }
    }
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// 先占位，再自己提交。给「调用方想先标记再启动」的老用法留的出口。
    ///
    /// 占位后必须由 [`start`] 接手，或者调用 [`release`](Self::release) 放掉；
    /// 一直占着会让这个任务再也启动不了。
    pub fn claim(&self, id: &str) -> Result<()> {
        let mut m = self.active.lock().unwrap();
        if m.contains_key(id) {
            return Err(anyhow!("这个任务已经在运行了"));
        }
        m.insert(
            id.to_string(),
            Slot {
                jobid: 0,
                started: Local::now(),
                generation: self.generation.load(Ordering::SeqCst),
                reserved: true,
                waiting: false,
                execute_id: None,
            },
        );
        Ok(())
    }

    /// `start` 专用的占位：没有槽位就新建；已有的槽位如果是 `claim` 留下的空位就接手。
    ///
    /// 接手这一步是关键：以前 `claim` 和 `start` 各自检查，调用方先 claim 再 start 必然报
    /// 「已在运行」，任务反而启动不了。已经提交过作业的槽位仍然拒绝，那才是真的重复启动。
    pub fn begin(&self, id: &str) -> Result<Claim<'_>> {
        let mut m = self.active.lock().unwrap();
        match m.get_mut(id) {
            Some(slot) if slot.jobid == 0 && slot.reserved => {
                slot.reserved = false;
                slot.started = Local::now();
                slot.waiting = false;
            }
            Some(_) => return Err(anyhow!("这个任务已经在运行了")),
            None => {
                m.insert(
                    id.to_string(),
                    Slot {
                        jobid: 0,
                        started: Local::now(),
                        generation: self.generation.load(Ordering::SeqCst),
                        reserved: false,
                        waiting: false,
                        execute_id: None,
                    },
                );
            }
        }
        Ok(Claim {
            engine: self,
            id: id.to_string(),
            committed: false,
        })
    }

    pub fn set_job(&self, id: &str, jobid: i64) {
        let mut m = self.active.lock().unwrap();
        if let Some(slot) = m.get_mut(id) {
            slot.jobid = jobid;
            slot.reserved = false;
            slot.generation = self.generation.load(Ordering::SeqCst);
            slot.waiting = false;
            // 新提交的作业属于哪个内核实例，还得问一次才知道。
            slot.execute_id = None;
        }
    }

    /// 记下这个作业所属的内核实例（rclone `job/status` 里的 `executeId`）。
    ///
    /// 槽位已经释放时什么也不做：那是作业刚结束的正常竞争，不该把它又写回来。
    pub fn set_execute_id(&self, id: &str, execute: String) {
        let mut m = self.active.lock().unwrap();
        if let Some(slot) = m.get_mut(id) {
            slot.waiting = false;
            slot.execute_id = Some(execute);
        }
    }

    /// 已记下的内核实例编号；`None` 表示还没问到，不代表作业没了。
    pub fn execute_id_of(&self, id: &str) -> Option<String> {
        self.active
            .lock()
            .unwrap()
            .get(id)
            .and_then(|s| s.execute_id.clone())
    }

    pub fn release(&self, id: &str) {
        self.active.lock().unwrap().remove(id);
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.active.lock().unwrap().contains_key(id)
    }

    /// 正在跑的任务 id，按字典序，方便界面稳定刷新。
    pub fn active_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.active.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// 作业号反查任务 id。只在当前这一代内核内有效。
    pub fn task_of(&self, jobid: i64) -> Option<String> {
        if jobid == 0 {
            return None;
        }
        self.active
            .lock()
            .unwrap()
            .iter()
            .find(|(_, s)| s.jobid == jobid)
            .map(|(id, _)| id.clone())
    }

    /// 取一个任务当前的运行状态（作业号 + 开始时间）。
    pub fn active_of(&self, id: &str) -> Option<Active> {
        self.active.lock().unwrap().get(id).map(|s| Active {
            jobid: s.jobid,
            started: s.started,
        })
    }

    /// 当前内核是第几代。内核每启动一次就 +1，用来判断作业号还能不能查。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// 内核（重新）启动后调用，作废旧一代的作业号。
    ///
    /// 自己托管的内核（重新）启动后调用，作废旧一代的作业号。
    ///
    /// 这一代计数器只覆盖「我们自己启动的内核」。别人从外面接进来的 rclone 重启时没人会
    /// 调用这里，那种情况靠 [`poll`] 里的 `executeId` 比对来发现——rclone 的 `job/status`
    /// 确实带这个字段，每次内核启动都会换一个值。所以这里换代只是让自管内核的中断判得
    /// 更早、更明确，不是唯一手段。
    pub fn on_core_started(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 上次查询是否遇到暂时性错误（网络没连上、超时）。界面用它显示「等待重连」。
    pub fn is_waiting(&self, id: &str) -> bool {
        self.active
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.waiting)
            .unwrap_or(false)
    }

    /// 标记/清除「等待重连」。
    pub fn set_waiting(&self, id: &str, waiting: bool) {
        let mut m = self.active.lock().unwrap();
        if let Some(slot) = m.get_mut(id) {
            slot.waiting = waiting;
        }
    }

    /// 正在等待重连的任务 id。
    pub fn waiting_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.waiting)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }
}

/// 从一次 `job/status` 的返回里读 rclone 的内核实例编号。
///
/// 读不到（老版本 rclone、字段缺失、空串）就返回 `None`：那只是「不认识」，不能当成换了内核。
fn execute_id_from(v: &serde_json::Value) -> Option<String> {
    let s = v.get("executeId")?.as_str()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 问一次这个作业属于哪个内核实例；问不到就返回 `None`，不代表作业没了。
async fn fetch_execute_id(rc: &RcClient, jobid: i64) -> Option<String> {
    let v = rc
        .call("job/status", &json!({ "jobid": jobid }))
        .await
        .ok()?;
    execute_id_from(&v)
}

/// 提交一次同步。返回 rclone 的作业号。
///
/// 已经有人在跑同一个任务时返回 Err，不会重复提交。提交失败（或这个 future 被取消）
/// 会释放占位，任务下一轮还能重试。
pub async fn start(rc: &RcClient, engine: &Engine, task: &SyncTask) -> Result<i64> {
    validate(task)?;
    let claim = engine.begin(&task.id)?;
    let (src, dst) = pair(task);
    let jobid = if is_mirror(task) {
        rc.sync_sync_async(src, dst, false).await?
    } else {
        rc.sync_copy_async(src, dst, false).await?
    };
    claim.commit(jobid);
    // 顺手记下这个作业属于哪个内核实例。查不到不影响启动，只是之后没法用它判换代。
    if let Some(exec) = fetch_execute_id(rc, jobid).await {
        engine.set_execute_id(&task.id, exec);
    }
    Ok(jobid)
}

/// 一个刚刚结束的任务。
pub struct Finished {
    pub task_id: String,
    pub jobid: i64,
    /// `JobStatus::outcome()` 的结果（ok / failed: …），或者中断说明。
    pub status: String,
}

/// 这个错误是不是「作业确实不在了」。
///
/// 只有 rclone 明确回答作业不存在（或者内核根本不认这个方法）才算。
/// 连接失败、超时、返回体解析不出来都属于「不知道」，不能据此结束任务。
fn job_is_gone(e: &RcError) -> bool {
    match e.kind {
        RcErrorKind::Unsupported => true,
        RcErrorKind::Api => {
            let d = e.detail.to_ascii_lowercase();
            d.contains("job not found") || d.contains("no such job")
        }
        _ => false,
    }
}

/// 查一遍正在跑的作业，返回这一轮刚刚结束的。
///
/// 还在跑的作业留在 Active 里；查询遇到暂时性错误也只是标记「等待重连」，不结束。
/// 内核换代后老作业一律判中断，因为数字相同的作业可能已经不是原来那个。
pub async fn poll(rc: &RcClient, engine: &Engine) -> Vec<Finished> {
    let generation = engine.generation();
    let snapshot: Vec<(String, i64, u64, Option<String>)> = {
        let m = engine.active.lock().unwrap();
        m.iter()
            .filter(|(_, s)| s.jobid != 0)
            .map(|(id, s)| (id.clone(), s.jobid, s.generation, s.execute_id.clone()))
            .collect()
    };

    let mut done = Vec::new();
    for (task_id, jobid, gen, known_exec) in snapshot {
        if gen != generation {
            engine.release(&task_id);
            done.push(Finished {
                task_id,
                jobid,
                status: rcgui_i18n::t("已中断：rclone 内核重启，结果未知").to_string(),
            });
            continue;
        }
        // 一次请求同时拿到状态和内核实例编号，省一次往返。
        let raw = match rc.call("job/status", &json!({ "jobid": jobid })).await {
            Ok(v) => v,
            Err(e) if job_is_gone(&e) => {
                engine.release(&task_id);
                done.push(Finished {
                    task_id,
                    jobid,
                    status: rcgui_i18n::f("failed: 查不到这个任务（{}）", &[&e]),
                });
                continue;
            }
            Err(_) => {
                // 暂时问不到：保留 Active，界面显示等待重连，下一轮继续问。
                engine.set_waiting(&task_id, true);
                continue;
            }
        };
        match (&known_exec, &execute_id_from(&raw)) {
            (Some(known), Some(now)) if known != now => {
                // 同一个地址上的 rclone 被换掉了：数字作业号现在可能指向别人的作业。
                engine.release(&task_id);
                done.push(Finished {
                    task_id,
                    jobid,
                    status: rcgui_i18n::t("已中断：rclone 内核重启，结果未知").to_string(),
                });
                continue;
            }
            (None, Some(now)) => engine.set_execute_id(&task_id, now.clone()),
            _ => {}
        }
        let st: JobStatus = match serde_json::from_value(raw) {
            Ok(st) => st,
            Err(_) => {
                // 返回体不是我们认识的形状，同样算「这次没问到」。
                engine.set_waiting(&task_id, true);
                continue;
            }
        };
        if !st.finished {
            engine.set_waiting(&task_id, false);
            continue;
        }
        let outcome = st.outcome();
        engine.release(&task_id);
        done.push(Finished {
            task_id,
            jobid,
            status: if outcome.is_empty() {
                "ok".to_string()
            } else {
                outcome
            },
        });
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn task() -> SyncTask {
        SyncTask {
            id: "t1".into(),
            name: "照片".into(),
            local: "D:\\Photos".into(),
            remote: "gdrive:backup/photos".into(),
            direction: DIR_UP.into(),
            mode: MODE_COPY.into(),
            enabled: true,
            auto_sync: true,
            interval_hours: DEFAULT_HOURS,
            last_run: String::new(),
            last_status: String::new(),
        }
    }

    fn at(s: i64) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 3, 1, 12, 0, 0)
            .single()
            .unwrap()
            + ChronoDuration::seconds(s)
    }

    use chrono::TimeZone;

    #[test]
    fn direction_and_mode_mapping() {
        let mut t = task();
        assert!(is_up(&t));
        assert!(!is_mirror(&t));
        assert_eq!(ui_direction(&t), "local-to-remote");
        t.direction = DIR_DOWN.into();
        assert!(!is_up(&t));
        assert_eq!(ui_direction(&t), "remote-to-local");
        t.mode = MODE_SYNC.into();
        assert!(is_mirror(&t));
        // 空方向按上行（历史配置里 direction 就是空串）。
        t.direction = String::new();
        assert!(is_up(&t));
    }

    #[test]
    fn pair_swaps_with_direction() {
        let mut t = task();
        assert_eq!(pair(&t), ("D:\\Photos", "gdrive:backup/photos"));
        t.direction = DIR_DOWN.into();
        assert_eq!(pair(&t), ("gdrive:backup/photos", "D:\\Photos"));
    }

    #[test]
    fn validate_rejects_relative_local_and_bare_remote() {
        let mut t = task();
        assert!(validate(&t).is_ok());
        t.local = "Photos".into();
        assert!(validate(&t).is_err());
        let mut t = task();
        t.remote = "gdrive".into();
        assert!(validate(&t).is_err());
    }

    #[test]
    fn validate_rejects_unknown_direction_and_mode() {
        let mut t = task();
        // 写错的方向值不能被当成上行默默跑起来。
        t.direction = "upload".into();
        assert!(validate(&t).is_err());
        let mut t = task();
        t.mode = "mirror".into();
        assert!(validate(&t).is_err());
        let mut t = task();
        t.mode = MODE_SYNC.into();
        assert!(validate(&t).is_ok(), "显式镜像仍然是允许的");
    }

    #[test]
    fn validate_rejects_empty_id_and_huge_interval() {
        let mut t = task();
        t.id = " ".into();
        assert!(validate(&t).is_err());
        let mut t = task();
        t.interval_hours = MAX_HOURS + 1;
        assert!(validate(&t).is_err());
    }

    #[test]
    fn validate_rejects_same_source_and_recursion() {
        let mut t = task();
        // 远程路径抄成本地路径。
        t.remote = "gdrive:D:\\Photos".into();
        assert!(validate(&t).is_err());
        // 远程路径写成绝对路径且是本地子目录。
        let mut t = task();
        t.remote = "gdrive:D:/Photos/inner".into();
        assert!(validate(&t).is_err());
        // 正常的远程路径不能被误判。
        let mut t = task();
        t.remote = "gdrive:photos".into();
        assert!(validate(&t).is_ok());
    }

    #[test]
    fn split_remote_trims_leading_slashes() {
        assert_eq!(
            split_remote("gdrive:backup/photos").unwrap(),
            ("gdrive", "backup/photos")
        );
        assert_eq!(
            split_remote("gdrive:/backup/").unwrap(),
            ("gdrive", "backup/")
        );
        assert!(split_remote("gdrive").is_err());
        assert!(split_remote(":photos").is_err());
    }

    #[test]
    fn never_run_is_due_immediately() {
        let t = task();
        assert_eq!(due_at(&t, at(0)), Some(at(0)));
        assert!(is_due(&t, at(0)));
    }

    #[test]
    fn due_after_interval_only() {
        let mut t = task();
        t.last_run = at(0).to_rfc3339();
        assert_eq!(due_at(&t, at(60)), Some(at(21600)));
        assert!(!is_due(&t, at(21599)));
        assert!(is_due(&t, at(21600)));
    }

    #[test]
    fn disabled_is_never_due() {
        let mut t = task();
        t.enabled = false;
        assert_eq!(due_at(&t, at(0)), None);
        assert!(!is_due(&t, at(0)));
    }

    #[test]
    fn zero_interval_runs_once() {
        let mut t = task();
        t.interval_hours = 0;
        assert!(is_due(&t, at(0)));
        t.last_run = at(0).to_rfc3339();
        assert_eq!(due_at(&t, at(60)), None);
    }

    #[test]
    fn bad_last_run_counts_as_never() {
        let mut t = task();
        t.last_run = "2026/03/01 12:00".into();
        assert_eq!(due_at(&t, at(0)), Some(at(0)));
    }

    #[test]
    fn claim_prevents_double_start() {
        let e = Engine::new();
        assert!(e.claim("t1").is_ok());
        assert!(e.claim("t1").is_err());
        assert!(e.is_active("t1"));
        e.release("t1");
        assert!(!e.is_active("t1"));
        assert!(e.claim("t1").is_ok());
    }

    #[test]
    fn begin_adopts_reservation_but_not_running_job() {
        let e = Engine::new();
        // 调用方先 claim，start 必须能接手，否则这个任务永远起不来。
        e.claim("t1").unwrap();
        let c = e.begin("t1").expect("claim 之后的 begin 应该接手");
        c.commit(9);
        assert_eq!(e.active_of("t1").unwrap().jobid, 9);
        assert!(!e.is_active("t2"));
    }

    #[test]
    fn begin_refuses_job_already_submitted() {
        let e = Engine::new();
        e.begin("t1").unwrap().commit(3);
        assert!(e.begin("t1").is_err());
    }

    #[test]
    fn dropped_claim_releases_slot() {
        let e = Engine::new();
        {
            let _c = e.begin("t1").unwrap();
            assert!(e.is_active("t1"));
        }
        assert!(!e.is_active("t1"), "提交失败或被取消后必须能重试");
    }

    #[test]
    fn jobid_maps_back_to_task() {
        let e = Engine::new();
        e.begin("t1").unwrap().commit(77);
        assert_eq!(e.task_of(77).as_deref(), Some("t1"));
        assert_eq!(e.task_of(0), None);
        assert_eq!(e.task_of(78), None);
        assert_eq!(e.active_ids(), vec!["t1".to_string()]);
    }

    #[test]
    fn next_text_shows_waiting_then_time() {
        let mut t = task();
        t.last_run = at(0).to_rfc3339();
        assert_eq!(
            next_text(&t, at(0)),
            rcgui_i18n::f("{} 小时后", &[&(DEFAULT_HOURS as i64)]),
            "刚跑完，下一次在 6 小时后"
        );
        assert_eq!(next_text(&t, at(21600)), rcgui_i18n::t("等待执行"));
        let text = next_text(&t, at(21599));
        assert!(text.contains('1'), "一分钟内应该是 1 分钟后：{text}");
        t.enabled = false;
        assert_eq!(next_text(&t, at(0)), rcgui_i18n::t("已停用"));
    }

    // ---- 下面用本地假 RC 服务器验证轮询分类，不连真实 rclone ----

    struct Mock {
        base: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// 起一个只会照本宣科的 HTTP 服务：按请求路径挑一条 (状态码, 响应体)。
    async fn mock_rc<F>(handler: F) -> Mock
    where
        F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    let head_end = loop {
                        let n = match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = find(&buf, b"\r\n\r\n") {
                            break i + 4;
                        }
                        if buf.len() > 64 * 1024 {
                            return;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let path = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let want = head
                        .lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + want {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let (code, body) = handler(&path);
                    let resp = format!(
                        "HTTP/1.1 {code} MOCK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Mock {
            base: format!("http://{addr}"),
            task,
        }
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    #[tokio::test]
    async fn start_submits_copy_and_records_jobid() {
        let mock = mock_rc(|path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 12}).to_string())
            } else {
                (404, json!({"error": "find method"}).to_string())
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        let jobid = start(&rc, &e, &task()).await.unwrap();
        assert_eq!(jobid, 12);
        assert_eq!(e.active_of("t1").unwrap().jobid, 12);
        assert!(!e.is_waiting("t1"));
    }

    #[tokio::test]
    async fn start_releases_claim_when_rc_rejects() {
        let mock = mock_rc(|_| (500, json!({"error": "boom"}).to_string())).await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        let err = start(&rc, &e, &task()).await.unwrap_err();
        assert!(err.to_string().contains("boom"));
        // 关键：失败不能把任务永久卡在「运行中」。
        assert!(!e.is_active("t1"));
    }

    #[tokio::test]
    async fn start_works_after_caller_claimed_first() {
        let mock = mock_rc(|_| (200, json!({"jobid": 4}).to_string())).await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        e.claim("t1").unwrap();
        let jobid = start(&rc, &e, &task())
            .await
            .expect("先 claim 再 start 必须能启动");
        assert_eq!(jobid, 4);
    }

    #[tokio::test]
    async fn finished_job_is_reported_and_released() {
        let mock = mock_rc(|path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 5}).to_string())
            } else {
                (
                    200,
                    json!({"jobid": 5, "finished": true, "success": true, "error": ""}).to_string(),
                )
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        start(&rc, &e, &task()).await.unwrap();
        let done = poll(&rc, &e).await;
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].task_id, "t1");
        assert_eq!(done[0].status, "ok");
        assert!(!e.is_active("t1"));
    }

    #[tokio::test]
    async fn failed_job_reports_error_detail() {
        let mock = mock_rc(|path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 6}).to_string())
            } else {
                (
                    200,
                    json!({"jobid": 6, "finished": true, "success": false, "error": "disk full"})
                        .to_string(),
                )
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        start(&rc, &e, &task()).await.unwrap();
        let done = poll(&rc, &e).await;
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].status, "failed: disk full");
    }

    #[tokio::test]
    async fn transient_status_error_keeps_task_active() {
        // 服务端暂时出错（不是「作业不存在」）：任务必须留着，下一轮再问。
        let mock = mock_rc(|path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 7}).to_string())
            } else {
                (
                    500,
                    json!({"error": "connection reset by peer"}).to_string(),
                )
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        start(&rc, &e, &task()).await.unwrap();
        let done = poll(&rc, &e).await;
        assert!(done.is_empty(), "暂时性错误不能当成作业结束");
        assert!(e.is_active("t1"));
        assert!(e.is_waiting("t1"), "界面要能显示「等待重连」");
        assert_eq!(e.waiting_ids(), vec!["t1".to_string()]);
    }

    #[tokio::test]
    async fn unreachable_core_keeps_task_active() {
        // 端口上没人听：连接错误同样只是暂时问不到。
        let mock = mock_rc(|_| (200, json!({"jobid": 8}).to_string())).await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        start(&rc, &e, &task()).await.unwrap();
        drop(mock); // 服务消失
        let done = poll(&rc, &e).await;
        assert!(done.is_empty());
        assert!(e.is_active("t1"));
        assert!(e.is_waiting("t1"));
    }

    #[tokio::test]
    async fn job_not_found_ends_task() {
        let mock = mock_rc(|path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 9}).to_string())
            } else {
                (400, json!({"error": "job not found"}).to_string())
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        start(&rc, &e, &task()).await.unwrap();
        let done = poll(&rc, &e).await;
        assert_eq!(done.len(), 1, "明确的「没有这个作业」要结束任务");
        assert!(done[0].status.contains("job not found"));
        assert!(!e.is_active("t1"));
    }

    #[tokio::test]
    async fn running_job_stays_active_without_waiting_flag() {
        let mock = mock_rc(|path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 10}).to_string())
            } else {
                (200, json!({"jobid": 10, "finished": false}).to_string())
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        start(&rc, &e, &task()).await.unwrap();
        assert!(poll(&rc, &e).await.is_empty());
        assert!(e.is_active("t1"));
        assert!(!e.is_waiting("t1"));
    }

    #[tokio::test]
    async fn core_restart_interrupts_old_job_without_querying() {
        // 内核换代后不再拿旧 jobid 去查：万一数字被新作业占用，就会认错作业。
        let mock = mock_rc(|path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 11}).to_string())
            } else {
                (
                    200,
                    json!({"jobid": 11, "finished": true, "success": true}).to_string(),
                )
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        let first = e.on_core_started();
        start(&rc, &e, &task()).await.unwrap();
        let second = e.on_core_started();
        assert_ne!(first, second);
        let done = poll(&rc, &e).await;
        assert_eq!(done.len(), 1);
        assert!(done[0].status.contains("中断"), "{}", done[0].status);
        assert!(!e.is_active("t1"));
    }

    #[tokio::test]
    async fn jobid_from_previous_generation_is_not_reused() {
        // 同一代内核内 jobid 反查有效；换代后状态查询不会再指向新作业。
        let e = Engine::new();
        e.begin("t1").unwrap().commit(11);
        assert_eq!(e.task_of(11).as_deref(), Some("t1"));
        e.on_core_started();
        assert_eq!(e.generation(), 1);
    }

    #[tokio::test]
    async fn external_core_restart_is_caught_by_execute_id() {
        // 别人从外面把 rclone 换掉了：没人会调用 on_core_started，只有 executeId 变了。
        // 这时同一个数字可能已经是新内核里别人的作业，绝不能拿它当真结果。
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let mock = mock_rc(move |path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 21}).to_string())
            } else if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                // start 之后那一次：还是当初那个内核，作业还在跑
                (
                    200,
                    json!({"jobid": 21, "executeId": "abc", "finished": false}).to_string(),
                )
            } else {
                // 轮询时：另一个内核实例，而且 21 号作业是别人的、已经成功结束
                (
                    200,
                    json!({"jobid": 21, "executeId": "xyz", "finished": true, "success": true})
                        .to_string(),
                )
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        start(&rc, &e, &task()).await.unwrap();
        assert_eq!(e.execute_id_of("t1").as_deref(), Some("abc"));
        let done = poll(&rc, &e).await;
        assert_eq!(done.len(), 1);
        assert!(done[0].status.contains("中断"), "{}", done[0].status);
        assert!(!e.is_active("t1"));
    }

    #[tokio::test]
    async fn same_execute_id_keeps_job_active() {
        // 同一个内核实例、作业还在跑：照旧留在 Active，也不标「等待重连」。
        let mock = mock_rc(|path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 22}).to_string())
            } else {
                (
                    200,
                    json!({"jobid": 22, "executeId": "same", "finished": false}).to_string(),
                )
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        start(&rc, &e, &task()).await.unwrap();
        assert_eq!(e.execute_id_of("t1").as_deref(), Some("same"));
        assert!(poll(&rc, &e).await.is_empty());
        assert!(e.is_active("t1"));
        assert!(!e.is_waiting("t1"));
    }

    #[tokio::test]
    async fn missing_execute_id_is_adopted_on_first_successful_query() {
        // 提交后第一次没问到（旧版 rclone 或临时错误）：不能就此认定作业没了，
        // 之后问到了就把编号补上，后面的换代判断才有依据。
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let mock = mock_rc(move |path| {
            if path.contains("/sync/copy") {
                (200, json!({"jobid": 23}).to_string())
            } else if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                (404, json!({"error": "find method"}).to_string())
            } else {
                (
                    200,
                    json!({"jobid": 23, "executeId": "gen-1", "finished": false}).to_string(),
                )
            }
        })
        .await;
        let rc = RcClient::new(mock.base.clone(), None, None);
        let e = Engine::new();
        start(&rc, &e, &task()).await.unwrap();
        assert!(e.execute_id_of("t1").is_none(), "没问到就不该编一个");
        assert!(poll(&rc, &e).await.is_empty());
        assert_eq!(e.execute_id_of("t1").as_deref(), Some("gen-1"));
        assert!(e.is_active("t1"));
    }
}
