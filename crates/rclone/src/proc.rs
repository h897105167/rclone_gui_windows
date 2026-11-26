//! rclone 集成层的**非进程**部分：探测工具 + 连接发布。
//!
//! 阶段 7 清理结果：本模块已**不再包含任何进程启动/终止代码**。
//! RCGUI 不拥有 rclone 的生命周期（不启动、不停止、不重启），因此原来的
//! `start_managed` / `stop` / `is_managed_running` / `cat_to_file` /
//! `open_rclone_log` / `pick_free_port` 全部删除。
//!
//! 图片直链回源已改为 `rcgui_rc::fetch_to_temp`（走 RC `operations/copyfile`），
//! 因此这里连「派生一个短命 rclone 进程」的路径也不存在了。

use anyhow::anyhow;
use rcgui_rc::{RcClient, RcState, VersionInfo};
use std::path::PathBuf;
use std::sync::Arc;

/// 在文本行中提取 Windows 绝对路径（X:\ 或 X:/ 开头）。
pub fn extract_windows_path(line: &str) -> Option<String> {
    let b = line.as_bytes();
    if b.len() < 3 {
        return None;
    }
    for i in 0..b.len() - 2 {
        if b[i].is_ascii_alphabetic() && b[i + 1] == b':' && (b[i + 2] == b'\\' || b[i + 2] == b'/')
        {
            return Some(line[i..].trim().trim_matches(['"', '\'']).to_string());
        }
    }
    None
}

/// 生成随机密码（用于 serve 等需要口令的场景，不用于 RC 连接）。
pub fn generate_password() -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..24)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

