//! 按 remote 分配代理：把档案里的绑定下发到 rclone，并支持回滚。
//!
//! 分工（与 `rcgui_profile::proxy` 的关系）：
//! - `rcgui_profile::proxy` 负责**纯决策**（该 Set / Unset / Restore / 报冲突），
//!   不碰网络也不碰文件，因此那部分逻辑有完整单元测试；
//! - 本模块负责**真实下发**：读 rclone 当前值 → 用决策函数算出动作 → 调 RC →
//!   把结果（含被覆盖的原值）写回档案。
//!
//! 三条硬要求（用户明确提出，勿违反）：
//!
//! 1. **不静默覆盖用户原有代理** —— 覆盖前必须把原值记进 `original_override`；
//! 2. **切回「不代理」也不误删原有设置** —— 按记录恢复原值，而不是无脑 unset；
//! 3. **不重启、不停 rclone** —— 只调 `config/update` / `config/unset`，
//!    绝不触碰进程；代理只在后端实例创建时生效，所以要提示用户重新挂载/启动服务。
//!
//! 另外：大多数后端只认 `override.http_proxy` 一个键；ftp / sftp 用后端原生
//! `http_proxy` / `socks_proxy`（由 `rcgui_profile::proxy` 决定该写哪个键）。
//! 依据见 `docs/proxy-ftp-sftp-and-errors.md`。

use crate::config::AppConfig;
use rcgui_profile::proxy::{
    plan_apply, uses_native_keys, ApplyPlan, PlanOutcome, ProxyConfigValues, ProxyKey, ProxyProfile,
    ProxyStep,
};
use rcgui_profile::{ProxyRegistry, RemoteProxyBinding};
use rcgui_rc::RcClient;

/// 该 remote 当前绑定的代理配置（`None` = 不代理 / 悬空引用）。
pub fn bound_profile(cfg: &AppConfig, remote: &str) -> Option<ProxyProfile> {
    let reg = ProxyRegistry::new(&cfg.proxies, &cfg.proxy_bindings);
    reg.for_remote(remote).cloned()
}

/// 读取 rclone 里某 remote 的类型与三个代理键的当前值。
///
/// 只调一次 `config/get`：类型必须来自 rclone（权威），不能拿 UI 缓存猜；
/// 三个键一次取全，换键 / 影子键检测都不需要再发请求。
async fn read_config(
    client: &RcClient,
    remote: &str,
) -> Result<(String, ProxyConfigValues), String> {
    let map = client
        .config_get(remote)
        .await
        .map_err(|e| rcgui_i18n::f("读取 {} 的配置失败：{}", &[&(remote), &(e)]))?;
    Ok((
        ProxyConfigValues::backend_type(&map),
        ProxyConfigValues::from_config_map(&map),
    ))
}

/// 一次下发的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyOutcome {
    /// 值已经是期望的，没做任何调用
    Unchanged,
    /// 已写入新值
    Applied,
    /// 已移除（原来有代理）
    Removed,
    /// 已恢复成用户原有的值
    Restored(String),
    /// 用户自己在 rclone 里配了原生代理键：按“只补空缺”跳过，**没有写入**
    RespectExisting { key: ProxyKey },
    /// rclone 里的值被别人改过，需要用户裁决，**本次什么都没改**
    Conflict {
        key: ProxyKey,
        ours: String,
        current: String,
    },
    /// 「不代理」且本来就没被我们管过，什么都没做
    LeftAlone,
}

impl ApplyOutcome {
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    /// 本次是否真的改动了 rclone（用于统计「已重载 N 项」）。
    pub fn is_changed(&self) -> bool {
        matches!(self, Self::Applied | Self::Removed | Self::Restored(_))
    }

    /// 是否因为尊重用户已有配置而跳过。
    pub fn is_skipped(&self) -> bool {
        matches!(self, Self::RespectExisting { .. })
    }

