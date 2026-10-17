//! remote 级代理配置模型。
//!
//! 定位（用户需求）：**按 remote 分流**，不是按文件、域名或单次请求分流。
//!
//! 实测约束（见 `docs/proxy-verification.md`，rclone v1.75.4）：
//!
//! 1. rclone 只认 `override.http_proxy` **一个**键，由 Go `http.Transport` 按 URL
//!    scheme 决定走 HTTP 还是 SOCKS5。`override.socks_proxy` 会被写进 conf 但
//!    **完全不生效**，因此本模块只生成 `override.http_proxy`。
//! 2. rclone 对代理 URL **不做任何校验**：非法字符串照单全收，未知 scheme 会被静默
//!    当作 HTTP 代理使用。所以校验必须在这里做完，不能指望 rclone 报错。
//! 3. 代理账号密码以**百分号编码的明文**落 rclone.conf，不是 obscure、更不是加密。
//!
//! 设计取舍：模型里把 `protocol / host / port / user / pass` 拆开存，不存拼好的 URL。
//! 这样「改端口」不会因为要把旧 URL 解析回来而引入解析歧义，也让 UI 表单与模型一一对应；
//! URL 只在 `to_proxy_url()` 里构造。
//!
//! 关于「用 URL 库还是手写编码」：本模块**没有**引入 `url` crate，原因是实测发现
//! `url` 会规范化掉协议默认端口（http 的 80、https 的 443）并给 special scheme 补 `/`，
//! 导致「用户填了 80 端口」的配置在 rclone.conf 里变成不带端口的形态。这对功能无害，
//! 但用户会以为自己的端口被丢掉了。因此这里只用 `url` 的等价语义做**纯字符串编码**：
//! 按 RFC 3986 的 unreserved 集合（`A-Za-z0-9-._~`）之外全部百分号编码，
//! 覆盖 `@ : / ? # % 空格` 等所有会破坏 URL 结构的字符，行为可预测且与 Go 侧一致。

use serde::{Deserialize, Serialize};

/// 受支持的代理协议。
///
/// 白名单是硬编码的，并且**与 rclone 实际能力一一对应**（实测）：
/// - `http` / `https` → 标准 HTTP 代理（https 指"到代理本身是 TLS"）
/// - `socks5` / `socks5h` → SOCKS5；区别在 `socks5h` 由代理解析域名
///
/// 不在这四项里的 scheme 一律拒绝：实测 `ftp-proxy://` 之类的未知 scheme 会被
/// rclone 静默当成 HTTP 代理，等于「假装配上了」，比直接报错更危险。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyProtocol {
    Http,
    Https,
    Socks5,
    /// SOCKS5 + 由代理解析主机名（远程 DNS）
    Socks5h,
}

impl Default for ProxyProtocol {
    fn default() -> Self {
        Self::Http
    }
}

impl ProxyProtocol {
    /// URL scheme。写进 `override.http_proxy` 的前缀。
    pub fn scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks5 => "socks5",
            Self::Socks5h => "socks5h",
        }
    }

    /// 面向用户的名字（UI 下拉框用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Http => "HTTP",
            Self::Https => "HTTPS",
            Self::Socks5 => "SOCKS5",
            Self::Socks5h => "SOCKS5H",
        }
    }

    pub fn all() -> [Self; 4] {
        [Self::Http, Self::Https, Self::Socks5, Self::Socks5h]
    }

    /// 从下拉框索引还原；越界回落到 HTTP（UI 侧调用）。
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Https,
            2 => Self::Socks5,
            3 => Self::Socks5h,
            _ => Self::Http,
        }
    }

    pub fn index(self) -> i32 {
        Self::all().iter().position(|p| *p == self).unwrap_or(0) as i32
    }

    /// 解析用户输入或存档里的 scheme 文本。未知一律 `None`（**不猜**）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "socks5" => Some(Self::Socks5),
            "socks5h" => Some(Self::Socks5h),
            _ => None,
        }
    }

    /// 该协议是否走 SOCKS5（UI 提示与参数差异用）。
    pub fn is_socks(self) -> bool {
        matches!(self, Self::Socks5 | Self::Socks5h)
    }
}

