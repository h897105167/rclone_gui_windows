//! 连接握手：一次成功的连接必须能回答三个问题。
//!
//! 1. 对端是什么版本（`core/version`）—— 低于门槛就明确告知「版本过旧」；
//! 2. 它实际在用哪个配置文件（`config/paths`）—— 绝不假设 `%APPDATA%\rclone\rclone.conf`；
//! 3. 它支持哪些 RC 方法（`rc/list`）—— 缺失接口明确报「不支持」，而不是通用失败。
//!
//! 设计取舍：除了「连不上 / 鉴权失败」这类硬失败，其余问题都收集到结构体里返回，
//! 由 UI 决定是阻断、降级还是只提示。这样「老版本 rclone」也能部分可用。

use crate::client::RcClient;
use crate::error::RcError;
use crate::types::{ConfigPaths, VersionInfo};

/// 最低支持版本（低于此版本按「版本过旧」处理）。
pub const MIN_MAJOR: u32 = 1;
pub const MIN_MINOR: u32 = 60;

/// 一个 RC 方法要求：`name` 及其历史别名满足其一即可。
pub struct MethodReq {
    pub name: &'static str,
    pub alternatives: &'static [&'static str],
    /// 缺失时是否阻断主功能（远程/挂载/服务/配置 四类核心操作）。
    pub required: bool,
    /// 面向用户的能力名（用于拼提示）。
    pub capability: &'static str,
}

/// 主功能所需的方法表。
pub const METHOD_REQS: &[MethodReq] = &[
    MethodReq { name: "config/listremotes", alternatives: &[], required: true, capability: "列出远程" },
    MethodReq { name: "config/create", alternatives: &[], required: true, capability: "新建远程" },
    MethodReq { name: "config/update", alternatives: &[], required: true, capability: "修改远程" },
    MethodReq { name: "config/delete", alternatives: &[], required: true, capability: "删除远程" },
    // rclone 1.75 起为 mount/listmounts，更早版本是 mount/list
    MethodReq {
        name: "mount/listmounts",
        alternatives: &["mount/list"],
        required: true,
        capability: "读取挂载列表",
    },
    MethodReq { name: "mount/mount", alternatives: &[], required: true, capability: "挂载" },
    MethodReq { name: "mount/unmount", alternatives: &[], required: true, capability: "卸载" },
    MethodReq { name: "serve/list", alternatives: &[], required: true, capability: "读取服务列表" },
    MethodReq { name: "serve/start", alternatives: &[], required: true, capability: "启动服务" },
    MethodReq { name: "serve/stop", alternatives: &[], required: true, capability: "停止服务" },
    // 以下缺失只影响体验，不阻断
    MethodReq { name: "config/paths", alternatives: &[], required: false, capability: "读取配置文件路径" },
    MethodReq { name: "config/providers", alternatives: &[], required: false, capability: "动态参数表单" },
    // 代理功能需要：config/get 读现有值、config/unset 移除代理。
    // 做成非阻断：老版本缺这两个方法时，远程/挂载/服务仍可用，
    // 只是「按 remote 分配代理」这项能力要明确报「不支持」而不是通用失败。
    MethodReq { name: "config/get", alternatives: &[], required: false, capability: "读取单条远程配置（代理功能依赖）" },
    MethodReq { name: "config/unset", alternatives: &[], required: false, capability: "移除远程代理（代理功能依赖）" },
    MethodReq { name: "options/get", alternatives: &[], required: false, capability: "读取运行参数" },
    MethodReq { name: "options/set", alternatives: &[], required: false, capability: "修改运行参数" },
    MethodReq { name: "core/bwlimit", alternatives: &[], required: false, capability: "全局限速" },
    MethodReq { name: "core/transferred", alternatives: &[], required: false, capability: "后台任务列表" },
    MethodReq { name: "job/stop", alternatives: &[], required: false, capability: "停止任务" },
    MethodReq { name: "operations/about", alternatives: &[], required: false, capability: "远程容量/连通性" },
];

/// 握手结果。`Err` 只用于「连不上 / 鉴权失败」等无法继续的情况。
#[derive(Debug, Clone, Default)]
pub struct Handshake {
    pub version: VersionInfo,
    /// 缺失时为 None（该版本不支持 `config/paths`）。
    pub config: Option<ConfigPaths>,
    /// 缺失时为 None（该版本不支持 `rc/list`，此时无法做特性探测）。
    pub methods: Option<Vec<String>>,
    /// 阻断性缺失：`(能力名, 期望方法)`。
    pub missing_required: Vec<(String, String)>,
    /// 非阻断问题：版本偏低、可选方法缺失、`rc/list` 不可用。
    pub warnings: Vec<String>,
    /// 版本低于门槛。
    pub too_old: bool,
    /// 对端**没有配置** RC 鉴权：除 `NoAuth` 接口（`core/version`、`rc/list`…）外
    /// 全部返回 403，挂载/服务/参数读取都不可用。属于阻断状态。
    pub auth_unset: bool,
}