    /// 面向用户的一句话说明（不含密码）。
    pub fn describe(&self, remote: &str) -> String {
        match self {
            Self::Unchanged => rcgui_i18n::f("{} 的代理已是最新，无需改动", &[&(remote)]),
            Self::Applied => rcgui_i18n::f("{} 的代理已生效", &[&(remote)]),
            Self::Removed => rcgui_i18n::f("{} 的代理已移除（原本没有代理）", &[&(remote)]),
            Self::Restored(_) => rcgui_i18n::f("{} 的代理已恢复为你原来设置的值", &[&(remote)]),
            Self::RespectExisting { key } => rcgui_i18n::f("{}：你在 rclone 里已经设置了 {}，本程序只补空缺、不会覆盖；如需使用这里的代理，请先在远程配置里清空它", &[&(remote), &(key.label())]),
            Self::Conflict { key, current, .. } => rcgui_i18n::f("{}：rclone 里的 {} 已被其他方式修改（当前 {}），为避免覆盖你的改动已跳过", &[&(remote), &(key.label()), &(redact_proxy_value(current))]),
            Self::LeftAlone => rcgui_i18n::f("{} 未由本程序管理，保持原样", &[&(remote)]),
        }
    }
}

/// 把代理值里的凭据部分打码后用于提示。
///
/// 代理 URL 与原生 `socks_proxy` 都可能带 `user:pass@`，而提示会进 toast、
/// 历史记录，也可能被截图 —— 不能原样透出。
pub fn redact_proxy_value(v: &str) -> String {
    if v.is_empty() {
        return rcgui_i18n::t("（空）").into();
    }
    let (prefix, rest) = match v.find("://") {
        Some(i) => (&v[..i + 3], &v[i + 3..]),
        None => ("", v),
    };
    match rest.rfind('@') {
        Some(i) => format!("{prefix}***@{}", &rest[i + 1..]),
        None => v.to_string(),
    }
}

/// 执行一个原子写操作。
async fn run_step(client: &RcClient, name: &str, step: &ProxyStep) -> Result<(), String> {
    let key = step.key().config_key();
    match step {
        ProxyStep::Set { value, .. } | ProxyStep::Restore { value, .. } => {
            let mut params = serde_json::Map::new();
            params.insert(key.to_string(), serde_json::Value::String(value.clone()));
            client
                .config_update(name, serde_json::Value::Object(params))
                .await
                .map_err(|e| rcgui_i18n::f("写入 {} 的 {} 失败：{}", &[&(name), &(key), &(e)]))
        }
        ProxyStep::Unset { .. } => client
            .config_unset(name, &[key])
            .await
            .map(|_| ())
            .map_err(|e| rcgui_i18n::f("移除 {} 的 {} 失败：{}", &[&(name), &(key), &(e)])),
    }
}

