//! remote 级代理配置模型，以及「这次该写哪个键、写什么值」的纯决策。
//!
//! 定位（用户需求）：**按 remote 分流**，不是按文件、域名或单次请求分流。
//!
//! 实测约束（rclone v1.75.4；键与格式另核对过上游 `backend/ftp`、`backend/sftp`、
//! `lib/proxy` 源码）：
//!
//! 1. 大多数后端只认 `override.http_proxy` **一个**键，由 Go `http.Transport` 按 URL
//!    scheme 决定走 HTTP 还是 SOCKS5。`override.socks_proxy` 会被写进 conf 但
//!    **完全不生效**。
//! 2. **ftp / sftp 是例外**：它们有后端原生选项 `http_proxy`（完整 URL）与
//!    `socks_proxy`（裸 `[user[:pass]@]host:port`，**不做 URL 解码**）。两个键同时
//!    存在时 **socks_proxy 优先**（两个后端都是 `if socks != "" { } else if http`）。
//! 3. rclone 对代理 URL **不做任何校验**：非法字符串照单全收，未知 scheme 会被静默
//!    当作 HTTP 代理使用。所以校验必须在这里做完，不能指望 rclone 报错。
//! 4. 代理账号密码以明文落 rclone.conf：override 是百分号编码的 URL，
//!    原生 `socks_proxy` 是原样字符串（连百分号编码都没有）。
//!
//! ## 产品策略（用户 2026-10-18 决定）
//!
//! **原生键只补空缺**：ftp / sftp 上如果用户自己已经写了 `http_proxy` 或
//! `socks_proxy`，RCGUI 不覆盖、不代管，只在界面上说明；只有两个键都为空时才写入。
//! `override.http_proxy` 沿用既定语义：覆盖前记原值，切回「不代理」时还原而不是删键。
//! 取舍与依据见 `docs/proxy-ftp-sftp-and-errors.md`。
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
    /// URL scheme。写进 `override.http_proxy` / 原生 `http_proxy` 的前缀。
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

/// rclone 里实际承载代理的配置键。
///
/// 大多数后端只有 `override.http_proxy`；ftp / sftp 用后端原生键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKey {
    /// `override.http_proxy`：HTTP 类后端的统一入口，值 = 带 scheme 的完整 URL
    Override,
    /// `http_proxy`：ftp / sftp 原生键，值 = `http(s)://[user:pass@]host:port`
    NativeHttp,
    /// `socks_proxy`：ftp / sftp 原生键，值 = 裸 `[user[:pass]@]host:port`
    NativeSocks,
}

impl ProxyKey {
    /// 写进 `config/update` / `config/unset` 的真实键名。
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Override => "override.http_proxy",
            Self::NativeHttp => "http_proxy",
            Self::NativeSocks => "socks_proxy",
        }
    }

    /// 面向用户的键名。**不携带值**，避免把凭据带进提示。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Override => "override.http_proxy",
            Self::NativeHttp => "原生 http_proxy",
            Self::NativeSocks => "原生 socks_proxy",
        }
    }

    /// 是否是 ftp / sftp 的原生键。
    pub const fn is_native(self) -> bool {
        !matches!(self, Self::Override)
    }

    /// ftp / sftp 的另一个原生键。rclone 会优先使用 `socks_proxy`。
    pub const fn native_peer(self) -> Option<Self> {
        match self {
            Self::NativeHttp => Some(Self::NativeSocks),
            Self::NativeSocks => Some(Self::NativeHttp),
            Self::Override => None,
        }
    }

    /// 从档案里持久化的键名还原；未知一律 `None`（**不猜**）。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "override.http_proxy" => Some(Self::Override),
            "http_proxy" => Some(Self::NativeHttp),
            "socks_proxy" => Some(Self::NativeSocks),
            _ => None,
        }
    }
}

/// 三个可能承载代理的键的当前值（`None` = 键不存在，`Some("")` = 存在但为空）。
///
/// 区分 `None` 与 `Some("")` 是回滚正确性的前提：原本存在但为空的键要恢复成空值，
/// 而不是删掉。rclone `config/get` 只返回**显式配置过**的键，不合成默认值，
/// 因此这个区分是可靠的。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfigValues {
    pub override_http: Option<String>,
    pub native_http: Option<String>,
    pub native_socks: Option<String>,
}

impl ProxyConfigValues {
    pub fn get(&self, key: ProxyKey) -> Option<&str> {
        match key {
            ProxyKey::Override => self.override_http.as_deref(),
            ProxyKey::NativeHttp => self.native_http.as_deref(),
            ProxyKey::NativeSocks => self.native_socks.as_deref(),
        }
    }

    pub fn set(&mut self, key: ProxyKey, value: Option<String>) {
        match key {
            ProxyKey::Override => self.override_http = value,
            ProxyKey::NativeHttp => self.native_http = value,
            ProxyKey::NativeSocks => self.native_socks = value,
        }
    }