/// 一条代理配置。**稳定 ID 由调用方分配**，本模块不生成。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProxyProfile {
    /// 稳定 ID（UUID 字符串）。绑定关系只引用它，改名不会断链。
    pub id: String,
    /// 显示名（用户可读，允许重复；去重靠 id）
    pub name: String,
    pub protocol: ProxyProtocol,
    /// 主机名或 IP 字面量（IPv6 **不带**方括号，构造 URL 时自动加）
    pub host: String,
    /// 1–65535
    pub port: u16,
    /// 代理账号（明文；落 rclone.conf 时同样是明文）
    pub user: String,
    /// 代理密码（明文；落 rclone.conf 时是**百分号编码的明文**，不是 obscure）
    pub pass: String,
}

impl ProxyProfile {
    /// 校验一条配置是否可用。`Err` 是**面向用户**的中文原因，直接可以进 toast。
    ///
    /// 刻意不做「自动修正」（比如把 `http://` 前缀从 host 里剥掉）：静默修正会让用户
    /// 以为存进去的是他写的值，而实际落盘的是另一样东西。
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("代理配置名称不能为空".into());
        }
        let host = self.host.trim();
        if host.is_empty() {
            return Err("代理地址不能为空".into());
        }
        if host.contains(' ') {
            return Err("代理地址不能包含空格".into());
        }
        if host.contains("://") {
            return Err("代理地址只填主机名或 IP，不要带 http:// 之类的前缀".into());
        }
        if host.contains('[') || host.contains(']') {
            return Err("IPv6 地址请直接填写，不要带方括号".into());
        }
        // 域名/主机名/点分 IPv4 可以用字母数字与 . - _ 以及 IPv6 的冒号
        let looks_v6 = host.contains(':');
        if !looks_v6
            && !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err("代理地址包含非法字符".into());
        }
        if looks_v6 && host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("IPv6 地址格式不正确".into());
        }
        if self.port == 0 {
            return Err("端口必须在 1–65535 之间".into());
        }
        // 用户名和密码：只要有一个非空就当作「要认证」，
        // 但不允许密码里有裸的换行（会破坏 conf 文件的行结构）
        if self.pass.contains(['\n', '\r']) || self.user.contains(['\n', '\r']) {
            return Err("代理账号/密码不能包含换行".into());
        }
        Ok(())
    }

    pub fn has_auth(&self) -> bool {
        !self.user.is_empty() || !self.pass.is_empty()
    }

    /// 构造要写进 `override.http_proxy` 的完整 URL。
    ///
    /// 用 `url` crate 而不是字符串拼接，正是为了：
    /// - 用户名/密码里的 `@ : / ? # %` 等字符被正确百分号编码；
    /// - IPv6 主机自动加方括号；
    /// - 端口不会因为缺失而写出 `host:` 这种非法形式。
    ///
    /// 返回 `Err` 表示主机/端口本身无法构造 URL（正常流程里 `validate()` 已经拦掉）。
    ///
    /// 构造方式：用 `url` crate **只做凭据编码**（它能正确处理 `@ : / ? # 空格` 等），
    /// 主机与端口由本函数直接拼接。这样做是因为 `url` 会规范化掉默认端口
    /// （http 的 80、https 的 443）并给 special scheme 补 `/`，导致同一份配置
    /// 在不同协议下生成不同形态的字符串；而 rclone.conf 里的值最好稳定、可 diff、
    /// 且保留用户填的端口。
    pub fn to_proxy_url(&self) -> Result<String, String> {
        self.validate()?;
        let host = self.host.trim();
        // IPv6 字面量在 URL 里必须带方括号
        let host_part = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        let userinfo = if self.has_auth() {
            let user = encode_userinfo(&self.user);
            if self.pass.is_empty() {
                format!("{user}@")
            } else {
                format!("{user}:{}@", encode_userinfo(&self.pass))
            }
        } else {
            String::new()
        };
        Ok(format!(
            "{}://{}{}:{}",
            self.protocol.scheme(),
            userinfo,
            host_part,
            self.port
        ))
    }

    /// 供 UI 显示：**永远不含密码**。
    pub fn display_target(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host.trim())
        } else {
            self.host.trim().to_string()
        };
        format!("{}://{}:{}", self.protocol.scheme(), host, self.port)
    }
}