/// 把某 remote 的代理设为 `desired`（`None` = 不代理）并落到 rclone。
///
/// `binding` 会被就地更新（脚印 + 下发状态），调用方负责落盘。**即使返回 `Err`
/// 也应把 `binding` 写回**：换键时旧键可能已经成功还原，丢掉这次更新会让下一次
/// 比对把「已还原」误判成「被外部改过」。
///
/// **本函数只发 RC 请求，绝不启动/停止/重启 rclone 进程。**
pub async fn apply_to_remote(
    client: &RcClient,
    remote: &str,
    desired: Option<&ProxyProfile>,
    binding: &mut RemoteProxyBinding,
) -> Result<ApplyOutcome, String> {
    let name = remote.trim_end_matches(':');
    let (backend_type, values) = read_config(client, name).await?;

    // 决策是纯函数，放在 profile crate 里可单测
    let plan = plan_apply(desired, &backend_type, &values, Some(binding))?;

    match plan {
        ApplyPlan::Nothing { key } => {
            // 没有脚印时把「已经满足」记一笔：否则 `needs_apply()` 永远为真，
            // 每次连接都会重试一次、界面也一直显示「待应用」。
            if !binding.has_footprint() {
                binding.applied = true;
                binding.applied_key = key.config_key().to_string();
                binding.applied_value.clear();
                binding.original_override = None;
            }
            Ok(ApplyOutcome::Unchanged)
        }
        ApplyPlan::Unmanaged => Ok(ApplyOutcome::LeftAlone),
        ApplyPlan::RespectExisting { key } => Ok(ApplyOutcome::RespectExisting { key }),
        ApplyPlan::Conflict {
            key,
            ours,
            current,
        } => {
            // 什么都不改：用户的改动优先，让他在界面上确认后再操作
            Ok(ApplyOutcome::Conflict {
                key,
                ours,
                current,
            })
        }
        ApplyPlan::Steps {
            steps,
            commit,
            outcome,
        } => {
            for step in &steps {
                run_step(client, name, step).await?;
                // 换键时旧键一还原就作废脚印：否则下一步失败会留下
                // 「旧键已还原、记录还指向旧值」的假冲突。
                if step.key() != commit.key {
                    binding.clear_footprint();
                }
            }
            // 被用户配置挡住时保持“待应用”：等用户清空那个键后，
            // 下次连接/重载还能自动补上，而不是永远停在“已处理”。
            let respected = matches!(&outcome, PlanOutcome::Respect(_));
            binding.applied = !respected;
            binding.applied_key = commit.key.config_key().to_string();
            binding.applied_value = commit.value.clone();
            binding.original_override = commit.original.clone();
            Ok(match outcome {
                PlanOutcome::Applied => ApplyOutcome::Applied,
                PlanOutcome::Removed => ApplyOutcome::Removed,
                PlanOutcome::Restored(v) => ApplyOutcome::Restored(v),
                PlanOutcome::Respect(key) => ApplyOutcome::RespectExisting { key },
            })
        }
    }
}

/// 悬空绑定（指向已删除的配置）需要 UI 提示「待修复」。
pub fn dangling_remotes(cfg: &AppConfig) -> Vec<String> {
    let reg = ProxyRegistry::new(&cfg.proxies, &cfg.proxy_bindings);
    reg.dangling().iter().map(|b| b.remote.clone()).collect()
}

/// 某条代理配置正被哪些 remote 使用（删除前提示关联用）。
pub fn remotes_using(cfg: &AppConfig, proxy_id: &str) -> Vec<String> {
    let reg = ProxyRegistry::new(&cfg.proxies, &cfg.proxy_bindings);
    reg.used_by(proxy_id)
}

/// 代理改完后必须提示的说明（实测结论：后端实例在创建时读配置）。
pub fn restart_hint() -> &'static str {
    rcgui_i18n::t("代理只在远程建立连接时生效：该远程若有正在运行的挂载或服务，需要重新挂载/重新启动才会使用新代理。")
}

/// 「不代理」的准确措辞。
///
/// **不能**写成「强制直连」：rclone 仍可能继承系统 `HTTP_PROXY` / `HTTPS_PROXY`
/// 环境变量（本机实测发现默认就带全局代理变量）。
pub fn direct_hint() -> &'static str {
    rcgui_i18n::t("「不代理」= 不设置 RCGUI 的 remote 级代理。rclone 仍可能继承系统的 HTTP_PROXY / HTTPS_PROXY 环境变量，因此不等于强制直连。")
}

/// 凭据存储形式的说明（实测：百分号编码明文，连 obscure 都不是）。
pub fn credential_hint() -> &'static str {
    rcgui_i18n::t("代理账号密码会以明文（百分号编码）写入 rclone.conf，不是加密，请勿在共享环境使用敏感凭据。")
}

/// ftp / sftp 的代理键与限制说明（绑定时提示）。
///
/// 三条都是实测/上游源码确认的，不能省略：用原生键而不是 `override.http_proxy`、
/// `socks_proxy` 只认裸 `host:port`、写入后正在运行的挂载/服务不会热生效。
pub fn native_key_hint() -> &'static str {
    rcgui_i18n::t("ftp / sftp 使用 rclone 原生 http_proxy / socks_proxy 键；若该远程自己已设了代理，本程序只补空缺、不会覆盖（rclone 优先使用 socks_proxy）。SOCKS5 与 SOCKS5H 在这两类后端上等价（都由代理解析域名）；账号密码以明文写入 rclone.conf，密码不能包含「@」、用户名不能包含「:」。")
}

