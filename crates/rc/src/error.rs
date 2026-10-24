//! RC 调用错误分类。
//!
//! 存在的理由：重构指南要求「连接错误、RC API 错误、WinFsp 缺失、盘符被占用、
//! 远端后端慢」分别报告，而通用 `anyhow!("失败: {e}")` 无法区分这些情况。
//! 这里只做**分类**，不改变调用方签名（`RcError` 可自动转成 `anyhow::Error`）。

use std::fmt;

/// 错误类别。UI 侧可据此给出不同文案与后续动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcErrorKind {
    /// 连不上端点：拒绝连接、端口不通、DNS 失败。
    Connect,
    /// 鉴权失败：401/403（用户名或密码不对）。
    Auth,
    /// 方法不存在 / 版本过旧：404 或 "unknown"/"not found"。
    Unsupported,
    /// rclone 明确返回了业务错误。
    Api,
    /// 响应不是合法 JSON，或缺少必要字段。
    Decode,
    /// 请求超时（端点存在但无响应，常见于后端卡住）。
    Timeout,
}

impl RcErrorKind {
    /// 简短中文标签，可直接进 toast / 状态栏。
    pub fn label(self) -> &'static str {
        match self {
            Self::Connect => "无法连接",
            Self::Auth => "鉴权失败",
            Self::Unsupported => "rclone 不支持该接口",
            Self::Api => "rclone 返回错误",
            Self::Decode => "响应无法解析",
            Self::Timeout => "请求超时",
        }
    }

    /// 给用户的下一步建议（不要与 WinFsp/盘符等无关原因混用）。
    pub fn hint(self) -> &'static str {
        match self {
            Self::Connect => "请确认 rclone 已用 --rc 启动，并检查地址与端口",
            Self::Auth => "请检查 RC 用户名与密码（--rc-user / --rc-pass）",
            Self::Unsupported => "请升级 rclone，或改用兼容模式",
            Self::Api => "请查看 rclone 日志并检查该远程的配置",
            Self::Decode => "可能是 rclone 版本不兼容或代理返回了异常内容",
            Self::Timeout => "端点无响应：远程后端可能较慢或已卡住",
        }
    }

    /// 供日志/诊断使用的稳定标识（不随文案变化）。
    pub fn id(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Auth => "auth",
            Self::Unsupported => "unsupported",
            Self::Api => "api",
            Self::Decode => "decode",
            Self::Timeout => "timeout",
        }
    }
}

/// 一次 RC 调用的失败原因。
///
/// 约定：`detail` **不得**包含 RC 密码、OAuth token 或完整 rclone.conf 内容。
#[derive(Debug, Clone)]
pub struct RcError {
    pub kind: RcErrorKind,
    /// 出错的 RC 方法名，如 `config/listremotes`。
    pub method: String,
    /// 面向用户的细节（可含 rclone 的 error 文本，不含凭据）。
    pub detail: String,
}

impl RcError {
    pub fn new(kind: RcErrorKind, method: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            method: method.into(),
            detail: detail.into(),
        }
    }

    /// 方法不存在（用于新旧方法名回退，如 `operations/copy` → `operations/copyfile`）。
    pub fn is_unsupported(&self) -> bool {
        self.kind == RcErrorKind::Unsupported
    }

    /// 端点完全不可达（离线模式的判定依据）。
    pub fn is_unreachable(&self) -> bool {
        matches!(self.kind, RcErrorKind::Connect | RcErrorKind::Timeout)
    }

    /// 是「对端**没有配置** RC 鉴权」，而不是「我们的凭据不对」。
    ///
    /// rclone 的 rcserver 只对注册为 `NoAuth` 的接口放行；服务端没配置鉴权时，
    /// 其余接口返回 **403** + 固定文案 `authentication must be set up on the rc server
    /// to use "..." or the --rc-no-auth flag must be in use`。凭据错误的形态是
    /// HTTP 层的 **401**。rclone 没有用状态码区分这两种情况，所以判定必须看文案。
    ///
    /// 用途：握手时区分「连上了但没法用」（403）与「密码打错了」（401），
    /// 前者要明确告诉用户怎么重启 rclone，后者要提示去改凭据。
    pub fn is_auth_not_configured(&self) -> bool {
        self.kind == RcErrorKind::Auth
            && self
                .detail
                .to_ascii_lowercase()
                .contains("authentication must be set up")
    }

    /// 单行完整描述，形如 `config/listremotes: rclone 返回错误: …`。
    pub fn describe(&self) -> String {
        format!("{}: {}（{}）", self.method, self.detail, self.kind.label())
    }
}