/// 百分号编码 userinfo 部分（用户名/密码）。
///
/// 用 `url::form_urlencoded` 的字节语义手写，而不是 `Url::set_password`：
/// 后者会连带规范化端口和路径，而我们只想编码这一段字符串。
///
/// 保留字符集：`A-Z a-z 0-9 - . _ ~` 之外全部编码。这样 `@ : / ? # % 空格`
/// 这些会破坏 URL 结构的字符都被安全转义。
fn encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 一条「remote → 代理配置」的绑定。
///
/// 除了「指向哪条配置」，还记录**下发状态**与**被覆盖的原值**。
/// 这是「不静默覆盖用户原有代理」这条硬要求的落地载体：
/// 没有原值就没法回滚，没有下发记录就没法区分「是我们改的」还是「用户后来自己改的」。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RemoteProxyBinding {
    /// remote 名（不含结尾冒号）
    pub remote: String,
    /// 引用的 `ProxyProfile.id`；空串 = 明确「不代理」
    pub proxy_id: String,
    /// 是否已经下发到 rclone。false = 待应用（RC 未连接时的状态）
    pub applied: bool,
    /// 上次**由本程序**写入 rclone 的实际值（空 = 从未写过）。
    ///
    /// 用来判断 rclone 里的当前值是否仍是我们写的：
    /// 若不一致，说明用户在别处改过，此时切回「不代理」不能贸然覆盖。
    pub applied_value: String,
    /// 覆盖前用户**原有**的 `override.http_proxy`。
    ///
    /// - `None`：原本没有这个键（切回「不代理」时应删除键）
    /// - `Some("")`：原本存在但为空（应恢复成空值而不是删键）
    pub original_override: Option<String>,
}

impl RemoteProxyBinding {
    /// 是否表示「明确不代理」。
    pub fn is_direct(&self) -> bool {
        self.proxy_id.is_empty()
    }

    /// 是否需要在连上 rclone 后补下发。
    pub fn needs_apply(&self) -> bool {
        !self.is_direct() && !self.applied
    }
}

/// 绑定表 + 配置表的查询辅助。
pub struct ProxyRegistry<'a> {
    pub proxies: &'a [ProxyProfile],
    pub bindings: &'a [RemoteProxyBinding],
}

impl<'a> ProxyRegistry<'a> {
    pub fn new(proxies: &'a [ProxyProfile], bindings: &'a [RemoteProxyBinding]) -> Self {
        Self { proxies, bindings }
    }

    pub fn by_id(&self, id: &str) -> Option<&'a ProxyProfile> {
        self.proxies.iter().find(|p| p.id == id)
    }

    /// 某 remote 绑定的配置（`None` = 没有绑定记录或明确不代理）。
    pub fn for_remote(&self, remote: &str) -> Option<&'a ProxyProfile> {
        let b = self
            .bindings
            .iter()
            .find(|b| b.remote.eq_ignore_ascii_case(remote))?;
        if b.is_direct() {
            return None;
        }
        self.by_id(&b.proxy_id)
    }

    /// 某配置被哪些 remote 使用（删除前要提示关联）。
    pub fn used_by(&self, proxy_id: &str) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|b| b.proxy_id == proxy_id)
            .map(|b| b.remote.clone())
            .collect()
    }

    /// 绑定了已不存在配置的 remote（悬空引用）。UI 要标成「待修复」而不是静默忽略。
    pub fn dangling(&self) -> Vec<&'a RemoteProxyBinding> {
        self.bindings
            .iter()
            .filter(|b| !b.is_direct() && self.by_id(&b.proxy_id).is_none())
            .collect()
    }
}