/// ftp / sftp 的绑定提示；其它类型返回 `None`。
pub fn native_note(rtype: &str) -> Option<&'static str> {
    uses_native_keys(rtype).then_some(native_key_hint())
}


/// 新建代理配置时的 ID 生成。
pub fn new_proxy_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------- 代理失败的友好提示 ----------------
//
// rclone 的报错原文很长且夹着 SDK 措辞（`operation error S3: ListBuckets …
// request send failed, Get "…": proxyconnect tcp: dial tcp …`），用户看不出
// 「到底该改哪里」。这里把**实测出现过**的签名映射成一句可操作的话，
// 同时保留原文供排查。
//
// 设计底线：宁可不判定，也不要误报。只有原文里出现明确的代理证据
// （`proxyconnect`、407、502）才归因给代理；普通网络故障一律原样透出。
// 实测样本见 `docs/proxy-failure-messages.md`。

/// 由 rclone 原文判定出的代理故障类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyFailure {
    /// 代理端口没人监听：`proxyconnect tcp: dial tcp … actively refused it`
    Refused,
    /// 代理要求认证：`Statuscode 407` / `ProxyAuthenticationRequired`
    AuthRequired,
    /// 代理网关上游不可达：`StatusCode: 502` / `api error BadGateway`
    BadGateway,
    /// `https://` 指向了纯 HTTP 代理：`tls: first record does not look like a TLS handshake`
    SchemeMismatch,
}

impl ProxyFailure {
    /// 简短标签，用作提示开头。
    pub fn label(self) -> &'static str {
        match self {
            Self::Refused => rcgui_i18n::t("代理拒绝连接"),
            Self::AuthRequired => rcgui_i18n::t("代理要求认证"),
            Self::BadGateway => rcgui_i18n::t("代理上游不可达"),
            Self::SchemeMismatch => rcgui_i18n::t("代理协议不匹配"),
        }
    }

    /// 下一步该做什么（一句话，不掺与代理无关的原因）。
    pub fn hint(self) -> &'static str {
        match self {
            Self::Refused => {
                rcgui_i18n::t("代理端口上没有服务在监听：请确认代理软件已启动，并核对代理配置里的地址与端口。")
            }
            Self::AuthRequired => {
                rcgui_i18n::t("代理要求用户名/密码（HTTP 407）：请在代理配置里填写账号密码，或改用免认证的代理。")
            }
            Self::BadGateway => {
                rcgui_i18n::t("代理返回 502：它连不上自己的上游或目标站点，请检查代理软件内部的上游设置。")
            }
            Self::SchemeMismatch => {
                rcgui_i18n::t("代理协议填错：https:// 只能用于支持 TLS 的代理；本机常见的 Clash / v2ray 等要用 http:// 或 socks5://。")
            }
        }
    }
}

/// 注解里标记「已经补过提示」的固定片段，用于避免重复包装。
fn annotated_mark() -> &'static str {
    rcgui_i18n::t("（rclone 原文：")
}

/// 从 rclone 原文判断代理故障；没有明确证据就返回 `None`。
pub fn classify_proxy_failure(raw: &str) -> Option<ProxyFailure> {
    let s = raw.to_ascii_lowercase();

    // 407 / ProxyAuthenticationRequired 只可能来自代理，无需再要 proxyconnect。
    if s.contains("statuscode: 407")
        || s.contains("proxyauthenticationrequired")
        || s.contains("proxy authentication required")
    {
        return Some(ProxyFailure::AuthRequired);
    }
    // 502 可能来自代理，也可能是后端自己返回的网关错误；rclone 对代理 502 的
    // 措辞是 `api error BadGateway`，据此判定并在文案里说明「代理返回」。
    if s.contains("statuscode: 502") || s.contains("api error badgateway") {
        return Some(ProxyFailure::BadGateway);
    }

    let via_proxy = s.contains("proxyconnect");
    if via_proxy && s.contains("does not look like a tls handshake") {
        return Some(ProxyFailure::SchemeMismatch);
    }
    if via_proxy && (s.contains("actively refused") || s.contains("connection refused")) {
        return Some(ProxyFailure::Refused);
    }
    None
}