    /// 从 rclone `config/get` 的返回里取三个键。
    pub fn from_config_map(map: &serde_json::Map<String, serde_json::Value>) -> Self {
        let get = |k: &str| {
            map.get(k)
                .map(|v| v.as_str().unwrap_or_default().to_string())
        };
        Self {
            override_http: get("override.http_proxy"),
            native_http: get("http_proxy"),
            native_socks: get("socks_proxy"),
        }
    }

    /// `config/get` 里的 backend type（统一小写；缺失返回空串）。
    pub fn backend_type(map: &serde_json::Map<String, serde_json::Value>) -> String {
        map.get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
    }
}

/// 该后端类型是否使用 ftp / sftp 的原生代理键。
///
/// 全量 `config/providers` 扫描（71 个后端）里只有 `ftp` / `sftp` 声明了
/// `http_proxy` / `socks_proxy`（`google photos` 的 `proxy` 是 gphotosdl 专用，
/// 与代理无关）。未知/空类型一律走 `override`，保持旧行为。
pub fn uses_native_keys(backend_type: &str) -> bool {
    matches!(
        backend_type.trim().to_ascii_lowercase().as_str(),
        "ftp" | "sftp"
    )
}

/// 目标 backend 上应该写哪个键。
pub fn proxy_key_for(backend_type: &str, protocol: ProxyProtocol) -> ProxyKey {
    if uses_native_keys(backend_type) {
        if protocol.is_socks() {
            ProxyKey::NativeSocks
        } else {
            ProxyKey::NativeHttp
        }
    } else {
        ProxyKey::Override
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

    /// 这条配置落到指定 backend 上时要写的键与值。
    ///
    /// - 普通后端：`override.http_proxy` + 完整 URL（与 [`Self::to_proxy_url`] 一致）
    /// - ftp / sftp + http/https：原生 `http_proxy` + 完整 URL
    /// - ftp / sftp + socks5/socks5h：原生 `socks_proxy` + **裸** `[user[:pass]@]host:port`
    pub fn to_apply_value(&self, backend_type: &str) -> Result<(ProxyKey, String), String> {
        let key = proxy_key_for(backend_type, self.protocol);
        let value = match key {
            ProxyKey::NativeSocks => self.to_native_socks_value()?,
            _ => self.to_proxy_url()?,
        };
        Ok((key, value))
    }

    /// 构造 ftp / sftp 原生 `socks_proxy` 的值。
    ///
    /// 格式由 rclone `lib/proxy.SOCKS5Dial` 决定：先按**第一个** `@` 切出
    /// `user[:pass]`，再按**第一个** `:` 切出用户名与密码，全程**不做 URL 解码**。
    /// 因此这里不能百分号编码（会把 `%40` 原样发给代理），只能把无法表达的凭据
    /// 直接拒绝并给出可操作的说明。
    ///
    /// 注意 `socks5` 与 `socks5h` 在这里**等价**：x/net/proxy 总是把主机名交给
    /// 代理解析，没有“本地解析”的变体。UI 必须明示这一点。
    pub fn to_native_socks_value(&self) -> Result<String, String> {
        self.validate()?;
        if self.user.contains('@') || self.pass.contains('@') {
            return Err(
                "ftp / sftp 的原生 socks_proxy 不支持账号或密码里的「@」：rclone 会在第一个 @ 处切分，且不做 URL 解码"
                    .into(),
            );
        }
        if self.user.contains(':') {
            return Err(
                "ftp / sftp 的原生 socks_proxy 不支持用户名里的「:」：rclone 用它分隔用户名与密码"
                    .into(),
            );
        }
        // IPv6 字面量在 host:port 里必须带方括号，否则 x/net/proxy 解析不了
        let host = self.host.trim();
        let host_part = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        let target = format!("{host_part}:{}", self.port);
        Ok(if self.has_auth() {
            if self.pass.is_empty() {
                format!("{}@{target}", self.user)
            } else {
                format!("{}:{}@{target}", self.user, self.pass)
            }
        } else {
            target
        })
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
    /// 上次**由本程序**写入 rclone 的实际值（空 = 没留下值）。
    ///
    /// 用来判断 rclone 里的当前值是否仍是我们写的：
    /// 若不一致，说明用户在别处改过，此时切回「不代理」不能贸然覆盖。
    pub applied_value: String,
    /// 上次**由本程序**写入的是哪个 rclone 配置键（空 = 旧档案，按 override 解释）。
    ///
    /// 必须记住键：ftp/sftp 用原生 `http_proxy`/`socks_proxy`，普通后端用
    /// `override.http_proxy`。换键（改协议、改远程类型）时，回滚只能回到
    /// **当时写过的那个键**，不能按“现在的类型”重新猜。
    pub applied_key: String,
    /// 覆盖前 `applied_key` 所指键上**原有**的值。
    ///
    /// - `None`：原本没有这个键（切回「不代理」时应删除键）
    /// - `Some("")`：原本存在但为空（应恢复成空值而不是删键）
    ///
    /// 名字里的 `override` 是历史原因：该字段最初只服务于 `override.http_proxy`，
    /// 现在泛指“被我们覆盖的那个键”。
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

    /// 上次写过的键。旧档案没有 `applied_key`：旧实现只写 override，
    /// 因此有下发记录时按 `override.http_proxy` 解释；什么都没写过则 `None`。
    pub fn applied_key(&self) -> Option<ProxyKey> {
        ProxyKey::parse(&self.applied_key).or_else(|| {
            if !self.applied_value.is_empty() || self.original_override.is_some() {
                Some(ProxyKey::Override)
            } else {
                None
            }
        })
    }

    /// 我们在 rclone 里是否留有脚印。
    ///
    /// 有值 = 要回滚；有原值记录 = 也要回滚（哪怕上次写的是空）。
    /// 回滚完成后会调用 [`Self::clear_footprint`]，所以“没有脚印”=
    /// 用户现在的配置不归我们管，任何情况下都不该动。
    pub fn has_footprint(&self) -> bool {
        !self.applied_value.is_empty() || self.original_override.is_some()
    }

    /// 清掉脚印（回滚成功，或换键时旧键已还原）。
    pub fn clear_footprint(&mut self) {
        self.applied_key.clear();
        self.applied_value.clear();
        self.original_override = None;
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

// ---------------- 下发计划 ----------------

/// 一次下发中的一个原子写操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyStep {
    /// 把某键设成该值
    Set { key: ProxyKey, value: String },
    /// 删除某键
    Unset { key: ProxyKey },
    /// 把某键恢复成覆盖前的值
    Restore { key: ProxyKey, value: String },
}

impl ProxyStep {
    pub fn key(&self) -> ProxyKey {
        match self {
            Self::Set { key, .. } | Self::Unset { key } | Self::Restore { key, .. } => *key,
        }
    }
}

/// 执行成功后要写回绑定的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingCommit {
    pub key: ProxyKey,
    /// 我们留在 rclone 里的值；空串 = 没有留下值（已删除，或没写）
    pub value: String,
    /// 覆盖前该键的原值（`None` = 原本没有这个键）
    pub original: Option<String>,
}

/// 一次下发的面向用户的结果类别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanOutcome {
    Applied,
    Removed,
    Restored(String),
    /// 旧键已还原，但目标键上是用户自己的配置，本次没有写入
    Respect(ProxyKey),
}

/// [`plan_apply`] 的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyPlan {
    /// 已经是期望值，不需要任何写入
    Nothing { key: ProxyKey },
    /// 需要按顺序执行这些步骤；全部成功后按 `commit` 更新绑定
    Steps {
        steps: Vec<ProxyStep>,
        commit: BindingCommit,
        outcome: PlanOutcome,
    },
    /// 用户已经在这个键上配了代理：尊重用户配置，只补空缺
    RespectExisting { key: ProxyKey },
    /// rclone 里的值被本程序之外的方式改过
    Conflict {
        key: ProxyKey,
        ours: String,
        current: String,
    },
    /// 从没由本程序管理过，也不该由本程序回滚
    Unmanaged,
}