impl ProxyProfile {
    /// 复制一份用于编辑（换新 id 的时机由调用方决定）。
    pub fn duplicate_with_id(&self, id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..self.clone()
        }
    }
}

/// 把「期望状态」+「rclone 当前实际值」换算成一次具体操作。
///
/// 抽成纯函数是为了能脱离 RC 与网络做单元测试 —— 这段逻辑承载
/// 「不静默覆盖用户原有代理」「切回不代理也不误删」两条硬要求，
/// 是本次功能最容易出致命错误的地方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyAction {
    /// 无需任何操作
    Nothing,
    /// 把 remote 的 `override.http_proxy` 设成这个值
    Set(String),
    /// 删除 remote 的 `override.http_proxy` 键
    Unset,
    /// 恢复成原来的值（键原本存在）
    Restore(String),
    /// rclone 里的值已被别人改过，与我们的记录不一致 → 必须让用户裁决
    Conflict {
        /// 上次由本程序写入的值
        ours: String,
        /// 现在 rclone 里的值
        current: String,
    },
}

impl ProxyAction {
    pub fn is_noop(&self) -> bool {
        matches!(self, Self::Nothing)
    }
}

/// 计算「把某 remote 的代理设为 `desired`」需要做什么。
///
/// 参数：
/// - `desired`：期望写入的完整代理 URL（`None` = 用户选「不代理」）
/// - `current`：rclone 里**现在**的 `override.http_proxy`（`None` = 键不存在）
/// - `binding`：已有的绑定记录（`None` = 从未管理过这个 remote）
///
/// 关键规则：
/// 1. 要设置时，若 rclone 里已有值且**不是我们写的** → 先把它记进
///    `original_override`（由调用方写入绑定），本次仍然覆盖 —— 但调用方据此可回滚。
/// 2. 要移除（切回不代理）时：
///    - 从未管理过 → 什么都别动（用户自己设的代理不归我们管）
///    - rclone 里的值 ≠ 我们上次写的 → 别人改过，报冲突，**不覆盖**
///    - 否则恢复 `original_override`：有的恢复原值，没有的删键
pub fn plan_set(
    desired: Option<&str>,
    current: Option<&str>,
    binding: Option<&RemoteProxyBinding>,
) -> ProxyAction {
    match desired {
        // ---- 设置代理 ----
        Some(url) => {
            if current == Some(url) {
                ProxyAction::Nothing
            } else {
                ProxyAction::Set(url.to_string())
            }
        }
        // ---- 切回「不代理」 ----
        None => {
            let Some(b) = binding else {
                // 从没管过这个 remote：现在它身上的代理（如果有）是用户自己设的，
                // 我们没有原值记录，也就谈不上「恢复」——一律不碰。
                return ProxyAction::Nothing;
            };
            // 从没**成功下发过**任何东西：既没有 applied_value，也没有原值记录。
            // 此时无论 rclone 里现在是什么值，都不是我们写的，不该去动它。
            // （典型场景：只是存了一条「不代理」的绑定记录，还没连过 rclone）
            if b.applied_value.is_empty() && b.original_override.is_none() {
                return ProxyAction::Nothing;
            }
            // 判断 rclone 里的值是否仍是我们上次写的
            let ours = &b.applied_value;
            let untouched = match (b.original_override.as_deref(), current) {
                // 我们上次写的是 X，现在仍是 X → 是我们留下的
                _ if !ours.is_empty() && current == Some(ours.as_str()) => true,
                // 我们上次删掉了键（写的是空），现在仍不存在 → 是我们留下的
                _ if ours.is_empty() && current.is_none() => true,
                // 我们上次写的就是当前这个值
                _ if current == Some(ours.as_str()) => true,
                _ => false,
            };
            if !untouched {
                return ProxyAction::Conflict {
                    ours: ours.clone(),
                    current: current.unwrap_or("").to_string(),
                };
            }
            match b.original_override.as_deref() {
                Some(v) => ProxyAction::Restore(v.to_string()),
                None => ProxyAction::Unset,
            }
        }
    }
}