/// 是否像一次网络层失败（用于「该远程绑了代理」时的弱归因）。
fn looks_like_network_failure(raw: &str) -> bool {
    let s = raw.to_ascii_lowercase();
    // 这些片段对「代理不通」和「远端本身不可达」都成立，只能用来提示，
    // 不能用来断言是谁的问题。
    const SIGNS: [&str; 6] = [
        "request send failed",
        "timeout",
        "eof",
        "connection reset",
        "no such host",
        "network is unreachable",
    ];
    SIGNS.iter().any(|k| s.contains(k))
}

/// 给一条错误补上代理相关的中文提示。
///
/// - 命中实测签名 → `【代理】<标签>：<怎么办>（rclone 原文：…）`
/// - 未命中但有代理上下文，且看着像网络失败 → 弱提示「可能与代理有关」
/// - 其它情况原样返回，绝不改写无关错误
///
/// 已带注解的文本直接原样返回，因此可以安全地在多个层重复调用。
pub fn annotate_error(raw: &str, proxy_endpoint: Option<&str>) -> String {
    if raw.contains(annotated_mark()) {
        return raw.to_string();
    }
    if let Some(f) = classify_proxy_failure(raw) {
        return rcgui_i18n::f("【代理】{}：{}{}{}）", &[&(f.label()), &(f.hint()), &(annotated_mark()), &(raw)]);
    }
    if let Some(ep) = proxy_endpoint {
        if looks_like_network_failure(raw) {
            return rcgui_i18n::f("该远程绑定了代理 {}，本次失败可能与代理有关；若代理正常，请再查远程自身配置。{}{}）", &[&(ep), &(annotated_mark()), &(raw)]);
        }
    }
    raw.to_string()
}

/// 检查代理配置名称是否与**其他**配置重复。
///
/// `exclude_id` 是正在编辑的那条自己的 id（新建时传空串）：
/// 编辑时允许保留原名，否则「只改端口」这种操作会被自己的名字判成重名。
///
/// 比较用 `eq_ignore_ascii_case`：用户不该因为大小写差异就能存出两个看起来
/// 一样的配置，而 rclone 的 remote 名也向来不区分大小写。
pub fn proxy_name_conflict(
    proxies: &[rcgui_profile::ProxyProfile],
    name: &str,
    exclude_id: &str,
) -> Option<String> {
    let want = name.trim();
    proxies
        .iter()
        .find(|p| {
            p.id != exclude_id && p.name.trim().eq_ignore_ascii_case(want) && !want.is_empty()
        })
        .map(|p| p.name.clone())
}