/// 取出「我们在 rclone 里留下的脚印」。
///
/// 只有 `applied_value` 非空或 `original_override` 存在时才算有脚印；
/// 否则无论 `applied_key` 里留着什么，都不该去动 rclone。
///
/// 返回值全部借用绑定本身，因此是 `Copy` 的，可以在同一函数里多次判断。
fn footprint_of(b: &RemoteProxyBinding) -> Option<(ProxyKey, &str, Option<&str>)> {
    if !b.has_footprint() {
        return None;
    }
    let key = b.applied_key()?;
    Some((key, b.applied_value.as_str(), b.original_override.as_deref()))
}

/// 计算一次下发该做什么。**纯函数**，不碰网络也不碰文件。
///
/// 规则（用户 2026-10-18 决定：原生键**只补空缺**）：
///
/// | 情形 | 结论 |
/// |---|---|
/// | 目标键上有我们的脚印且值未变 | `Nothing` |
/// | 目标键上有我们的脚印、但值被外部改过 | `Conflict`，零写入 |
/// | 目标键上没有脚印、已有用户值 | `RespectExisting`，零写入 |
/// | 目标是原生键、另一个原生键有用户值 | `RespectExisting`（rclone 优先 socks） |
/// | 脚印在别的键（改协议/改远程类型） | 先还原旧键，再写新键 |
/// | 期望「不代理」且有脚印 | 还原原值或删键，并清空脚印 |
/// | 期望「不代理」且无脚印 | `Unmanaged` |
///
/// `backend_type` 必须来自 `config/get` 的 `type`（权威值），不要用 UI 缓存猜。
pub fn plan_apply(
    desired: Option<&ProxyProfile>,
    backend_type: &str,
    values: &ProxyConfigValues,
    binding: Option<&RemoteProxyBinding>,
) -> Result<ApplyPlan, String> {
    let footprint = binding.and_then(footprint_of);

    // ---------- 期望「不代理」：回滚 ----------
    let Some(profile) = desired else {
        return Ok(rollback_plan(footprint, values));
    };

    // ---------- 期望设置某个代理 ----------
    let (target, value) = profile.to_apply_value(backend_type)?;

    // 1) 脚印就在目标键上：就地判断
    if let Some((key, ours, original)) = footprint.filter(|(k, _, _)| *k == target) {
        if ours.is_empty() {
            // 我们上次删掉了这个键，现在它又有值 → 别人设的
            if let Some(cur) = values.get(key) {
                return Ok(ApplyPlan::Conflict {
                    key,
                    ours: String::new(),
                    current: cur.to_string(),
                });
            }
        } else if values.get(key) != Some(ours) {
            return Ok(ApplyPlan::Conflict {
                key,
                ours: ours.to_string(),
                current: values.get(key).unwrap_or_default().to_string(),
            });
        }
        // rclone 会优先使用 socks_proxy：另一个原生键被用户占用时，
        // 我们写的值其实不会生效，必须说清楚而不是谎报成功。
        if let Some(peer) = key.native_peer() {
            if values.get(peer).map(|v| !v.is_empty()).unwrap_or(false) {
                return Ok(ApplyPlan::RespectExisting { key: peer });
            }
        }
        if ours == value {
            return Ok(ApplyPlan::Nothing { key });
        }
        // 值变了（用户改了地址/协议）：覆盖的是我们自己的值，原值记录沿用
        return Ok(ApplyPlan::Steps {
            steps: vec![ProxyStep::Set {
                key,
                value: value.clone(),
            }],
            commit: BindingCommit {
                key,
                value,
                original: original.map(str::to_string),
            },
            outcome: PlanOutcome::Applied,
        });
    }

    // 2) 脚印在别的键上：先还原（那是我们自己写的，且原生 socks 会遮挡用户配置）
    let mut steps = Vec::new();
    let mut after = values.clone();
    if let Some((key, ours, original)) = footprint {
        let current = values.get(key);
        let untouched = if ours.is_empty() {
            current.is_none() || current == Some("")
        } else {
            current == Some(ours)
        };
        if !untouched {
            return Ok(ApplyPlan::Conflict {
                key,
                ours: ours.to_string(),
                current: current.unwrap_or_default().to_string(),
            });
        }
        let rollback = match original {
            Some(v) => {
                after.set(key, Some(v.to_string()));
                ProxyStep::Restore {
                    key,
                    value: v.to_string(),
                }
            }
            None => {
                after.set(key, None);
                ProxyStep::Unset { key }
            }
        };
        steps.push(rollback);
    }

    // 3) 目标键上的现状
    if let Some(cur) = after.get(target) {
        if !cur.is_empty() {
            // 已经是期望值：不重复写，也不「接管」用户的值（否则回滚时会误删）
            if cur == value.as_str() {
                if steps.is_empty() {
                    return Ok(ApplyPlan::Nothing { key: target });
                }
                return Ok(finish_without_write(steps, target, target));
            }
            // 原生键只补空缺；override 类沿用「覆盖 + 记原值」，落到第 4 步
            if target.is_native() {
                let peer_occupied = target
                    .native_peer()
                    .and_then(|peer| after.get(peer))
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                let key = if peer_occupied {
                    target.native_peer().unwrap_or(target)
                } else {
                    target
                };
                return Ok(finish_without_write(steps, target, key));
            }
        }
    }
    if let Some(peer) = target.native_peer() {
        if after.get(peer).map(|v| !v.is_empty()).unwrap_or(false) {
            return Ok(finish_without_write(steps, target, peer));
        }
    }

    // 4) 写目标键。非原生键覆盖前要记住用户原值；原生键按策略此时必为空。
    let original = if target.is_native() {
        None
    } else {
        after.get(target).map(|v| v.to_string())
    };
    steps.push(ProxyStep::Set {
        key: target,
        value: value.clone(),
    });
    Ok(ApplyPlan::Steps {
        steps,
        commit: BindingCommit {
            key: target,
            value,
            original,
        },
        outcome: PlanOutcome::Applied,
    })
}

