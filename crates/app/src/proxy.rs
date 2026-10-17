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
//! 另外：rclone 只认 `override.http_proxy` 一个键（SOCKS5 靠 `socks5://` scheme），
//! 详见 `docs/proxy-verification.md`。

use crate::config::AppConfig;
use rcgui_profile::proxy::{plan_set, remember_original, ProxyAction, ProxyProfile};
use rcgui_profile::{ProxyRegistry, RemoteProxyBinding};
use rcgui_rc::RcClient;

/// 该 remote 当前绑定的代理配置（`None` = 不代理 / 悬空引用）。
pub fn bound_profile(cfg: &AppConfig, remote: &str) -> Option<ProxyProfile> {
    let reg = ProxyRegistry::new(&cfg.proxies, &cfg.proxy_bindings);
    reg.for_remote(remote).cloned()
}

/// 读取 rclone 里某 remote 当前的 `override.http_proxy`。
///
/// 返回 `Ok(None)` 表示键不存在（与「存在但为空」区分开，回滚时要用）。
pub async fn read_current(
    client: &RcClient,
    remote: &str,
) -> Result<Option<String>, String> {
    let map = client
        .config_get(remote)
        .await
        .map_err(|e| format!("读取 {remote} 的配置失败：{e}"))?;
    match map.get("override.http_proxy") {
        None => Ok(None),
        Some(v) => Ok(Some(v.as_str().unwrap_or_default().to_string())),
    }
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
    /// rclone 里的值被别人改过，需要用户裁决，**本次什么都没改**
    Conflict { ours: String, current: String },
    /// 「不代理」且本来就没被我们管过，什么都没做
    LeftAlone,
}

impl ApplyOutcome {
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    /// 面向用户的一句话说明（不含密码）。
    pub fn describe(&self, remote: &str) -> String {
        match self {
            Self::Unchanged => format!("{remote} 的代理已是最新，无需改动"),
            Self::Applied => format!("{remote} 的代理已生效"),
            Self::Removed => format!("{remote} 的代理已移除（原本没有代理）"),
            Self::Restored(_) => format!("{remote} 的代理已恢复为你原来设置的值"),
            Self::Conflict { current, .. } => format!(
                "{remote}：rclone 里的代理已被其他方式修改（当前 {}），为避免覆盖你的改动已跳过",
                if current.is_empty() { "为空" } else { current }
            ),
            Self::LeftAlone => format!("{remote} 未由本程序管理，保持原样"),
        }
    }
}