/// 探测 rclone.exe（自定义路径 → PATH → 常见安装位置）。
///
/// 仅用于**提示与诊断**：RCGUI 不会执行它来启动进程。
pub fn discover_exe(custom: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(p) = custom {
        if !p.trim().is_empty() {
            let pb = PathBuf::from(p.trim());
            if pb.exists() {
                return Ok(pb);
            }
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("rclone.exe");
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let candidates = [
        format!(r"{local}\Rclone\rclone.exe"),
        r"C:\rclone\rclone.exe".to_string(),
        r"C:\Program Files\Rclone\rclone.exe".to_string(),
        format!(r"{local}\Microsoft\WinGet\Links\rclone.exe"),
        format!(r"{local}\scoop\shims\rclone.exe"),
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "{}",
        rcgui_i18n::t("未找到 rclone.exe，请确认已安装并加入 PATH")
    ))
}

/// 连接发布器：只负责「把可用的客户端放到共享连接状态里」。
///
/// 没有子进程字段、没有日志文件参数——它已经不含任何生命周期管理。
pub struct RcloneManager {
    rc: Arc<RcState>,
}

impl RcloneManager {
    pub fn new(rc: Arc<RcState>) -> Self {
        Self { rc }
    }

    /// 共享连接状态（诊断用）。
    pub fn state(&self) -> &Arc<RcState> {
        &self.rc
    }

    /// 连接已有实例（验证可达）。成功后把客户端发布到共享连接状态。
    ///
    /// 这是 RCGUI 唯一允许的「建立连接」方式：连用户指定的端点，不启动任何进程。
    pub async fn attach(
        &self,
        url: &str,
        user: Option<String>,
        pass: Option<String>,
    ) -> anyhow::Result<VersionInfo> {
        let client = Arc::new(RcClient::new(url, user, pass));
        let info = match client.version().await {
            Ok(v) => v,
            Err(e) => {
                self.rc.set_disconnected(Some(e.describe())).await;
                return Err(anyhow!(
                    "{}",
                    rcgui_i18n::f("连接已有 rclone 失败: {}", &[&(e)])
                ));
            }
        };
        self.rc.set_connected(client).await;
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_windows_path() {
        assert_eq!(
            extract_windows_path("Configuration file is C:\\Users\\me\\rclone.conf"),
            Some("C:\\Users\\me\\rclone.conf".to_string())
        );
        assert_eq!(
            extract_windows_path(r#"path: "D:\data\cfg.conf""#),
            Some("D:\\data\\cfg.conf".to_string())
        );
        assert_eq!(extract_windows_path("no path here"), None);
        assert_eq!(extract_windows_path("ab"), None);
    }

    #[test]
    fn password_is_reasonable() {
        let a = generate_password();
        let b = generate_password();
        assert_eq!(a.len(), 24);
        assert_eq!(b.len(), 24);
        assert_ne!(a, b, "两次生成不应相同");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    /// 只答 `core/version` 的假 RC 服务器，够 `attach` 与握手验证可达。
    ///
    /// 端口交给系统分配：真机上每次启动内核的端口都是新的随机值，任何写死端口
    /// 的断言都只是把「新内核上仍显示旧端口」这个 bug 固化下来。
    /// `version` 由调用方给，这样测试能区分增强版（`v1.76.0-extra`）与原版。
    fn spawn_fake_rc(version: &str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let version = version.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut sock) = stream else { break };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let body = format!(r#"{{"version":"{version}","decomposed":[1,76,0]}}"#);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    /// 一个已经没人监听的地址：换内核之后旧端口就是这个样子。
    fn dead_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        format!("http://{addr}")
    }

    /// 换内核之后必须落到新端口上，而不是继续指着旧端口。
    ///
    /// 走真实路径：`RcloneManager::attach` 真的发 HTTP 请求，`RcState` 真的改状态。
    /// 旧内核死掉、新内核起来时依次是三步：
    /// 1. 连一个死地址失败**不能**抹掉地址 —— 外部实例靠这条保留上次快照；
    /// 2. 停掉自家内核时要 `forget`，否则界面永远显示那个已经消失的端口；
    /// 3. 再 attach 必须用新端点和新的随机凭据。
    #[tokio::test]
    async fn reconnect_after_a_core_swap_targets_the_new_endpoint() {
        let rc = Arc::new(RcState::new());
        let mgr = RcloneManager::new(rc.clone());

        let first = spawn_fake_rc("v1.76.0");
        mgr.attach(&first, Some("old-user".into()), Some("old-pass".into()))
            .await
            .expect("第一个实例应该连得上");
        assert_eq!(rc.endpoint().await.url, first);
        assert!(rc.is_live());

        let gone = dead_endpoint();
        let err = mgr
            .attach(&gone, Some("old-user".into()), Some("old-pass".into()))
            .await
            .expect_err("没人监听时必须报连接失败");
        assert!(err.to_string().contains("连接已有 rclone 失败"), "{err}");
        assert_eq!(
            rc.endpoint().await.url,
            first,
            "连不上不该抹掉上一次的地址（外部实例要保留快照）"
        );
        assert!(!rc.is_live());

        // 我们亲手停掉的自家内核：端口随进程没了，忘掉它。
        rc.forget().await;
        assert_eq!(rc.status().await, rcgui_rc::ConnStatus::Disconnected);
        assert_ne!(rc.endpoint().await.url, first);

        let second = spawn_fake_rc("v1.76.0");
        mgr.attach(&second, Some("new-user".into()), Some("new-pass".into()))
            .await
            .expect("新实例应该连得上");
        let ep = rc.endpoint().await;
        assert_eq!(ep.url, second, "必须指向新端口");
        assert_ne!(ep.url, first);
        assert_eq!(ep.user.as_deref(), Some("new-user"));
        assert!(ep.has_pass, "新凭据要跟着过来");
        assert!(rc.is_live());
    }

    /// 握手报出的版本必须来自当前连着的那个进程。
    ///
    /// 换渠道时两个内核的版本号只差后缀（`v1.76.0-extra` / `v1.76.0`）：版本号若来自
    /// 缓存、或来自上一次的端点，界面就会在换了内核之后继续显示旧渠道——「端口、进程、
    /// 状态都换成新的，只有版本号还写 v1.76.0-extra」正是这种错位。
    #[tokio::test]
    async fn handshake_version_comes_from_the_endpoint_it_is_connected_to() {
        let rc = Arc::new(RcState::new());
        let mgr = RcloneManager::new(rc.clone());

        let old = spawn_fake_rc("v1.76.0-extra");
        mgr.attach(&old, Some("u1".into()), Some("p1".into()))
            .await
            .expect("旧内核应该连得上");
        let before_client = rc.client().await;
        let before = rcgui_rc::Handshake::perform(before_client.as_ref())
            .await
            .expect("握手");
        assert_eq!(before.version.version, "v1.76.0-extra");

        rc.forget().await;
        let new = spawn_fake_rc("v1.76.0");
        mgr.attach(&new, Some("u2".into()), Some("p2".into()))
            .await
            .expect("新内核应该连得上");
        let after_client = rc.client().await;
        let after = rcgui_rc::Handshake::perform(after_client.as_ref())
            .await
            .expect("握手");
        assert_eq!(
            after.version.version, "v1.76.0",
            "换了内核之后握手必须报新端点上的版本，不能沿用上一个"
        );
    }

    #[test]
    fn crate_has_no_process_spawning() {
        // 防回归：本文件（以及整个 rclone_client）不应再出现进程派生/终止。
        // 用源码自检代替运行时断言——这是「RCGUI 不拥有 rclone 生命周期」的硬约束。
        let all = include_str!("proc.rs");
        // 只检查测试模块之前的实现代码，避免匹配到本测试自身的字符串字面量
        let src = &all[..all.find("#[cfg(test)]").unwrap_or(all.len())];
        for banned in [
            "Command::new",
            "std::process::Command",
            "tokio::process",
            "child.kill",
        ] {
            assert!(
                !src.contains(banned),
                "rclone_client 不应包含进程管理代码：{banned}"
            );
        }
    }
}
