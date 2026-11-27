//! 换内核的那套顺序：先停、再写配置、再起，起不来就退回去。
//!
//! 这里只写顺序和判断，不碰进程、不碰磁盘——那两件事由调用方通过 [`Backend`] 提供。
//! 分开的唯一理由是能测：换内核真正的难点不是「怎么起一个进程」（`core` 里已经有了），
//! 而是「起不来的时候谁负责把用户放回一个能用的状态」。这条责任链用假后端跑一遍
//! 就能看清，不需要真的杀掉一个 rclone。
//!
//! 三条硬规矩，违反任何一条都会让用户觉得「我点了个按钮，程序就废了」：
//!
//! 1. 只停我们自己拉起来的那个进程（`core::Running::stop` 只杀自己的 pid 句柄）。
//!    用户自己开着的 rclone 不在这个句柄里，所以永远不会被这里误杀。
//! 2. 写配置之前先记下旧值。写失败、起不来，都要按旧值退回去——不能留下「配置写着 A、
//!    跑着的什么都没有」这种状态。
//! 3. 退回去之后要把旧内核**重新起起来**。旧配置是对的，就是那一轮没跑而已。

/// 换内核之前的配置快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// `core.mode`，`"managed"` 或 `"external"`
    pub mode: String,
    /// `core.path`，空串表示交给程序去找
    pub path: String,
}

/// 一次换内核的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 新内核起来了，而且握手成功
    Connected,
    /// 新内核没成，旧的已经退回去并重新连上
    KeptOld,
    /// 新旧都没起来：现在是「内核不在」的状态，附带最后一次失败的原因
    Degraded(String),
}

/// 换内核要动手的那几件事。实现方负责真实的进程与配置文件。
#[async_trait::async_trait]
pub trait Backend: Send {
    /// 停掉我们自己拉起来的内核。没有在跑就什么都不做。
    fn stop(&mut self);

    /// 现在有没有我们自己拉起来的内核在跑。
    fn running(&mut self) -> bool;

    /// 把「用哪个内核」写进配置。写盘失败要如实返回错误。
    async fn commit(&mut self, path: &str) -> Result<(), String>;

    /// 按当前配置启动一次，返回启动说明（这句话**不参与**成败判断）。
    async fn start(&mut self) -> String;

    /// 启动之后，内核进程在不在。
    fn alive(&mut self) -> bool;

    /// 握手成不成。`alive()` 为假时不会被调用。
    async fn live(&mut self) -> bool;

    /// 退回旧配置。
    async fn restore(&mut self, before: Snapshot) -> Result<(), String>;
}

/// 按顺序换一次内核，返回最终该跟用户说什么。
///
/// `want` 是用户选中的内核路径；空串表示「交回托管、由程序自己找」。
pub async fn run(backend: &mut dyn Backend, before: Snapshot, want: &str) -> Verdict {
    let was_running = backend.running();
    if was_running {
        backend.stop();
    }

    if let Err(e) = backend.commit(want).await {
        // 配置都没写下去：把内存里的旧值放回去，旧内核按旧配置重新起。
        let restored = backend.restore(before.clone()).await;
        if was_running {
            backend.start().await;
        }
        return Verdict::Degraded(match restored {
            Ok(()) => format!("保存内核设置失败：{e}"),
            Err(again) => format!("保存内核设置失败：{e}；退回旧设置也失败：{again}"),
        });
    }

    backend.start().await;
    if backend.alive() && backend.live().await {
        return Verdict::Connected;
    }

    // 新内核没成。退回旧配置再起一次——这一步是「失败时保留一个能用的状态」的全部内容。
    let restore_error = backend.restore(before).await.err();
    backend.start().await;
    let back = backend.alive() && backend.live().await;
    match (back, restore_error) {
        (true, None) => Verdict::KeptOld,
        (true, Some(e)) => Verdict::KeptOld.with_note(e),
        (false, None) => Verdict::Degraded("新内核没起来，旧内核也没能恢复".to_string()),
        (false, Some(e)) => Verdict::Degraded(format!("新内核没起来，旧内核也没能恢复：{e}")),
    }
}

