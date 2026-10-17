//! 连接状态：`当前连到哪个端点` + `是否连上`。
//!
//! 这个模块是「解耦」的核心接缝：进程管理器（`rclone_client::proc`）在拉起/连接
//! rclone 之后只做一件事 —— 把可用的 `RcClient` 发布到这里；而 UI 层不再关心
//! 客户端是怎么来的（自己拉起、还是用户已经在跑的实例），只读这里。
//!
//! 阶段 2 的约束：**本模块不启动、不终止、不探测任何进程**，也不扫描端口。

use crate::client::RcClient;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// 用户显式选择的 RC 端点。
///
/// 只记录「连到哪里、用什么用户名、有没有密码」，**不保存密码明文**：
/// 真正的凭据留在 `RcClient` 内部，避免在状态快照、日志、UI 绑定里扩散。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// 形如 `http://127.0.0.1:5572`（不含尾部斜杠）。
    pub url: String,
    pub user: Option<String>,
    /// 是否配置了密码（仅布尔，取不到明文值）。
    pub has_pass: bool,
}

impl Endpoint {
    /// 推荐默认值。只作为**建议**呈现，不自动探测、不自动连接。
    pub fn loopback_default() -> Self {
        Self {
            url: "http://127.0.0.1:5572".to_string(),
            user: None,
            has_pass: false,
        }
    }

    /// 从已构造的客户端反推端点信息（用于「刚连上」时记录）。
    pub fn from_client(c: &RcClient) -> Self {
        Self {
            url: c.endpoint().to_string(),
            user: c.auth_user().map(|s| s.to_string()),
            has_pass: c.has_auth(),
        }
    }

    /// 面向 UI 的安全展示串：只显示地址与用户名，绝不含密码。
    pub fn display(&self) -> String {
        match &self.user {
            Some(u) if !u.is_empty() => format!("{}（用户 {u}{}）", self.url, if self.has_pass { "，含密码" } else { "" }),
            _ => self.url.clone(),
        }
    }
}

/// 连接状态机。
///
/// - `Disconnected`：从未连上，或用户主动断开。
/// - `Connected`：握手成功，`RcState::client()` 指向活端点。
/// - `Stale`：曾经连上、现在断开。保留上一次快照以便离线查看，但**必须**在 UI 标记为陈旧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStatus {
    Disconnected,
    Connected,
    Stale,
}

impl ConnStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "未连接",
            Self::Connected => "已连接",
            Self::Stale => "已断开（显示的是上次快照）",
        }
    }

    /// 是否可以认为读到的数据是「运行中」的实时状态。
    pub fn is_live(self) -> bool {
        matches!(self, Self::Connected)
    }
}

struct Inner {
    client: Arc<RcClient>,
    endpoint: Endpoint,
    status: ConnStatus,
    /// 最近一次连接失败原因（已脱敏）。连上后清空。
    last_error: Option<String>,
    connected_at: Option<Instant>,
}

/// 全局唯一的连接状态持有者（由 app 层以 `Arc` 共享）。
pub struct RcState {
    inner: RwLock<Inner>,
    /// `Connected` 的同步镜像：让轮询热路径不做 `await` 也能判断「该不该发请求」。
    live: std::sync::atomic::AtomicBool,
}

impl Default for RcState {
    fn default() -> Self {
        Self::new()
    }
}

impl RcState {
    /// 初始为未连接：客户端指向默认地址但没有任何凭据，调用会以连接/鉴权错误失败，
    /// 这与旧实现「先给一个占位客户端」的语义一致（阶段 2 不改变现有行为）。
    pub fn new() -> Self {
        let endpoint = Endpoint::loopback_default();
        let client = Arc::new(RcClient::new(endpoint.url.clone(), None, None));
        Self {
            inner: RwLock::new(Inner {
                client,
                endpoint,
                status: ConnStatus::Disconnected,
                last_error: None,
                connected_at: None,
            }),
            live: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 同步判断是否处于已连接状态（供轮询门控使用，不发请求、不 await）。
    pub fn is_live(&self) -> bool {
        self.live.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 取当前客户端。**不返回 Option**：调用方无需改写成 `if let`（阶段 2 保持行为一致），
    /// 是否真的连上由 `status()` 判断。
    pub async fn client(&self) -> Arc<RcClient> {
        self.inner.read().await.client.clone()
    }

    pub async fn status(&self) -> ConnStatus {
        self.inner.read().await.status
    }

    pub async fn endpoint(&self) -> Endpoint {
        self.inner.read().await.endpoint.clone()
    }

    pub async fn last_error(&self) -> Option<String> {
        self.inner.read().await.last_error.clone()
    }

    /// 连接成功后发布新客户端。`connected_at` 用于「上次刷新时间」展示。
    pub async fn set_connected(&self, client: Arc<RcClient>) {
        let mut g = self.inner.write().await;
        g.endpoint = Endpoint::from_client(&client);
        g.client = client;
        g.status = ConnStatus::Connected;
        g.last_error = None;
        g.connected_at = Some(Instant::now());
        self.live.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 断开连接。**不丢弃**客户端与快照：离线时仍可读上次数据（并标注 Stale）。
    pub async fn set_disconnected(&self, err: Option<String>) {
        let mut g = self.inner.write().await;
        g.status = match g.status {
            // 连上过 → 保留数据但标记陈旧
            ConnStatus::Connected => ConnStatus::Stale,
            other => other,
        };
        g.last_error = err;
        g.connected_at = None;
        self.live.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// 已连接时长（未连接为 None）。
    pub async fn connected_for(&self) -> Option<Duration> {
        self.inner.read().await.connected_at.map(|t| t.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn starts_disconnected() {
        let st = RcState::new();
        assert_eq!(st.status().await, ConnStatus::Disconnected);
        assert!(st.last_error().await.is_none());
        assert!(st.connected_for().await.is_none());
    }

    #[tokio::test]
    async fn connect_then_disconnect_keeps_client_as_stale() {
        let st = RcState::new();
        let c = Arc::new(RcClient::new("http://127.0.0.1:6000", Some("u".into()), Some("p".into())));
        st.set_connected(c.clone()).await;
        assert_eq!(st.status().await, ConnStatus::Connected);
        assert!(st.status().await.is_live());
        assert_eq!(st.endpoint().await.url, "http://127.0.0.1:6000");
        assert!(st.endpoint().await.has_pass);

        st.set_disconnected(Some("connection refused".into())).await;
        assert_eq!(st.status().await, ConnStatus::Stale);
        // 端点与客户端保留，离线仍可读快照
        assert_eq!(st.client().await.endpoint(), "http://127.0.0.1:6000");
        assert_eq!(st.last_error().await.as_deref(), Some("connection refused"));
    }

    #[tokio::test]
    async fn disconnect_without_prior_connect_stays_disconnected() {
        let st = RcState::new();
        st.set_disconnected(Some("refused".into())).await;
        assert_eq!(st.status().await, ConnStatus::Disconnected);
    }

    #[test]
    fn endpoint_display_never_contains_password() {
        let e = Endpoint {
            url: "http://127.0.0.1:5572".into(),
            user: Some("gui".into()),
            has_pass: true,
        };
        let s = e.display();
        assert!(s.contains("127.0.0.1:5572"));
        assert!(s.contains("gui"));
        assert!(s.contains("含密码"));
    }
}