impl fmt::Display for RcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detail.is_empty() {
            write!(f, "{}", self.method)
        } else {
            write!(f, "{}: {}", self.method, self.detail)
        }
    }
}

impl std::error::Error for RcError {}

// 注：anyhow 提供了 `impl<E: StdError + Send + Sync + 'static> From<E> for anyhow::Error`
// 的通用实现，`RcError` 全部字段为 `String`，自然满足，因此旧调用点里的 `?`
// 无需任何改动即可把 `RcError` 转成 `anyhow::Error`。这里不要手写 `From`，会冲突。

pub type RcResult<T> = Result<T, RcError>;

/// 由 HTTP 状态码与 rclone 的 error 文本判定类别。
///
/// rclone 对不存在的方法返回 404（部分版本返回 400 + "not found"），
/// 旧版本对重命名过的方法（`operations/copy` 等）也用这两类形态，故合并判定。
pub(crate) fn classify_response(status: u16, message: &str) -> RcErrorKind {
    if status == 401 || status == 403 {
        return RcErrorKind::Auth;
    }
    let lower = message.to_ascii_lowercase();
    // 只认「方法本身不存在」的签名：rclone 的文案是
    // `couldn't find method "operations/cat"`（404）。
    // 以前把任何 `not found` / `doesn't exist` / `no such` 都算进这里，
    // 结果 `object not found`（源文件不存在）被误判成「不支持」，
    // 调用方于是回退到旧方法名，最终报出与真因无关的错误（实测踩过）。
    let looks_unknown = lower.contains("find method")
        || lower.contains("unknown method")
        || lower.contains("method not found")
        // 只有「404 + 空正文」（rclone 对未注册的 rc 路径就是这样回的）才算方法缺失；
        // 「404 + object not found」是资源缺失，必须留在 Api 里。
        || (status == 404 && lower.trim().is_empty());
    if looks_unknown {
        return RcErrorKind::Unsupported;
    }
    RcErrorKind::Api
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_statuses_are_auth() {
        assert_eq!(classify_response(401, "x"), RcErrorKind::Auth);
        assert_eq!(classify_response(403, "x"), RcErrorKind::Auth);
    }

    #[test]
    fn missing_method_is_unsupported() {
        assert_eq!(classify_response(404, ""), RcErrorKind::Unsupported);
        assert_eq!(
            classify_response(400, "couldn't find method \"operations/copy\""),
            RcErrorKind::Unsupported
        );
    }

    /// rclone 的「源文件/目录不存在」也是 404，但那是资源缺失，不是方法缺失：
    /// 不能触发旧方法名回退，否则最终报错会被回退那次的 404 覆盖（实测踩过）。
    #[test]
    fn resource_errors_are_api_not_unsupported() {
        assert_eq!(classify_response(404, "object not found"), RcErrorKind::Api);
        assert_eq!(classify_response(500, "no such remote"), RcErrorKind::Api);
        assert_eq!(classify_response(404, "directory not found"), RcErrorKind::Api);
    }

    #[test]
    fn other_errors_are_api() {
        assert_eq!(classify_response(500, "backend error"), RcErrorKind::Api);
        assert_eq!(classify_response(200, "rate limited"), RcErrorKind::Api);
    }

    #[test]
    fn describe_includes_method_and_label() {
        let e = RcError::new(RcErrorKind::Connect, "core/version", "refused");
        assert!(e.describe().contains("core/version"));
        assert!(e.describe().contains("无法连接"));
        assert!(e.is_unreachable());
        assert!(!e.is_unsupported());
    }

    /// 「对端没配鉴权」(403 + 固定文案) 必须与「凭据不对」(401) 分开。
    #[test]
    fn detects_server_without_auth() {
        let real = RcError::new(
            RcErrorKind::Auth,
            "options/get",
            "authentication must be set up on the rc server to use \"options/get\" \
             or the --rc-no-auth flag must be in use",
        );
        assert!(real.is_auth_not_configured());

        // 401（密码错）：类别相同，但绝不能判成「服务端没配鉴权」
        let bad_creds = RcError::new(RcErrorKind::Auth, "rc/noopauth", "HTTP 401");
        assert!(!bad_creds.is_auth_not_configured());

        // 其它类别的错误即使文案撞上了也不算
        let other = RcError::new(RcErrorKind::Api, "rc/noopauth", "authentication must be set up");
        assert!(!other.is_auth_not_configured());
    }
}
