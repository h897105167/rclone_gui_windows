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

    /// 主功能是否可用（版本与必需方法都满足）。
    pub fn is_compatible(&self) -> bool {
        !self.too_old && self.missing_required.is_empty()
    }

    /// 判断某方法是否可用。方法表未知时返回 true（不做无谓阻断）。
    pub fn supports(&self, method: &str) -> bool {
        match &self.methods {
            None => true,
            Some(list) => list.iter().any(|m| m == method),
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

    #[test]
    fn detects_missing_required_via_alternatives() {
        // 只有旧名 mount/list，没有 mount/listmounts：仍应满足要求
        let req = &METHOD_REQS[4];
        assert_eq!(req.name, "mount/listmounts");
        let list = vec!["mount/list".to_string()];
        assert!(has_any(&list, req));
    }

    #[test]
    fn version_threshold() {
        assert!(hs_with(None, (1, 75)).version.at_least(MIN_MAJOR, MIN_MINOR));
        assert!(!hs_with(None, (1, 40)).version.at_least(MIN_MAJOR, MIN_MINOR));
    }

    #[test]
    fn summary_mentions_method_count() {
        let hs = hs_with(Some(&["config/listremotes", "serve/list"]), (1, 75));
        let s = hs.summary();
        assert!(s.contains("2 个 RC 方法"), "got {s}");
    }
}