impl Verdict {
    /// 旧内核回来了、但配置没退干净：仍然算「退回去了」，只是要把这件事说出来。
    fn with_note(self, note: String) -> Self {
        match self {
            Verdict::KeptOld => Verdict::Degraded(format!(
                "旧内核已经重新连上，但配置没退回原样：{note}。请再点一次「保存」。"
            )),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 记下调用顺序的假后端。
    struct Fake {
        log: Arc<Mutex<Vec<String>>>,
        was_running: bool,
        commit_ok: bool,
        /// 每次 `start()` 依次取一个值：true = 起来了且握手成功
        starts: Vec<bool>,
        alive: bool,
        live: bool,
        restore_ok: bool,
    }

    impl Fake {
        fn new(was_running: bool, starts: Vec<bool>) -> Self {
            Self {
                log: Arc::new(Mutex::new(Vec::new())),
                was_running,
                commit_ok: true,
                starts,
                alive: false,
                live: false,
                restore_ok: true,
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Backend for Fake {
        fn stop(&mut self) {
            self.log.lock().unwrap().push("stop".into());
            self.alive = false;
        }

        fn running(&mut self) -> bool {
            self.was_running
        }

        async fn commit(&mut self, path: &str) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("commit:{path}"));
            if self.commit_ok {
                Ok(())
            } else {
                Err("磁盘满了".into())
            }
        }

        async fn start(&mut self) -> String {
            self.log.lock().unwrap().push("start".into());
            let ok = if self.starts.is_empty() {
                false
            } else {
                self.starts.remove(0)
            };
            self.alive = ok;
            self.live = ok;
            String::new()
        }

        fn alive(&mut self) -> bool {
            self.alive
        }

        async fn live(&mut self) -> bool {
            self.live
        }

        async fn restore(&mut self, before: Snapshot) -> Result<(), String> {
            self.log
                .lock()
                .unwrap()
                .push(format!("restore:{}|{}", before.mode, before.path));
            if self.restore_ok {
                Ok(())
            } else {
                Err("写不进去".into())
            }
        }
    }

    fn before() -> Snapshot {
        Snapshot {
            mode: "managed".into(),
            path: String::new(),
        }
    }

    /// 顺利的路：停旧的、写配置、起新的，一步不多。
    #[tokio::test]
    async fn a_working_kernel_is_swapped_in_after_stopping_the_old_one() {
        let mut f = Fake::new(true, vec![true]);
        assert_eq!(
            run(&mut f, before(), "D:\\rclone\\rclone.exe").await,
            Verdict::Connected
        );
        assert_eq!(
            f.log(),
            vec!["stop", "commit:D:\\rclone\\rclone.exe", "start"],
            "顺序错了就会出现「两个内核同时占着一个端口」或者「停了老的、新的没起」"
        );
    }

    /// 新内核起不来：要把旧配置退回去，并且真的把旧内核再起一次。
    #[tokio::test]
    async fn a_dead_new_kernel_puts_the_old_one_back() {
        let mut f = Fake::new(true, vec![false, true]);
        assert_eq!(
            run(&mut f, before(), "D:\\bad\\rclone.exe").await,
            Verdict::KeptOld
        );
        assert_eq!(
            f.log(),
            vec![
                "stop",
                "commit:D:\\bad\\rclone.exe",
                "start",
                "restore:managed|",
                "start"
            ],
            "旧内核必须被重新启动：只写回配置，用户看到的是「内核不见了」"
        );
    }

    /// 配置压根写不下去：连新内核都不该起，直接退回旧状态。
    #[tokio::test]
    async fn a_config_that_will_not_save_never_starts_the_new_kernel() {
        let mut f = Fake::new(true, vec![true]);
        f.commit_ok = false;
        match run(&mut f, before(), "D:\\x\\rclone.exe").await {
            Verdict::Degraded(why) => assert!(why.contains("保存内核设置失败"), "{why}"),
            other => panic!("写盘失败不该报成 {other:?}"),
        }
        assert_eq!(
            f.log(),
            vec![
                "stop",
                "commit:D:\\x\\rclone.exe",
                "restore:managed|",
                "start"
            ],
            "写盘失败的路径上不能出现「启动新内核」"
        );
    }

    /// 新旧都起不来：如实报成「内核不在」，并带上原因。
    #[tokio::test]
    async fn when_neither_kernel_starts_the_verdict_says_so() {
        let mut f = Fake::new(true, vec![false, false]);
        match run(&mut f, before(), "").await {
            Verdict::Degraded(why) => assert!(why.contains("旧内核也没能恢复"), "{why}"),
            other => panic!("两个都没起来，不能报成 {other:?}"),
        }
    }

    /// 本来就没在跑（用户刚打开程序）：失败了没有旧内核可恢复，不该凭空起一个。
    #[tokio::test]
    async fn nothing_running_means_nothing_to_restart() {
        let mut f = Fake::new(false, vec![true, true]);
        f.commit_ok = false;
        assert!(matches!(
            run(&mut f, before(), "D:\\x\\rclone.exe").await,
            Verdict::Degraded(_)
        ));
        assert_eq!(
            f.log(),
            vec!["commit:D:\\x\\rclone.exe", "restore:managed|"],
            "没在跑的时候不该出现 stop/start：那是凭空的副作用"
        );
    }

    /// 旧内核回来了但配置没退干净：仍然报「退回去了」，同时要求用户再点一次保存。
    #[tokio::test]
    async fn a_half_rollback_is_reported_rather_than_hidden() {
        let mut f = Fake::new(true, vec![false, true]);
        f.restore_ok = false;
        match run(&mut f, before(), "D:\\bad\\rclone.exe").await {
            Verdict::Degraded(why) => {
                assert!(why.contains("旧内核已经重新连上"), "{why}");
                assert!(why.contains("再点一次"), "{why}");
            }
            other => panic!("配置没退回原样，不能说成 {other:?}"),
        }
    }
}