impl Handshake {
    /// 执行握手。只有 `core/version` 失败才返回 `Err`（端点不可达或鉴权失败）。
    pub async fn perform(client: &RcClient) -> Result<Handshake, RcError> {
        let version = client.version().await?;
        let mut hs = Handshake {
            version,
            ..Default::default()
        };

        if !hs.version.at_least(MIN_MAJOR, MIN_MINOR) {
            hs.too_old = true;
            hs.warnings.push(format!(
                "rclone {} 低于建议版本 {}.{}，部分接口可能缺失",
                if hs.version.version.is_empty() { "（未知版本）" } else { &hs.version.version },
                MIN_MAJOR,
                MIN_MINOR
            ));
        }

        // 鉴权探测：`core/version` 是 NoAuth 接口，它答得上来**不代表能干活**。
        //
        // 对端没配置 RC 鉴权时，除少数 NoAuth 接口外全部 403。不在这里判定，就会
        // 表现成「握手成功，然后挂载 / 服务 / 参数读取零散失败」——实测踩过：
        // 5 项自动恢复 + options/get + config/paths 一起报
        // `authentication must be set up on the rc server…`。
        let mut auth_unset = false;
        match client.noopauth().await {
            Ok(_) => {}
            Err(e) if e.is_auth_not_configured() => auth_unset = true,
            // 老版本没有 rc/noopauth：探测不了，不能当成「没配鉴权」（否则会把好实例判死）
            Err(e) if e.is_unsupported() => {}
            Err(e) => hs.warnings.push(format!("鉴权探测失败：{}", e.describe())),
        }
        if auth_unset {
            // 后面每个调用都会 403，索性只报这一个原因，不再堆一串误导性的
            // 「读取配置文件路径：鉴权失败」「该实例不支持特性探测」。
            hs.auth_unset = true;
            hs.warnings.push(
                "对端 rclone 没有配置 RC 鉴权：除 core/version 外的接口都会返回 403，\
                 挂载、服务、运行参数全部不可用。请用 --rc-user/--rc-pass 或 --rc-no-auth \
                 重启 rclone 后重新握手（内置插件「一键 rclone」默认会带上用户名）"
                    .to_string(),
            );
            return Ok(hs);
        }

        // 配置路径：可选能力，失败不阻断
        match client.config_paths().await {
            Ok(p) => hs.config = Some(p),
            Err(e) => hs.warnings.push(format!("无法读取配置文件路径：{}", e.kind.label())),
        }

        // 方法表：老版本可能没有 rc/list
        match client.rc_methods().await {
            Ok(list) => {
                let missing: Vec<(String, String)> = METHOD_REQS
                    .iter()
                    .filter(|r| !has_any(&list, r))
                    .map(|r| {
                        (
                            r.capability.to_string(),
                            if r.alternatives.is_empty() {
                                r.name.to_string()
                            } else {
                                format!("{} 或 {}", r.name, r.alternatives.join(" / "))
                            },
                        )
                    })
                    .collect();
                for (cap, method) in &missing {
                    let required = METHOD_REQS
                        .iter()
                        .find(|r| r.capability == cap.as_str())
                        .map(|r| r.required)
                        .unwrap_or(false);
                    if required {
                        hs.missing_required.push((cap.clone(), method.clone()));
                    } else {
                        hs.warnings.push(format!("缺少「{cap}」能力（需要 {method}）"));
                    }
                }
                hs.methods = Some(list);
            }
            Err(e) => hs
                .warnings
                .push(format!("该实例不支持特性探测（rc/list：{}）", e.kind.label())),
        }

        Ok(hs)
    }

    /// 主功能是否可用（版本、必需方法、对端鉴权都满足）。
    ///
    /// `auth_unset` 也是阻断项：对端没配置 RC 鉴权时，挂载、服务、参数读取
    /// 全部 403，UI 必须说清「连上了但用不了」，而不是显示成正常连接。
    pub fn is_compatible(&self) -> bool {
        !self.too_old && !self.auth_unset && self.missing_required.is_empty()
    }

    /// 判断某方法是否可用。方法表未知时返回 true（不做无谓阻断）。
    pub fn supports(&self, method: &str) -> bool {
        match &self.methods {
            None => true,
            Some(list) => list.iter().any(|m| m == method),
        }
    }