/// 检查服务显示名是否与**其他**服务重复。
///
/// `serve_names` 是「协议+远程+子路径+地址 → 显示名」的映射表。
/// 同一个显示名被两条不同记录使用会让界面上出现两个「Serve_01」，无法区分。
pub fn serve_name_conflict(
    names: &[crate::config::ServeName],
    name: &str,
    exclude_addr: &str,
) -> Option<String> {
    let want = name.trim();
    names
        .iter()
        .find(|n| {
            n.addr != exclude_addr && n.name.trim().eq_ignore_ascii_case(want) && !want.is_empty()
        })
        .map(|n| n.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgui_profile::proxy::ProxyProtocol;

    fn prof(id: &str) -> ProxyProfile {
        ProxyProfile {
            id: id.into(),
            name: "p".into(),
            protocol: ProxyProtocol::Http,
            host: "127.0.0.1".into(),
            port: 8080,
            user: String::new(),
            pass: String::new(),
        }
    }

    fn cfg_with(proxies: Vec<ProxyProfile>, bindings: Vec<RemoteProxyBinding>) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.proxies = proxies;
        cfg.proxy_bindings = bindings;
        cfg
    }

    #[test]
    fn bound_profile_resolves() {
        let cfg = cfg_with(
            vec![prof("p1")],
            vec![RemoteProxyBinding {
                remote: "teldrive".into(),
                proxy_id: "p1".into(),
                ..Default::default()
            }],
        );
        assert_eq!(bound_profile(&cfg, "teldrive").map(|p| p.id), Some("p1".into()));
        assert!(bound_profile(&cfg, "other").is_none());
    }

    #[test]
    fn dangling_and_usage_helpers() {
        let cfg = cfg_with(
            vec![prof("p1")],
            vec![
                RemoteProxyBinding { remote: "a".into(), proxy_id: "p1".into(), ..Default::default() },
                RemoteProxyBinding { remote: "b".into(), proxy_id: "gone".into(), ..Default::default() },
            ],
        );
        assert_eq!(dangling_remotes(&cfg), vec!["b".to_string()]);
        assert_eq!(remotes_using(&cfg, "p1"), vec!["a".to_string()]);
        assert!(remotes_using(&cfg, "p2").is_empty());
    }

    #[test]
    fn hints_do_not_overpromise() {
        // 「不代理」的文案不得出现「强制直连」这类误导说法
        assert!(direct_hint().contains("不等于强制直连"));
        assert!(direct_hint().contains("HTTP_PROXY"));
        // 凭据说明必须点明是明文、不是加密
        assert!(credential_hint().contains("明文"));
        assert!(credential_hint().contains("不是加密"));
        assert!(!credential_hint().contains("obscure 加密"));
        // 重启提示必须说清「需要重新挂载/启动」
        assert!(restart_hint().contains("重新挂载"));
        assert!(restart_hint().contains("重新启动"));
        // ftp/sftp 的说明必须点明原生键、只补空缺、凭据限制
        assert!(native_key_hint().contains("原生 http_proxy"));
        assert!(native_key_hint().contains("只补空缺"));
        assert!(native_key_hint().contains("socks_proxy"));
        assert!(native_key_hint().contains("「@」"));
    }

    /// 只有 ftp / sftp 需要原生键说明；其它类型不能误导用户。
    #[test]
    fn native_note_only_for_ftp_and_sftp() {
        assert!(native_note("ftp").is_some());
        assert!(native_note("  SFTP ").is_some());
        for t in ["webdav", "s3", "drive", "local", ""] {
            assert!(native_note(t).is_none(), "{t} 不该出现原生键说明");
        }
    }

    #[test]
    fn new_ids_are_unique_and_stable_length() {
        let a = new_proxy_id();
        let b = new_proxy_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36, "UUID v4 字符串应为 36 字符");
    }

    // ---------------- 重名检测 ----------------

    fn pname(id: &str, name: &str) -> rcgui_profile::ProxyProfile {
        rcgui_profile::ProxyProfile {
            id: id.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn duplicate_proxy_name_is_rejected() {
        let list = vec![pname("a", "公司代理"), pname("b", "家里代理")];
        // 新建时与已有名字冲突
        assert!(proxy_name_conflict(&list, "公司代理", "").is_some());
        // 大小写/首尾空格不应绕过检测
        assert!(proxy_name_conflict(&list, "  公司代理 ", "").is_some());
        assert!(proxy_name_conflict(&list, "ABC", "").is_none());
        // 不冲突
        assert!(proxy_name_conflict(&list, "新代理", "").is_none());
    }

    /// 编辑时保留自己的名字不能算重名，否则「只改端口」会失败。
    #[test]
    fn editing_self_name_is_allowed() {
        let list = vec![pname("a", "公司代理"), pname("b", "家里代理")];
        assert!(
            proxy_name_conflict(&list, "公司代理", "a").is_none(),
            "编辑 a 且名字没改，不应报重名"
        );
        // 但改成别人的名字仍要拦
        assert!(proxy_name_conflict(&list, "家里代理", "a").is_some());
    }

    #[test]
    fn empty_name_never_conflicts() {
        // 空名由 validate() 拦截，这里不重复报错
        let list = vec![pname("a", "x")];
        assert!(proxy_name_conflict(&list, "", "").is_none());
        assert!(proxy_name_conflict(&list, "   ", "").is_none());
    }

    fn sname(addr: &str, name: &str) -> crate::config::ServeName {
        crate::config::ServeName {
            stype: "webdav".into(),
            remote: "r".into(),
            sub_path: String::new(),
            addr: addr.into(),
            name: name.into(),
        }
    }

    #[test]
    fn duplicate_serve_name_is_rejected() {
        let list = vec![sname("127.0.0.1:8080", "Serve_01")];
        assert!(serve_name_conflict(&list, "Serve_01", "127.0.0.1:8081").is_some());
        // 同一地址（编辑自己）不算重名
        assert!(serve_name_conflict(&list, "Serve_01", "127.0.0.1:8080").is_none());
        // 不同名字
        assert!(serve_name_conflict(&list, "Serve_02", "127.0.0.1:8081").is_none());
    }

    #[test]
    fn outcome_is_failure_only_for_conflict() {
        assert!(ApplyOutcome::Conflict {
            key: ProxyKey::Override,
            ours: "a".into(),
            current: "b".into()
        }
        .is_failure());
        assert!(!ApplyOutcome::Applied.is_failure());
        assert!(!ApplyOutcome::Unchanged.is_failure());
        assert!(!ApplyOutcome::LeftAlone.is_failure());
        assert!(!ApplyOutcome::RespectExisting { key: ProxyKey::NativeSocks }.is_failure());
        // 只有真正写过的才算「已重载」
        assert!(ApplyOutcome::Applied.is_changed());
        assert!(ApplyOutcome::Removed.is_changed());
        assert!(ApplyOutcome::Restored("x".into()).is_changed());
        assert!(!ApplyOutcome::Unchanged.is_changed());
        assert!(!ApplyOutcome::LeftAlone.is_changed());
        assert!(ApplyOutcome::RespectExisting { key: ProxyKey::NativeHttp }.is_skipped());
    }

    /// 冲突提示必须把凭据打码：它会进 toast、历史记录，也可能被截图。
    #[test]
    fn conflict_description_redacts_credentials() {
        let d = ApplyOutcome::Conflict {
            key: ProxyKey::Override,
            ours: "http://u:secret@h:1".into(),
            current: "http://u:secret2@h:2".into(),
        }
        .describe("teldrive");
        assert!(d.contains("已被其他方式修改"));
        assert!(!d.contains("secret"), "密码不得出现在提示里：{d}");
        assert!(d.contains("***@h:2"), "主机可以保留：{d}");
        // 原生 socks 的裸 user:pass@ 形式同样要打码
        assert_eq!(
            redact_proxy_value("bob:pw@127.0.0.1:1080"),
            "***@127.0.0.1:1080"
        );
        assert_eq!(redact_proxy_value(""), "（空）");
        assert_eq!(redact_proxy_value("http://h:1"), "http://h:1");
    }

    #[test]
    fn respect_message_does_not_leak_values() {
        let d = ApplyOutcome::RespectExisting {
            key: ProxyKey::NativeSocks,
        }
        .describe("ftp1");
        assert!(d.contains("只补空缺"));
        assert!(d.contains("socks_proxy"));
        assert!(!d.contains("user:pass"));
    }

    // ---------------- 代理失败提示 ----------------
    //
    // 下面每条原文都是 rclone v1.75.4 实测抓到的（见 docs/proxy-failure-messages.md），
    // 不要为了「看起来更整齐」而改写这些字符串：它们就是判定依据。

    /// HTTP 代理端口没人监听。
    const SAMPLE_HTTP_REFUSED: &str = r#"无法进入 mywebdav: operations/list: error in ListJSON: operation error S3: ListBuckets, exceeded maximum number of attempts, 1, https response error StatusCode: 0, RequestID: , HostID: , request send failed, Get "http://127.0.0.1:9/?x-id=ListBuckets": proxyconnect tcp: dial tcp 127.0.0.1:9: connectex: No connection could be made because the target machine actively refused it."#;

    /// SOCKS5 代理端口没人监听（文案与 HTTP 一致，靠 scheme 区分，故同样归类）。
    const SAMPLE_SOCKS_REFUSED: &str = r#"error listing: operation error S3: ListBuckets, http response error StatusCode: 0, request send failed, Get "http://127.0.0.1:9/": proxyconnect tcp: dial tcp 127.0.0.1:55890: connectex: No connection could be made because the target machine actively refused it."#;

    /// 代理要求认证。
    const SAMPLE_407: &str = r#"error listing: operation error S3: ListBuckets, https response error StatusCode: 407, RequestID: , HostID: , api error ProxyAuthenticationRequired: Proxy Authentication Required"#;

    /// 代理网关上游不可达。
    const SAMPLE_502: &str = r#"error listing: operation error S3: ListBuckets, exceeded maximum number of attempts, 1, https response error StatusCode: 502, RequestID: , HostID: , api error BadGateway: Bad Gateway"#;

    /// `https://` 代理其实是个明文 HTTP 代理。
    const SAMPLE_TLS_MISMATCH: &str = r#"error listing: operation error S3: ListBuckets, exceeded maximum number of attempts, 1, https response error StatusCode: 0, RequestID: , HostID: , request send failed, Get "http://127.0.0.1:9/?x-id=ListBuckets": proxyconnect tcp: tls: first record does not look like a TLS handshake"#;

    #[test]
    fn refused_proxy_is_recognized_for_http_and_socks5() {
        assert_eq!(classify_proxy_failure(SAMPLE_HTTP_REFUSED), Some(ProxyFailure::Refused));
        assert_eq!(classify_proxy_failure(SAMPLE_SOCKS_REFUSED), Some(ProxyFailure::Refused));
    }

    #[test]
    fn auth_and_gateway_failures_are_recognized() {
        assert_eq!(classify_proxy_failure(SAMPLE_407), Some(ProxyFailure::AuthRequired));
        assert_eq!(classify_proxy_failure(SAMPLE_502), Some(ProxyFailure::BadGateway));
    }

    #[test]
    fn tls_handshake_mismatch_is_recognized() {
        assert_eq!(
            classify_proxy_failure(SAMPLE_TLS_MISMATCH),
            Some(ProxyFailure::SchemeMismatch)
        );
    }

    /// 目标站点自己拒绝连接（没有走代理）时**不能**归因给代理。
    #[test]
    fn plain_origin_refusal_is_not_blamed_on_proxy() {
        let raw = r#"error listing: dial tcp 127.0.0.1:9000: connectex: No connection could be made because the target machine actively refused it."#;
        assert_eq!(classify_proxy_failure(raw), None);
        assert_eq!(annotate_error(raw, None), raw);
        // 即使该远程绑定了代理，没有网络失败特征也不该硬扯上代理
        let unrelated = "无法进入 mywebdav: remote not found";
        assert_eq!(annotate_error(unrelated, Some("127.0.0.1:8080")), unrelated);
    }

    /// 已知的判定边界：超时/EOF 这类文本里没有代理证据，不做强归因。
    #[test]
    fn timeout_without_proxy_evidence_stays_raw() {
        let raw = r#"operation error S3: ListBuckets, request send failed, Get "http://127.0.0.1:9/": net/http: timeout awaiting response headers"#;
        assert_eq!(classify_proxy_failure(raw), None);
        assert_eq!(annotate_error(raw, None), raw);
        // 但知道「该远程绑了代理」时会给一句弱提示，不冒充结论
        let weak = annotate_error(raw, Some("127.0.0.1:8080"));
        assert!(weak.contains("可能与代理有关"));
        assert!(weak.contains("127.0.0.1:8080"));
        // 原文必须保留，便于排查
        assert!(weak.contains("timeout awaiting response headers"));
    }

    #[test]
    fn annotate_keeps_raw_text_and_is_idempotent() {
        let once = annotate_error(SAMPLE_407, None);
        assert!(once.starts_with("【代理】代理要求认证："));
        assert!(once.contains("ProxyAuthenticationRequired"), "原文必须保留");
        // 二次调用不得再套一层
        let twice = annotate_error(&once, None);
        assert_eq!(once, twice);
        // 弱提示同样幂等
        let weak = annotate_error("request send failed: EOF", Some("h:1"));
        assert_eq!(annotate_error(&weak, Some("h:1")), weak);
    }
}