/// 回滚计划：有脚印就还原，没有就明确「不归我们管」。
fn rollback_plan(
    footprint: Option<(ProxyKey, &str, Option<&str>)>,
    values: &ProxyConfigValues,
) -> ApplyPlan {
    let Some((key, ours, original)) = footprint else {
        return ApplyPlan::Unmanaged;
    };
    let current = values.get(key);
    let untouched = if ours.is_empty() {
        current.is_none() || current == Some("")
    } else {
        current == Some(ours)
    };
    if !untouched {
        return ApplyPlan::Conflict {
            key,
            ours: ours.to_string(),
            current: current.unwrap_or_default().to_string(),
        };
    }
    let (step, outcome) = match original {
        Some(v) => (
            ProxyStep::Restore {
                key,
                value: v.to_string(),
            },
            PlanOutcome::Restored(v.to_string()),
        ),
        None => (ProxyStep::Unset { key }, PlanOutcome::Removed),
    };
    ApplyPlan::Steps {
        steps: vec![step],
        // 回滚后不再留脚印：否则下一轮会把“恢复值”当成我们写的再删一次，
        // 最终吃掉用户原有的设置（缺陷 B）。
        commit: BindingCommit {
            key,
            value: String::new(),
            original: None,
        },
        outcome,
    }
}