    /// 「按 remote 分配代理」这项能力是否可用。
    ///
    /// 依赖 `config/update`（写代理，属必需能力）+ `config/get`（读原值做回滚）
    /// + `config/unset`（移除代理）。三者缺一，都不能安全地实现
    /// 「不静默覆盖用户原有代理」这条要求 —— 宁可明说「不支持」。
    ///
    /// 返回 `Err` 是**面向用户**的中文原因，可直接进 toast。
    pub fn proxy_capability(&self) -> Result<(), String> {
        let mut missing: Vec<&str> = Vec::new();
        if !self.supports("config/update") {
            missing.push("config/update");
        }
        if !self.supports("config/get") {
            missing.push("config/get");
        }
        if !self.supports("config/unset") {
            missing.push("config/unset");
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "该 rclone 实例缺少 {}，无法按远程分配代理（升级 rclone 后重试）",
                missing.join(" / ")
            ))
        }
    }

    pub fn config_path(&self) -> String {
        self.config
            .as_ref()
            .map(|p| p.config.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
    }

    /// 一行摘要（用于 toast / 状态栏）。
    pub fn summary(&self) -> String {
        let ver = if self.version.version.is_empty() {
            "未知版本".to_string()
        } else {
            self.version.version.clone()
        };
        let mut s = format!("已连接 rclone {ver}");
        if let Some(n) = self.methods.as_ref().map(|m| m.len()) {
            s.push_str(&format!(" · {n} 个 RC 方法"));
        }
        if self.auth_unset {
            s.push_str(" · 对端未配置 RC 鉴权");
        }
        if !self.missing_required.is_empty() {
            s.push_str(&format!(" · 缺 {} 项必需能力", self.missing_required.len()));
        }
        s
    }

    /// 缺失能力的用户可读列表，如「挂载、停止服务」。
    pub fn missing_text(&self) -> String {
        self.missing_required
            .iter()
            .map(|(cap, _)| cap.as_str())
            .collect::<Vec<_>>()
            .join("、")
    }
}