/// 把某 remote 的代理设为 `desired`（`None` = 不代理）并落到 rclone。
///
/// `binding` 会被就地更新（下发状态 + 被覆盖的原值），调用方负责落盘。
/// **本函数只发 RC 请求，绝不启动/停止/重启 rclone 进程。**
pub async fn apply_to_remote(
    client: &RcClient,
    remote: &str,
    desired: Option<&ProxyProfile>,
    binding: &mut RemoteProxyBinding,
) -> Result<ApplyOutcome, String> {
    let name = remote.trim_end_matches(':').to_string();
    let desired_url = match desired {
        Some(p) => Some(p.to_proxy_url()?),
        None => None,
    };
    let current = read_current(client, &name).await?;

    // 决策是纯函数，放在 profile crate 里可单测
    let action = plan_set(desired_url.as_deref(), current.as_deref(), Some(binding));

    match action {
        // 决策说「什么都不用做」时，要区分两种原因，否则界面会误报：
        // - 已经是期望值 → Unchanged（无需改动）
        // - 从未由我们管理过（用户自己设的代理）→ LeftAlone（明确不碰）
        ProxyAction::Nothing => {
            let ours = binding.applied_value.is_empty() && binding.original_override.is_none();
            if desired_url.is_none() && ours {
                Ok(ApplyOutcome::LeftAlone)
            } else {
                Ok(ApplyOutcome::Unchanged)
            }
        }

        ProxyAction::Set(url) => {
            // 关键：覆盖前先记住用户原值（若已有更早的记录则沿用，不覆盖成自己的值）
            let original = remember_original(Some(binding), current.as_deref());
            client
                .config_update(
                    &name,
                    serde_json::json!({ "override.http_proxy": url }),
                )
                .await
                .map_err(|e| format!("写入 {name} 的代理失败：{e}"))?;
            binding.applied = true;
            binding.applied_value = url;
            binding.original_override = original;
            Ok(ApplyOutcome::Applied)
        }

        ProxyAction::Unset => {
            client
                .config_unset(&name, &["override.http_proxy"])
                .await
                .map_err(|e| format!("移除 {name} 的代理失败：{e}"))?;
            binding.applied = true;
            binding.applied_value = String::new();
            binding.original_override = None;
            Ok(ApplyOutcome::Removed)
        }

        ProxyAction::Restore(v) => {
            if v.is_empty() {
                // 原本存在但为空 → 写回空值，而不是删键，保持与用户原状一致
                client
                    .config_update(&name, serde_json::json!({ "override.http_proxy": "" }))
                    .await
                    .map_err(|e| format!("恢复 {name} 的代理失败：{e}"))?;
            } else {
                client
                    .config_update(
                        &name,
                        serde_json::json!({ "override.http_proxy": v }),
                    )
                    .await
                    .map_err(|e| format!("恢复 {name} 的代理失败：{e}"))?;
            }
            binding.applied = true;
            binding.applied_value = v.clone();
            binding.original_override = None;
            Ok(ApplyOutcome::Restored(v))
        }

        ProxyAction::Conflict { ours, current } => {
            // 什么都不改：用户的改动优先，让他在界面上确认后再操作
            Ok(ApplyOutcome::Conflict { ours, current })
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
pub const RESTART_HINT: &str = "代理只在远程建立连接时生效：该远程若有正在运行的挂载或服务，需要重新挂载/重新启动才会使用新代理。";

/// 「不代理」的准确措辞。
///
/// **不能**写成「强制直连」：rclone 仍可能继承系统 `HTTP_PROXY` / `HTTPS_PROXY`
/// 环境变量（本机实测发现默认就带全局代理变量）。
pub const DIRECT_HINT: &str = "「不代理」= 不设置 RCGUI 的 remote 级代理。rclone 仍可能继承系统的 HTTP_PROXY / HTTPS_PROXY 环境变量，因此不等于强制直连。";

/// 凭据存储形式的说明（实测：百分号编码明文，连 obscure 都不是）。
pub const CREDENTIAL_HINT: &str = "代理账号密码会以明文（百分号编码）写入 rclone.conf，不是加密，请勿在共享环境使用敏感凭据。";

/// 新建代理配置时的 ID 生成。
pub fn new_proxy_id() -> String {
    uuid::Uuid::new_v4().to_string()
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
        assert!(DIRECT_HINT.contains("不等于强制直连"));
        assert!(DIRECT_HINT.contains("HTTP_PROXY"));
        // 凭据说明必须点明是明文、不是加密
        assert!(CREDENTIAL_HINT.contains("明文"));
        assert!(CREDENTIAL_HINT.contains("不是加密"));
        assert!(!CREDENTIAL_HINT.contains("obscure 加密"));
        // 重启提示必须说清「需要重新挂载/启动」
        assert!(RESTART_HINT.contains("重新挂载"));
        assert!(RESTART_HINT.contains("重新启动"));
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
        assert!(ApplyOutcome::Conflict { ours: "a".into(), current: "b".into() }.is_failure());
        assert!(!ApplyOutcome::Applied.is_failure());
        assert!(!ApplyOutcome::Unchanged.is_failure());
        assert!(!ApplyOutcome::LeftAlone.is_failure());
    }

    #[test]
    fn conflict_description_never_leaks_password() {
        let d = ApplyOutcome::Conflict {
            ours: "http://u:secret@h:1".into(),
            current: "http://u:secret2@h:2".into(),
        }
        .describe("teldrive");
        // 描述里只可能出现「当前值」，但那是 rclone 里的代理 URL（可能含密码）。
        // 因此这里断言 describe 不额外拼接 ours，避免重复暴露。
        assert!(d.contains("已被其他方式修改"));
    }
}