/// 旧键已还原（或本来无需还原），但目标键被用户占用 → 只报告、不写入。
fn finish_without_write(steps: Vec<ProxyStep>, target: ProxyKey, note: ProxyKey) -> ApplyPlan {
    if steps.is_empty() {
        ApplyPlan::RespectExisting { key: note }
    } else {
        ApplyPlan::Steps {
            steps,
            // 旧键已还原，目标键归用户 → 我们不留脚印
            commit: BindingCommit {
                key: target,
                value: String::new(),
                original: None,
            },
            outcome: PlanOutcome::Respect(note),
        }
    }
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

    // ---------------- 键选择与值构造 ----------------

    /// 只有 ftp / sftp 走原生键；其它（含未知类型）保持 override。
    #[test]
    fn only_ftp_and_sftp_use_native_keys() {
        use ProxyProtocol::*;
        assert_eq!(proxy_key_for("ftp", Http), ProxyKey::NativeHttp);
        assert_eq!(proxy_key_for("  SFTP ", Socks5), ProxyKey::NativeSocks);
        assert_eq!(proxy_key_for("sftp", Socks5h), ProxyKey::NativeSocks);
        assert_eq!(proxy_key_for("ftp", Https), ProxyKey::NativeHttp);
        for t in ["s3", "webdav", "drive", "", "unknown", "alias"] {
            assert_eq!(proxy_key_for(t, Socks5), ProxyKey::Override, "{t} 应走 override");
        }
        assert!(uses_native_keys("FTP"));
        assert!(!uses_native_keys("s3"));
        assert_eq!(ProxyKey::parse("http_proxy"), Some(ProxyKey::NativeHttp));
        assert_eq!(ProxyKey::parse("override.socks_proxy"), None);
    }

    fn apply_value(p: &ProxyProfile, t: &str) -> (&'static str, String) {
        let (k, v) = p.to_apply_value(t).expect("值应能构造");
        (k.config_key(), v)
    }

    #[test]
    fn apply_value_selects_key_and_format() {
        let http = p(ProxyProtocol::Http, "h", 1);
        let socks = p(ProxyProtocol::Socks5, "h", 1);
        assert_eq!(apply_value(&http, "ftp"), ("http_proxy", "http://h:1".into()));
        assert_eq!(apply_value(&http, "s3"), ("override.http_proxy", "http://h:1".into()));
        assert_eq!(apply_value(&socks, "ftp"), ("socks_proxy", "h:1".into()));
        assert_eq!(apply_value(&socks, "s3"), ("override.http_proxy", "socks5://h:1".into()));
    }

    /// 原生 socks_proxy 是**裸** host:port，不带 scheme，也不做 URL 编码。
    #[test]
    fn native_socks_value_has_no_scheme() {
        let mut x = p(ProxyProtocol::Socks5, "127.0.0.1", 1080);
        assert_eq!(x.to_native_socks_value().unwrap(), "127.0.0.1:1080");
        x.user = "bob".into();
        assert_eq!(x.to_native_socks_value().unwrap(), "bob@127.0.0.1:1080");
        x.pass = "pa:ss".into();
        assert_eq!(
            x.to_native_socks_value().unwrap(),
            "bob:pa:ss@127.0.0.1:1080",
            "密码里的「:」可以（rclone 只按第一个 : 切分）"
        );
    }

    /// rclone 不做 URL 解码，所以「@」无法表达，只能拒绝并说明。
    #[test]
    fn native_socks_rejects_ambiguous_credentials() {
        let mut x = p(ProxyProtocol::Socks5, "h", 1);
        x.user = "a:b".into();
        let e = x.to_native_socks_value().unwrap_err();
        assert!(e.contains("用户名") && e.contains(":"));
        x.user = "a@b".into();
        assert!(x.to_native_socks_value().unwrap_err().contains("@"));
        x.user = "u".into();
        x.pass = "p@ss".into();
        assert!(x.to_native_socks_value().unwrap_err().contains("@"));
    }

    #[test]
    fn native_socks_brackets_ipv6() {
        let x = p(ProxyProtocol::Socks5h, "::1", 1080);
        assert_eq!(x.to_native_socks_value().unwrap(), "[::1]:1080");
    }

    #[test]
    fn config_values_read_the_three_keys() {
        let map: serde_json::Map<String, serde_json::Value> = serde_json::json!({
            "type": "ftp",
            "http_proxy": "http://p:1",
            "socks_proxy": "",
            "pass": "secret"
        })
        .as_object()
        .unwrap()
        .clone();
        let v = ProxyConfigValues::from_config_map(&map);
        assert_eq!(v.native_http.as_deref(), Some("http://p:1"));
        assert_eq!(v.native_socks.as_deref(), Some(""), "存在但为空要保留区分");
        assert_eq!(v.override_http, None, "没配置的键不能凭默认值造出来");
        assert_eq!(ProxyConfigValues::backend_type(&map), "ftp");
    }

    // ---------------- plan_apply：覆盖 / 只补空缺 / 回滚 ----------------

    fn vals(
        override_http: Option<&str>,
        http: Option<&str>,
        socks: Option<&str>,
    ) -> ProxyConfigValues {
        ProxyConfigValues {
            override_http: override_http.map(str::to_string),
            native_http: http.map(str::to_string),
            native_socks: socks.map(str::to_string),
        }
    }

    /// 一条「我们写过」的绑定。`key` 为空 = 旧档案（按 override 解释）。
    fn binding(key: &str, applied_value: &str, original: Option<&str>) -> RemoteProxyBinding {
        RemoteProxyBinding {
            remote: "r".into(),
            proxy_id: "p1".into(),
            applied: true,
            applied_key: key.into(),
            applied_value: applied_value.into(),
            original_override: original.map(str::to_string),
        }
    }

    fn steps_of(plan: ApplyPlan) -> (Vec<ProxyStep>, BindingCommit, PlanOutcome) {
        match plan {
            ApplyPlan::Steps {
                steps,
                commit,
                outcome,
            } => (steps, commit, outcome),
            other => panic!("应为 Steps，实际 {other:?}"),
        }
    }

    /// 普通后端：首次下发写 override，并把用户原值记进绑定。
    #[test]
    fn plain_backend_sets_override_and_remembers_original() {
        let prof = p(ProxyProtocol::Http, "rcgui", 1);
        let cur = vals(Some("http://user:3128"), None, None);
        let (steps, commit, outcome) =
            steps_of(plan_apply(Some(&prof), "s3", &cur, None).unwrap());
        assert_eq!(
            steps,
            vec![ProxyStep::Set { key: ProxyKey::Override, value: "http://rcgui:1".into() }]
        );
        assert_eq!(commit.key, ProxyKey::Override);
        assert_eq!(commit.value, "http://rcgui:1");
        assert_eq!(commit.original.as_deref(), Some("http://user:3128"));
        assert_eq!(outcome, PlanOutcome::Applied);
    }

    #[test]
    fn plain_backend_same_value_is_nothing() {
        let prof = p(ProxyProtocol::Http, "rcgui", 1);
        let cur = vals(Some("http://rcgui:1"), None, None);
        assert_eq!(
            plan_apply(Some(&prof), "s3", &cur, None).unwrap(),
            ApplyPlan::Nothing { key: ProxyKey::Override },
            "值相同就不要重复调 config/update"
        );
    }

    /// 已经是我们写的值：什么都不做，且不能再记一次原值。
    #[test]
    fn same_value_with_footprint_is_nothing() {
        let prof = p(ProxyProtocol::Http, "rcgui", 1);
        let cur = vals(Some("http://rcgui:1"), None, None);
        let b = binding("override.http_proxy", "http://rcgui:1", Some("http://user:3128"));
        assert_eq!(
            plan_apply(Some(&prof), "s3", &cur, Some(&b)).unwrap(),
            ApplyPlan::Nothing { key: ProxyKey::Override }
        );
    }

    /// 改地址但键没变：覆盖我们自己的值，**保留最早的原值**。
    #[test]
    fn value_change_keeps_earliest_original() {
        let prof = p(ProxyProtocol::Http, "rcgui2", 2);
        let cur = vals(Some("http://rcgui:1"), None, None);
        let b = binding("override.http_proxy", "http://rcgui:1", Some("http://user:3128"));
        let (steps, commit, _) =
            steps_of(plan_apply(Some(&prof), "s3", &cur, Some(&b)).unwrap());
        assert_eq!(steps.len(), 1);
        assert_eq!(commit.original.as_deref(), Some("http://user:3128"));
    }

    /// 切回不代理：恢复用户原值，且**清空脚印**（否则下一轮会把恢复值删掉）。
    #[test]
    fn rollback_restores_original_and_clears_footprint() {
        let cur = vals(Some("http://rcgui:1"), None, None);
        let b = binding("override.http_proxy", "http://rcgui:1", Some("http://user:3128"));
        let (steps, commit, outcome) = steps_of(plan_apply(None, "s3", &cur, Some(&b)).unwrap());
        assert_eq!(
            steps,
            vec![ProxyStep::Restore { key: ProxyKey::Override, value: "http://user:3128".into() }]
        );
        assert_eq!(commit.value, "", "回滚后不再留脚印");
        assert_eq!(commit.original, None);
        assert_eq!(outcome, PlanOutcome::Restored("http://user:3128".into()));

        // 第二轮回滚：脚印已清空 → 不归我们管，绝不能把刚恢复的值删掉
        let mut b2 = b.clone();
        b2.applied_value.clear();
        b2.original_override = None;
        let cur2 = vals(Some("http://user:3128"), None, None);
        assert_eq!(
            plan_apply(None, "s3", &cur2, Some(&b2)).unwrap(),
            ApplyPlan::Unmanaged
        );
    }

    #[test]
    fn rollback_unsets_when_there_was_no_original() {
        let cur = vals(Some("http://rcgui:1"), None, None);
        let b = binding("override.http_proxy", "http://rcgui:1", None);
        let (steps, commit, outcome) = steps_of(plan_apply(None, "s3", &cur, Some(&b)).unwrap());
        assert_eq!(steps, vec![ProxyStep::Unset { key: ProxyKey::Override }]);
        assert_eq!(commit.value, "");
        assert_eq!(outcome, PlanOutcome::Removed);
    }

    /// 原本存在但为空：恢复成空值，而不是删键。
    #[test]
    fn rollback_restores_empty_value_instead_of_unsetting() {
        let cur = vals(Some("http://rcgui:1"), None, None);
        let b = binding("override.http_proxy", "http://rcgui:1", Some(""));
        let (steps, _, outcome) = steps_of(plan_apply(None, "s3", &cur, Some(&b)).unwrap());
        assert_eq!(
            steps,
            vec![ProxyStep::Restore { key: ProxyKey::Override, value: String::new() }]
        );
        assert_eq!(outcome, PlanOutcome::Restored(String::new()));
    }

    /// 从没管过 → 用户的代理不能被我们删掉。
    #[test]
    fn unmanaged_remote_is_left_alone() {
        let cur = vals(Some("http://user:3128"), None, None);
        assert_eq!(plan_apply(None, "s3", &cur, None).unwrap(), ApplyPlan::Unmanaged);
        let empty = RemoteProxyBinding {
            remote: "r".into(),
            ..Default::default()
        };
        assert_eq!(
            plan_apply(None, "s3", &cur, Some(&empty)).unwrap(),
            ApplyPlan::Unmanaged,
            "只有一条「不代理」记录、没下发过，不归我们管"
        );
    }

    /// 别人改过 rclone 里的值 → 报冲突，零写入。
    #[test]
    fn external_change_is_conflict() {
        let cur = vals(Some("http://someone-else:9"), None, None);
        let b = binding("override.http_proxy", "http://rcgui:1", None);
        assert_eq!(
            plan_apply(None, "s3", &cur, Some(&b)).unwrap(),
            ApplyPlan::Conflict {
                key: ProxyKey::Override,
                ours: "http://rcgui:1".into(),
                current: "http://someone-else:9".into()
            }
        );
        // 换个新地址也一样：不能一把盖掉别人的改动
        let prof = p(ProxyProtocol::Http, "rcgui2", 2);
        assert!(matches!(
            plan_apply(Some(&prof), "s3", &cur, Some(&b)).unwrap(),
            ApplyPlan::Conflict { .. }
        ));
    }

    /// ftp 走原生 http_proxy，不能再去写 override。
    #[test]
    fn ftp_uses_native_http_proxy_not_override() {
        let prof = p(ProxyProtocol::Http, "rcgui", 1);
        let empty = vals(None, None, None);
        let (steps, commit, _) = steps_of(plan_apply(Some(&prof), "ftp", &empty, None).unwrap());
        assert_eq!(
            steps,
            vec![ProxyStep::Set { key: ProxyKey::NativeHttp, value: "http://rcgui:1".into() }]
        );
        assert_eq!(commit.key, ProxyKey::NativeHttp);
        assert_eq!(commit.original, None, "原生键只补空缺：写入时它必然为空");
    }

    #[test]
    fn ftp_socks_writes_bare_host_port() {
        let mut prof = p(ProxyProtocol::Socks5, "10.0.0.9", 1080);
        prof.user = "u".into();
        prof.pass = "p".into();
        let empty = vals(None, None, None);
        let (steps, _, _) = steps_of(plan_apply(Some(&prof), "sftp", &empty, None).unwrap());
        assert_eq!(
            steps,
            vec![ProxyStep::Set {
                key: ProxyKey::NativeSocks,
                value: "u:p@10.0.0.9:1080".into()
            }]
        );
    }

    /// 只补空缺：用户自己设的 http_proxy 不能被覆盖。
    #[test]
    fn ftp_respects_existing_user_http_proxy() {
        let prof = p(ProxyProtocol::Http, "rcgui", 1);
        let cur = vals(None, Some("http://user:3128"), None);
        assert_eq!(
            plan_apply(Some(&prof), "ftp", &cur, None).unwrap(),
            ApplyPlan::RespectExisting { key: ProxyKey::NativeHttp }
        );
        // 值恰好相同 → 视为已满足，但不能接管（回滚时会误删用户的值）
        let same = vals(None, Some("http://rcgui:1"), None);
        assert_eq!(
            plan_apply(Some(&prof), "ftp", &same, None).unwrap(),
            ApplyPlan::Nothing { key: ProxyKey::NativeHttp }
        );
        // 空值不算用户配置
        let empty_slot = vals(None, Some(""), None);
        assert!(matches!(
            plan_apply(Some(&prof), "ftp", &empty_slot, None).unwrap(),
            ApplyPlan::Steps { .. }
        ));
    }

    /// rclone 优先用 socks_proxy：另一个原生键有值时必须说明，而不是写个死值。
    #[test]
    fn ftp_respects_user_socks_proxy() {
        let prof = p(ProxyProtocol::Http, "rcgui", 1);
        let cur = vals(None, None, Some("user:pass@h:1080"));
        assert_eq!(
            plan_apply(Some(&prof), "ftp", &cur, None).unwrap(),
            ApplyPlan::RespectExisting { key: ProxyKey::NativeSocks }
        );
    }

    /// 改协议 = 换键：先还原旧键，再写新键。
    #[test]
    fn switching_protocol_rolls_back_old_key() {
        let prof = p(ProxyProtocol::Socks5, "rcgui", 1080);
        let cur = vals(None, Some("http://rcgui:1"), None);
        let b = binding("http_proxy", "http://rcgui:1", None);
        let (steps, commit, outcome) =
            steps_of(plan_apply(Some(&prof), "ftp", &cur, Some(&b)).unwrap());
        assert_eq!(
            steps,
            vec![
                ProxyStep::Unset { key: ProxyKey::NativeHttp },
                ProxyStep::Set { key: ProxyKey::NativeSocks, value: "rcgui:1080".into() },
            ]
        );
        assert_eq!(commit.key, ProxyKey::NativeSocks);
        assert_eq!(outcome, PlanOutcome::Applied);
    }

    /// 换键时旧键被外部改过 → 冲突，新键也不能写。
    #[test]
    fn switching_protocol_conflicts_when_old_key_changed() {
        let prof = p(ProxyProtocol::Socks5, "rcgui", 1080);
        let cur = vals(None, Some("http://someone-else:9"), None);
        let b = binding("http_proxy", "http://rcgui:1", None);
        assert!(matches!(
            plan_apply(Some(&prof), "ftp", &cur, Some(&b)).unwrap(),
            ApplyPlan::Conflict { key: ProxyKey::NativeHttp, .. }
        ));
    }

    /// 旧档案没有 applied_key：按 override 解释，回滚仍能对上。
    #[test]
    fn legacy_binding_without_key_rolls_back_override() {
        let cur = vals(Some("http://rcgui:1"), None, None);
        let b = binding("", "http://rcgui:1", None);
        let (steps, _, _) = steps_of(plan_apply(None, "ftp", &cur, Some(&b)).unwrap());
        assert_eq!(steps, vec![ProxyStep::Unset { key: ProxyKey::Override }]);
    }

    /// 远程类型从 s3 改成 ftp：旧 override 先还原，再写原生键。
    #[test]
    fn type_change_moves_override_to_native() {
        let prof = p(ProxyProtocol::Http, "rcgui", 1);
        let cur = ProxyConfigValues {
            override_http: Some("http://rcgui-old:1".into()),
            ..Default::default()
        };
        let b = binding("override.http_proxy", "http://rcgui-old:1", Some("http://user:3128"));
        let (steps, commit, _) =
            steps_of(plan_apply(Some(&prof), "ftp", &cur, Some(&b)).unwrap());
        assert_eq!(
            steps,
            vec![
                ProxyStep::Restore { key: ProxyKey::Override, value: "http://user:3128".into() },
                ProxyStep::Set { key: ProxyKey::NativeHttp, value: "http://rcgui:1".into() },
            ]
        );
        assert_eq!(commit.key, ProxyKey::NativeHttp);
    }

    /// 换键后目标键上是用户的值：旧键还原 + 明确跳过，且不能留脚印。
    #[test]
    fn switch_respects_occupied_target_after_rollback() {
        let prof = p(ProxyProtocol::Http, "rcgui", 1);
        let cur = ProxyConfigValues {
            native_http: Some("http://user:3128".into()),
            native_socks: Some("10.0.0.9:1080".into()),
            ..Default::default()
        };
        let b = binding("socks_proxy", "10.0.0.9:1080", None);
        let (steps, commit, outcome) =
            steps_of(plan_apply(Some(&prof), "ftp", &cur, Some(&b)).unwrap());
        assert_eq!(steps, vec![ProxyStep::Unset { key: ProxyKey::NativeSocks }]);
        assert_eq!(commit.value, "", "没有写入目标键，不能留脚印");
        assert_eq!(outcome, PlanOutcome::Respect(ProxyKey::NativeHttp));
    }
}