fn has_any(list: &[String], req: &MethodReq) -> bool {
    std::iter::once(req.name)
        .chain(req.alternatives.iter().copied())
        .any(|m| list.iter().any(|x| x == m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hs_with(methods: Option<&[&str]>, version: (u32, u32)) -> Handshake {
        Handshake {
            version: VersionInfo {
                version: format!("v{}.{}.0", version.0, version.1),
                decomposed: vec![version.0, version.1, 0],
                ..Default::default()
            },
            methods: methods.map(|m| m.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn supports_returns_true_when_methods_unknown() {
        let hs = hs_with(None, (1, 75));
        assert!(hs.supports("anything"));
        assert!(hs.is_compatible());
    }

    /// 对端没配置鉴权是**阻断**状态：即使方法表里挂载/服务接口齐全也不能算可用。
    ///
    /// 实测场景：一键 rclone 启动的无鉴权实例，`core/version` 与 `rc/list` 都正常
    /// （NoAuth 接口），但 5 项自动恢复全部 403。旧代码在这里判成「已连接」。
    #[test]
    fn auth_unset_blocks_compatibility() {
        let mut hs = hs_with(Some(&["mount/mount", "serve/start", "config/listremotes"]), (1, 75));
        assert!(hs.is_compatible());
        hs.auth_unset = true;
        assert!(!hs.is_compatible(), "对端未配置 RC 鉴权时必须判为不可用");
        assert!(hs.summary().contains("未配置 RC 鉴权"), "摘要要点明原因：{}", hs.summary());
    }

    /// 起一个假的 RC 服务器，`core/version` 永远答 200，其余接口按 `mode` 回答：
    /// - `noauth`：403 + rclone 原文（服务端**没有配置**鉴权，这正是故障现场）
    /// - `ok`：200 `{}`（配置了鉴权、凭据也对）
    /// - `old`：404（老版本根本没有 `rc/noopauth` 这个方法）
    fn spawn_fake_rc(mode: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut sock) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let (status, body) = if path.ends_with("/core/version") {
                    (
                        "200 OK",
                        r#"{"version":"v1.76.0","decomposed":[1,76,0]}"#.to_string(),
                    )
                } else if path.ends_with("/rc/noopauth") && mode == "old" {
                    (
                        "404 Not Found",
                        r#"{"error":"couldn't find method \"rc/noopauth\""}"#.to_string(),
                    )
                } else if mode == "noauth" {
                    (
                        "403 Forbidden",
                        r#"{"error":"authentication must be set up on the rc server to use \"this method\" or the --rc-no-auth flag must be in use"}"#
                            .to_string(),
                    )
                } else {
                    ("200 OK", "{}".to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    /// 现场复现：服务端只配了 `--rc-addr`（无鉴权）时，`core/version`（NoAuth 接口）
    /// 照常 200，但握手必须判定「连上了也用不了」，而不是报成正常连接。
    #[tokio::test]
    async fn handshake_flags_server_without_auth() {
        let url = spawn_fake_rc("noauth");
        let c = RcClient::new(url, None, None);
        let hs = Handshake::perform(&c)
            .await
            .expect("core/version 能答上来，就不该报成连接失败");
        assert!(hs.auth_unset, "必须判定为「对端未配置鉴权」");
        assert!(!hs.is_compatible(), "这种实例挂载/服务/参数全做不了，不能算可用");
        assert!(
            hs.warnings.iter().any(|w| w.contains("没有配置 RC 鉴权")),
            "必须给出可操作提示，实际: {:?}",
            hs.warnings
        );
        assert!(hs.methods.is_none(), "已判定不可用，不必再做特性探测");
    }

    /// 配置了鉴权且凭据正确时，探测不能误报。
    #[tokio::test]
    async fn handshake_accepts_authenticated_server() {
        let url = spawn_fake_rc("ok");
        let c = RcClient::new(url, Some("gui".to_string()), Some(String::new()));
        let hs = Handshake::perform(&c).await.expect("应握手成功");
        assert!(!hs.auth_unset, "凭据正确时不能误判为未配置鉴权");
    }

    /// 老版本没有 `rc/noopauth`（404 → Unsupported）：探测不到 ≠ 没配鉴权，
    /// 否则「升级前的 rclone」会被整片判死。
    #[tokio::test]
    async fn missing_noopauth_method_is_not_treated_as_no_auth() {
        let url = spawn_fake_rc("old");
        let c = RcClient::new(url, None, None);
        let hs = Handshake::perform(&c).await.expect("应握手成功");
        assert!(!hs.auth_unset, "探测方法不存在时不能判定对端未配置鉴权");
    }

    #[test]
    fn detects_missing_required_via_alternatives() {
        // 只有旧名 mount/list，没有 mount/listmounts：仍应满足要求
        let req = &METHOD_REQS[4];
        assert_eq!(req.name, "mount/listmounts");
        let list = vec!["mount/list".to_string()];
        assert!(has_any(&list, req));
    }

    #[test]
    fn version_threshold() {        assert!(hs_with(None, (1, 75)).version.at_least(MIN_MAJOR, MIN_MINOR));
        assert!(!hs_with(None, (1, 40)).version.at_least(MIN_MAJOR, MIN_MINOR));
    }

    #[test]
    fn summary_mentions_method_count() {
        let hs = hs_with(Some(&["config/listremotes", "serve/list"]), (1, 75));
        let s = hs.summary();
        assert!(s.contains("2 个 RC 方法"), "got {s}");
    }

    #[test]
    fn proxy_capability_needs_get_update_unset() {
        let full = ["config/update", "config/get", "config/unset"];
        let hs = hs_with(Some(&full), (1, 75));
        assert!(hs.proxy_capability().is_ok());

        // 缺任何一个都要明确报出来，并说明缺的是哪个
        for drop_it in full {
            let list: Vec<&str> = full.iter().copied().filter(|m| *m != drop_it).collect();
            let hs = hs_with(Some(&list), (1, 75));
            let err = hs.proxy_capability().unwrap_err();
            assert!(err.contains(drop_it), "错误信息应点明缺 {drop_it}：{err}");
            assert!(err.contains("升级 rclone"), "应给出可操作的后续动作：{err}");
        }
    }

    #[test]
    fn proxy_capability_is_permissive_when_methods_unknown() {
        // 老版本没有 rc/list：不能因此断言「不支持」，否则明明是好的也用不了
        let hs = hs_with(None, (1, 75));
        assert!(hs.proxy_capability().is_ok());
    }

    /// 加进 METHOD_REQS 的两个新方法是**非阻断**的：
    /// 缺失时只能进 warnings，不能把 is_compatible 弄成 false。
    #[test]
    fn proxy_methods_are_not_blocking() {
        for want in ["config/get", "config/unset"] {
            let req = METHOD_REQS
                .iter()
                .find(|r| r.name == want)
                .unwrap_or_else(|| panic!("{want} 应出现在 METHOD_REQS 里"));
            assert!(!req.required, "{want} 不应是阻断性能力");
        }
    }
}