/// 在真正写入前，算出应该记进绑定的 `original_override`。
///
/// - 保留**最早**记录到的原值：第二次覆盖时不能把「我们自己写的值」当成原值，
///   否则回滚链就断了（A 覆盖 B 覆盖 C，回滚要回到 A）。
/// - 若 rclone 里当前值恰好是我们上次写的，说明中间没人动过，原值沿用。
pub fn remember_original(
    binding: Option<&RemoteProxyBinding>,
    current: Option<&str>,
) -> Option<String> {
    if let Some(b) = binding {
        // 已经有原值记录：除非当前值被别人改过（此时也不该覆盖），否则保持不动
        if b.original_override.is_some() {
            return b.original_override.clone();
        }
        // 我们上次写过，且现在仍是那个值 → 原值就是「没有」
        if !b.applied_value.is_empty() && current == Some(b.applied_value.as_str()) {
            return None;
        }
    }
    current.map(|v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(protocol: ProxyProtocol, host: &str, port: u16) -> ProxyProfile {
        ProxyProfile {
            id: "p1".into(),
            name: "测试".into(),
            protocol,
            host: host.into(),
            port,
            user: String::new(),
            pass: String::new(),
        }
    }

    #[test]
    fn plain_http_url() {
        let x = p(ProxyProtocol::Http, "127.0.0.1", 8080);
        assert_eq!(x.to_proxy_url().unwrap(), "http://127.0.0.1:8080");
    }

    #[test]
    fn socks5_url_uses_override_http_proxy_key_semantics() {
        // 实测：SOCKS5 必须通过 override.http_proxy + socks5:// 生效
        for (proto, want) in [
            (ProxyProtocol::Socks5, "socks5://127.0.0.1:1080"),
            (ProxyProtocol::Socks5h, "socks5h://127.0.0.1:1080"),
            (ProxyProtocol::Https, "https://127.0.0.1:1080"),
        ] {
            let x = p(proto, "127.0.0.1", 1080);
            assert_eq!(x.to_proxy_url().unwrap(), want);
        }
    }

    #[test]
    fn ipv6_host_gets_brackets() {
        let x = p(ProxyProtocol::Http, "::1", 8080);
        let url = x.to_proxy_url().unwrap();
        assert!(url.contains("[::1]"), "got {url}");
        assert_eq!(url, "http://[::1]:8080");
    }

    #[test]
    fn credentials_with_special_characters_are_percent_encoded() {
        // 用户需求明确要求覆盖特殊字符：@ : / ? # % 空格
        let mut x = p(ProxyProtocol::Http, "proxy.example.com", 3128);
        x.user = "alice@corp".into();
        x.pass = "p@ss:w/rd?#% and space".into();
        let url = x.to_proxy_url().unwrap();
        // 原始字符不得出现在 userinfo 里造成歧义
        assert!(!url.contains("alice@corp@"), "用户名里的 @ 必须被编码: {url}");
        assert!(url.contains("%40"), "@ 应编码为 %40: {url}");
        assert!(url.contains("%25"), "% 应编码为 %25: {url}");
        assert!(!url.contains(' '), "空格必须编码: {url}");
        // 结构仍然正确：scheme://userinfo@host:port
        assert!(url.starts_with("http://alice%40corp:"), "got {url}");
        assert!(url.ends_with("@proxy.example.com:3128"), "got {url}");
    }

    /// 每个特殊字符都必须能解码回原文（往返一致）。
    #[test]
    fn userinfo_roundtrips_through_percent_decoding() {
        let mut x = p(ProxyProtocol::Socks5, "127.0.0.1", 1080);
        x.user = "bob".into();
        x.pass = "se:cr@t/%#? .-_~".into();
        let url = x.to_proxy_url().unwrap();
        let userinfo = url
            .strip_prefix("socks5://")
            .and_then(|r| r.split('@').next())
            .unwrap();
        let (u, pw) = userinfo.split_once(':').unwrap();
        assert_eq!(percent_decode(u), "bob");
        assert_eq!(percent_decode(pw), "se:cr@t/%#? .-_~");
    }

    #[test]
    fn credentials_roundtrip_through_percent_decoding() {
        let mut x = p(ProxyProtocol::Socks5, "127.0.0.1", 1080);
        x.user = "bob".into();
        x.pass = "se:cr@t".into();
        let url = x.to_proxy_url().unwrap();
        assert_eq!(url, "socks5://bob:se%3Acr%40t@127.0.0.1:1080");
        let userinfo = url.strip_prefix("socks5://").unwrap().split('@').next().unwrap();
        let (_, pw) = userinfo.split_once(':').unwrap();
        assert_eq!(percent_decode(pw), "se:cr@t");
    }

    fn percent_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).to_string()
    }

    #[test]
    fn empty_password_still_yields_username_only() {
        let mut x = p(ProxyProtocol::Http, "h", 1);
        x.user = "u".into();
        let url = x.to_proxy_url().unwrap();
        assert!(url.starts_with("http://u@h:1"), "got {url}");
    }

    #[test]
    fn rejects_bad_values() {
        let mut x = p(ProxyProtocol::Http, "h", 80);
        assert!(x.validate().is_ok());

        x.port = 0;
        assert!(x.validate().unwrap_err().contains("端口"));

        x = p(ProxyProtocol::Http, "", 80);
        assert!(x.validate().unwrap_err().contains("地址不能为空"));

        x = p(ProxyProtocol::Http, "http://h", 80);
        assert!(x.validate().unwrap_err().contains("前缀"));

        x = p(ProxyProtocol::Http, "[::1]", 80);
        assert!(x.validate().unwrap_err().contains("方括号"));

        x = p(ProxyProtocol::Http, "has space", 80);
        assert!(x.validate().unwrap_err().contains("空格"));

        x = p(ProxyProtocol::Http, "999.1.1.1", 80);
        // 域名风格允许（不是 IPv6），但含非法字符的要拦
        x.host = "bad host!".into();
        assert!(x.validate().is_err());
    }

    #[test]
    fn rejects_invalid_ipv6() {
        let x = p(ProxyProtocol::Http, "::gg", 80);
        assert!(x.validate().unwrap_err().contains("IPv6"));
    }

    #[test]
    fn rejects_newlines_in_credentials() {
        // 换行会破坏 conf 的行结构，必须在模型层拦住
        let mut x = p(ProxyProtocol::Http, "h", 80);
        x.pass = "a\nb".into();
        assert!(x.validate().is_err());
    }

    #[test]
    fn protocol_parse_is_strict() {
        assert_eq!(ProxyProtocol::parse("HTTP"), Some(ProxyProtocol::Http));
        assert_eq!(ProxyProtocol::parse(" socks5h "), Some(ProxyProtocol::Socks5h));
        // 未知 scheme 不猜 —— 实测 rclone 会把未知 scheme 当 HTTP 代理静默使用
        assert_eq!(ProxyProtocol::parse("ftp-proxy"), None);
        assert_eq!(ProxyProtocol::parse(""), None);
        assert_eq!(ProxyProtocol::parse("socks4"), None);
    }

    #[test]
    fn index_roundtrip() {
        for p in ProxyProtocol::all() {
            assert_eq!(ProxyProtocol::from_index(p.index()), p);
        }
        assert_eq!(ProxyProtocol::from_index(99), ProxyProtocol::Http);
        assert_eq!(ProxyProtocol::from_index(-1), ProxyProtocol::Http);
    }

    #[test]
    fn display_target_hides_password() {
        let mut x = p(ProxyProtocol::Http, "proxy", 3128);
        x.user = "alice".into();
        x.pass = "hunter2".into();
        let s = x.display_target();
        assert!(!s.contains("hunter2"));
        assert!(!s.contains("alice"));
        assert_eq!(s, "http://proxy:3128");
    }

    #[test]
    fn display_target_brackets_ipv6() {
        let x = p(ProxyProtocol::Socks5, "::1", 1080);
        assert_eq!(x.display_target(), "socks5://[::1]:1080");
    }

    #[test]
    fn registry_resolves_bindings() {
        let a = p(ProxyProtocol::Http, "a", 1);
        let b = ProxyProfile::duplicate_with_id(&a, "p2");
        let proxies = vec![a, b];
        let bindings = vec![
            RemoteProxyBinding { remote: "r1".into(), proxy_id: "p1".into(), ..Default::default() },
            RemoteProxyBinding { remote: "r2".into(), proxy_id: String::new(), ..Default::default() },
            RemoteProxyBinding { remote: "r3".into(), proxy_id: "gone".into(), ..Default::default() },
        ];
        let reg = ProxyRegistry::new(&proxies, &bindings);
        assert_eq!(reg.for_remote("r1").map(|p| p.id.as_str()), Some("p1"));
        assert!(reg.for_remote("r2").is_none(), "空 id = 明确不代理");
        assert!(reg.for_remote("nope").is_none(), "无绑定 = 不代理");
        assert!(reg.for_remote("r3").is_none(), "悬空引用解析不出配置");
        assert_eq!(reg.used_by("p1"), vec!["r1".to_string()]);
        assert!(reg.used_by("p2").is_empty());
        let d = reg.dangling();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].remote, "r3");
    }

    #[test]
    fn lookup_is_case_insensitive_like_rclone() {
        let a = p(ProxyProtocol::Http, "a", 1);
        let bindings = vec![RemoteProxyBinding {
            remote: "Teldrive".into(),
            proxy_id: "p1".into(),
            ..Default::default()
        }];
        let proxies = vec![a];
        let reg = ProxyRegistry::new(&proxies, &bindings);
        assert!(reg.for_remote("teldrive").is_some());
    }

    #[test]
    fn serde_roundtrip_and_defaults() {
        let mut x = p(ProxyProtocol::Socks5h, "::1", 1080);
        x.user = "u".into();
        x.pass = "p".into();
        let s = serde_json::to_string(&x).unwrap();
        let back: ProxyProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(back, x);
        // 缺字段必须能补默认（老档案向前兼容）
        let partial: ProxyProfile = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert_eq!(partial.protocol, ProxyProtocol::Http);
        assert_eq!(partial.port, 0);
    }

    #[test]
    fn binding_serde() {
        let b = RemoteProxyBinding { remote: "r".into(), proxy_id: "p".into(), ..Default::default() };
        let s = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<RemoteProxyBinding>(&s).unwrap(), b);
        assert!(!b.is_direct());
        assert!(b.needs_apply(), "设了代理但未下发 → 待应用");
        let applied = RemoteProxyBinding {
            applied: true,
            ..b.clone()
        };
        assert!(!applied.needs_apply());
        // 「不代理」永远不需要下发
        let direct = RemoteProxyBinding { proxy_id: String::new(), ..b };
        assert!(!direct.needs_apply());
    }

    // ---------------- plan_set：不静默覆盖用户原有代理 ----------------

    fn binding(applied_value: &str, original: Option<&str>) -> RemoteProxyBinding {
        RemoteProxyBinding {
            remote: "r".into(),
            proxy_id: "p1".into(),
            applied: true,
            applied_value: applied_value.into(),
            original_override: original.map(|s| s.to_string()),
        }
    }

    #[test]
    fn set_proxy_when_nothing_there() {
        assert_eq!(
            plan_set(Some("http://new:1"), None, None),
            ProxyAction::Set("http://new:1".into())
        );
    }

    #[test]
    fn setting_same_value_is_noop() {
        assert_eq!(
            plan_set(Some("http://new:1"), Some("http://new:1"), None),
            ProxyAction::Nothing,
            "值相同就不要重复调 config/update"
        );
    }

    /// 用户原本手动配了代理 → 我们覆盖，但必须能记住原值以便回滚。
    #[test]
    fn overwriting_user_proxy_records_original() {
        let current = Some("http://user-proxy:3128");
        assert_eq!(
            plan_set(Some("http://rcgui:1"), current, None),
            ProxyAction::Set("http://rcgui:1".into())
        );
        // 首次覆盖：原值就是当前值
        assert_eq!(
            remember_original(None, current),
            Some("http://user-proxy:3128".into()),
            "必须记住用户原有代理，否则回滚不了"
        );
    }

    /// 第二次覆盖时不能把自己上次写的值当成「原值」，否则回滚链断裂。
    #[test]
    fn second_overwrite_keeps_earliest_original() {
        let b = binding("http://rcgui-1", Some("http://user-proxy:3128"));
        assert_eq!(
            remember_original(Some(&b), Some("http://rcgui-1")),
            Some("http://user-proxy:3128".into()),
            "已有原值记录时必须沿用最早的那个"
        );
        // rclone 里当前值已是我们写的、且原来没有 → 原值仍是 None
        let b2 = binding("http://rcgui-1", None);
        assert_eq!(remember_original(Some(&b2), Some("http://rcgui-1")), None);
    }

    #[test]
    fn switching_back_restores_user_original() {
        let b = binding("http://rcgui:1", Some("http://user-proxy:3128"));
        assert_eq!(
            plan_set(None, Some("http://rcgui:1"), Some(&b)),
            ProxyAction::Restore("http://user-proxy:3128".into()),
            "切回不代理必须恢复用户原值，而不是直接删键"
        );
    }

    #[test]
    fn switching_back_unsets_when_there_was_no_original() {
        let b = binding("http://rcgui:1", None);
        assert_eq!(plan_set(None, Some("http://rcgui:1"), Some(&b)), ProxyAction::Unset);
    }

    #[test]
    fn never_managed_remote_is_left_alone() {
        assert_eq!(
            plan_set(None, Some("http://user-proxy:3128"), None),
            ProxyAction::Nothing,
            "从没管过的 remote，用户自己设的代理不能被我们删掉"
        );
        assert_eq!(plan_set(None, None, None), ProxyAction::Nothing);
    }

    #[test]
    fn already_direct_binding_is_noop() {
        // 只是存了一条「不代理」的记录、还没连过 rclone：什么都不能动
        let b = RemoteProxyBinding {
            remote: "r".into(),
            ..Default::default()
        };
        assert_eq!(plan_set(None, None, Some(&b)), ProxyAction::Nothing);
        assert_eq!(
            plan_set(None, Some("x"), Some(&b)),
            ProxyAction::Nothing,
            "从未下发过，rclone 里的值不归我们管"
        );
    }

    /// 我们删过键、之后键又出现了 → 别人重新设的，报冲突不覆盖。
    #[test]
    fn reappeared_key_after_we_unset_is_conflict() {
        let b = binding("", Some("http://orig"));
        assert_eq!(
            plan_set(None, Some("http://new"), Some(&b)),
            ProxyAction::Conflict { ours: String::new(), current: "http://new".into() }
        );
    }

    /// 关键安全性：别人改过 rclone 里的值，我们不能一把盖掉。
    #[test]
    fn external_change_is_reported_as_conflict() {
        let b = binding("http://rcgui:1", Some("http://user-proxy:3128"));
        assert_eq!(
            plan_set(None, Some("http://someone-else:9"), Some(&b)),
            ProxyAction::Conflict {
                ours: "http://rcgui:1".into(),
                current: "http://someone-else:9".into()
            },
            "rclone 里的值已被他人修改，必须报冲突而不是静默覆盖"
        );
    }

    #[test]
    fn conflict_when_we_unset_but_key_reappeared() {
        let b = binding("", Some("http://user-proxy:3128"));
        assert_eq!(
            plan_set(None, Some("http://someone-else:9"), Some(&b)),
            ProxyAction::Conflict {
                ours: String::new(),
                current: "http://someone-else:9".into()
            }
        );
    }

    #[test]
    fn restore_of_empty_original_keeps_key_empty() {
        // 原本存在但为空 → 应恢复成空而不是删键（区分 None 与 Some("")）
        let b = binding("http://rcgui:1", Some(""));
        assert_eq!(plan_set(None, Some("http://rcgui:1"), Some(&b)), ProxyAction::Restore(String::new()));
    }
}
