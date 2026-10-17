//! `rcgui.conf`：单文件导入导出格式。
//!
//! 格式选择：**JSON 单文件**（不是 zip 容器）。理由——容器格式要额外防路径穿越、
//! 重名覆盖、超大展开、校验失败，而本格式只需要「一个文档」；阶段 6 的脚本清单以
//! 元数据（路径 + sha256 + 用途）形式内嵌，脚本文本本身**不随包携带**，从根上避免
//! 「导入即获得可执行文件」。
//!
//! 敏感数据：`sensitive: true` 表示 `remotes[].params` 里可能含 OAuth token / 密码。
//! 注意 rclone 的 `obscure` 只是可逆混淆，**不是加密**，因此该标记只用于提示，不提供
//! 任何保护。任何日志/预览都不得打印 params 内容。

use crate::{ProfileDoc, SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// 文件标识。导入时必须精确匹配，避免把任意 JSON 当配置包。
pub const KIND: &str = "rcgui.conf";
/// 包格式版本（与 `ProfileDoc::schema_version` 独立演进）。
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// 单个远程的配置片段。`params` 可能含凭据，禁止打印。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RemoteConf {
    pub name: String,
    /// 后端类型，如 drive / s3 / webdav
    pub rtype: String,
    pub params: Map<String, Value>,
    /// 该远程的凭据是否**不在**本文件中（导出时被剥离，或导入的是无凭据包）。
    /// 为 true 时 UI 必须标为「待认证」，不能当作可用配置。
    pub needs_auth: bool,
}

/// 判断某个参数键是否属于「可识别凭据」。
///
/// 原则：宁可多剥离也不能泄露。键名覆盖 token / password / secret / credential /
/// private_key / api_key / sas_url / bearer / refresh_*，以及独立的 `key`。
pub fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "token",
        "pass",
        "secret",
        "credential",
        "private_key",
        "api_key",
        "apikey",
        "sas_url",
        "bearer",
        "refresh_",
    ];
    k == "key" || NEEDLES.iter().any(|n| k.contains(n))
}

/// 从 `config/providers` 抽出「每个后端哪些字段是密码/敏感」的索引。
///
/// 这是关键词规则之外的**第二层保险**：字段标记由 rclone 自己给出
/// （`IsPassword` / `Sensitive`），不依赖硬编码名单。但标记会随 rclone 版本变化，
/// 因此只能与关键词规则**取并集**，不能替代它。
pub fn sensitive_fields(providers: &Value) -> HashMap<String, HashSet<String>> {
    let mut out = HashMap::new();
    let Some(list) = providers.get("providers").and_then(|p| p.as_array()) else {
        return out;
    };
    for p in list {
        let Some(name) = p.get("Name").and_then(|n| n.as_str()) else {
            continue;
        };
        let mut set = HashSet::new();
        if let Some(opts) = p.get("Options").and_then(|o| o.as_array()) {
            for o in opts {
                let flagged = o.get("IsPassword").and_then(|x| x.as_bool()).unwrap_or(false)
                    || o.get("Sensitive").and_then(|x| x.as_bool()).unwrap_or(false);
                if flagged {
                    if let Some(n) = o.get("Name").and_then(|x| x.as_str()) {
                        set.insert(n.to_ascii_lowercase());
                    }
                }
            }
        }
        if !set.is_empty() {
            out.insert(name.to_ascii_lowercase(), set);
        }
    }
    out
}

/// 字段是否必须剥离：关键词规则 **或** provider 元数据标记。
pub fn is_sensitive_field(
    rtype: &str,
    key: &str,
    index: &HashMap<String, HashSet<String>>,
) -> bool {
    if is_secret_key(key) {
        return true;
    }
    index
        .get(&rtype.to_ascii_lowercase())
        .map(|s| s.contains(&key.to_ascii_lowercase()))
        .unwrap_or(false)
}

impl RcguiConf {
    /// 应用凭据策略（**导入导出策略的唯一实现处**）。
    ///
    /// - `include_secrets == false`（默认）：按「关键词 ∪ provider 元数据」剥离字段，
    ///   被剥离的远程标 `needs_auth = true`（含义：凭据未随包导出，不代表已验证认证状态）；
    /// - `include_secrets == true`：保留全部参数，并标记 `sensitive`。
    ///
    /// 剥离后再扫一遍剩余键（对应 rclone 关于「config redacted 结果应再次检查」的提醒）：
    /// 万一某个后端把凭据放在没被标记的字段里，关键词规则仍会命中。
    pub fn apply_secret_policy(
        &mut self,
        include_secrets: bool,
        providers: Option<&Value>,
    ) {
        let index = providers.map(sensitive_fields).unwrap_or_default();
        let mut stripped_any = 0usize;
        for r in &mut self.remotes {
            if include_secrets {
                r.needs_auth = false;
                continue;
            }
            let before = r.params.len();
            r.params
                .retain(|k, _| !is_sensitive_field(&r.rtype, k, &index));
            // 第二遍检查：确保没有漏网的凭据键残留
            r.params
                .retain(|k, _| !is_sensitive_field(&r.rtype, k, &index));
            if r.params.len() != before {
                r.needs_auth = true;
                stripped_any += 1;
            }
        }
        self.sensitive = include_secrets && self.remotes.iter().any(|r| !r.params.is_empty());
        self.warnings.retain(|w| !w.contains("已排除"));
        if include_secrets && self.sensitive {
            self.warnings.push(
                "该文件包含远程凭据（OAuth token / 密码）。rclone 的 obscure 只是可逆混淆、\
                 并非加密，请存放在受控位置"
                    .to_string(),
            );
        }
        if stripped_any > 0 {
            self.warnings.push(format!(
                "已排除 {stripped_any} 个远程的凭据字段；这些远程在导入后标记为「凭据未随包导出」（可能需要重新认证）\
                 （表示凭据未随包导出，不代表其认证状态已被验证）"
            ));
        }
    }
}

/// 脚本清单条目（阶段 6 用于记录生成物；导入时不执行、不落盘）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ScriptEntry {
    /// 相对路径（拒绝绝对路径与 `..`）
    pub path: String,
    /// 期望的 sha256（十六进制小写）
    pub sha256: String,
    pub purpose: String,
}

/// `rcgui.conf` 根文档。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RcguiConf {
    pub kind: String,
    pub schema_version: u32,
    pub exported_at: String,
    pub app_name: String,
    pub app_version: String,
    /// 是否含敏感数据（凭据/令牌）
    pub sensitive: bool,
    /// 档案本体：期望挂载/服务配置、预设名、远程元信息、连接摘要
    pub profile: ProfileDoc,
    /// 远程完整配置（仅在允许携带凭据时才有 params）
    pub remotes: Vec<RemoteConf>,
    /// 脚本清单（只记录，不携带脚本内容）
    pub script_manifest: Vec<ScriptEntry>,
    /// 导出时记录的告警（如「离线导出，缺少远程参数」）
    pub warnings: Vec<String>,
}

impl Default for RcguiConf {
    fn default() -> Self {
        Self {
            kind: KIND.to_string(),
            schema_version: BUNDLE_SCHEMA_VERSION,
            exported_at: crate::now_iso(),
            app_name: "RCGUI".to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            sensitive: false,
            profile: ProfileDoc::default(),
            remotes: Vec::new(),
            script_manifest: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// 导入前置校验的错误。
#[derive(Debug, Clone)]
pub enum BundleError {
    /// 不是 rcgui.conf
    NotBundle { found: String },
    /// 包版本高于本程序
    VersionTooNew { found: u32, supported: u32 },
    /// 字段级问题（远程名非法、数量异常等）
    Invalid(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBundle { found } => write!(
                f,
                "不是 rcgui.conf 文件（kind = \"{found}\"，应为 \"{KIND}\"）"
            ),
            Self::VersionTooNew { found, supported } => write!(
                f,
                "文件格式版本 {found} 高于本程序支持的 {supported}，请升级 RCGUI"
            ),
            Self::Invalid(m) => write!(f, "文件内容不合法: {m}"),
        }
    }
}

impl std::error::Error for BundleError {}

/// 导入前给用户看的摘要（不含任何凭据内容）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub profile_name: String,
    pub remote_count: usize,
    pub mount_count: usize,
    pub serve_count: usize,
    pub sensitives: usize,
    pub sensitive: bool,
    /// 缺少凭据、需要重新认证的远程数量
    pub needs_auth: usize,
    pub exported_at: String,
    pub warnings: Vec<String>,
}

/// 与当前档案的差异（按名字/身份比较，不做内容级 diff）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub remotes_added: Vec<String>,
    pub remotes_removed: Vec<String>,
    pub remotes_kept: usize,
    pub mounts_before: usize,
    pub mounts_after: usize,
    pub serves_before: usize,
    pub serves_after: usize,
}

impl Diff {
    pub fn one_line(&self) -> String {
        let mut s = format!(
            "远程 +{} / -{} / 保留 {}；挂载 {}→{}，服务 {}→{}",
            self.remotes_added.len(),
            self.remotes_removed.len(),
            self.remotes_kept,
            self.mounts_before,
            self.mounts_after,
            self.serves_before,
            self.serves_after
        );
        if !self.remotes_removed.is_empty() {
            s.push_str(&format!("；将被移除的远程：{}", self.remotes_removed.join("、")));
        }
        s
    }
}

impl RcguiConf {
    /// 校验结构。**不做 IO**，因此可在任何线程调用。
    pub fn validate(&self) -> Result<(), BundleError> {
        if self.kind != KIND {
            return Err(BundleError::NotBundle {
                found: self.kind.clone(),
            });
        }
        if self.schema_version > BUNDLE_SCHEMA_VERSION {
            return Err(BundleError::VersionTooNew {
                found: self.schema_version,
                supported: BUNDLE_SCHEMA_VERSION,
            });
        }
        if self.profile.schema_version > SCHEMA_VERSION {
            return Err(BundleError::VersionTooNew {
                found: self.profile.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        // 数量上限：防止畸形/恶意文件把 UI 撑爆
        if self.mounts_len() > 500 || self.serves_len() > 500 || self.remotes.len() > 200 {
            return Err(BundleError::Invalid(
                "条目数量超出上限（远程 ≤200，挂载/服务各 ≤500）".to_string(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for r in &self.remotes {
            let name = r.name.trim();
            if name.is_empty() {
                return Err(BundleError::Invalid("存在空远程名".to_string()));
            }
            if name.contains(':') || name.contains(' ') {
                return Err(BundleError::Invalid(format!(
                    "远程名 \"{name}\" 不能包含冒号或空格"
                )));
            }
            if !seen.insert(name.to_ascii_lowercase()) {
                return Err(BundleError::Invalid(format!("远程名 \"{name}\" 重复")));
            }
            if r.rtype.trim().is_empty() {
                return Err(BundleError::Invalid(format!(
                    "远程 \"{name}\" 缺少后端类型"
                )));
            }
        }
        // 脚本清单：只做路径安全检查。导入阶段不会执行也不会写盘。
        for s in &self.script_manifest {
            if s.path.is_empty() {
                return Err(BundleError::Invalid("脚本清单存在空路径".to_string()));
            }
            let p = std::path::Path::new(&s.path);
            if p.is_absolute()
                || s.path.starts_with('/')
                || s.path.starts_with('\\')
                || p.components().any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(BundleError::Invalid(format!(
                    "脚本路径 \"{}\" 不允许绝对路径或 ..",
                    s.path
                )));
            }
        }
        Ok(())
    }

    pub fn mounts_len(&self) -> usize {
        self.profile.mounts.len()
    }

    pub fn serves_len(&self) -> usize {
        self.profile.serves.len()
    }

    /// 兼容旧导出文件：预设被移除后，早期版本导出的 `profile.serves` 永远是空的，
    /// 但 `serve_names`（服务显示名映射）里仍留着 stype/remote/sub_path/addr。
    /// 当 `serves` 为空而 `serve_names` 非空时，用后者还原出最小可用的服务条目，
    /// 否则导入预览会显示「服务 0」而实际文件里明明描述了一个服务。
    ///
    /// 还原出的条目没有密码（文件里本就没有）与 user（serve_names 不记录），
    /// 因此会在 warnings 里明确提示，导入后需要补齐。
    pub fn normalize_legacy(&mut self) {
        if !self.profile.serves.is_empty() || self.profile.serve_names.is_empty() {
            return;
        }
        let names = self.profile.serve_names.clone();
        self.profile.serves = names
            .iter()
            .map(|n| crate::ServeProfile {
                stype: n.stype.clone(),
                remote: n.remote.clone(),
                sub_path: n.sub_path.clone(),
                addr: n.addr.clone(),
                user: String::new(),
                pass: String::new(),
                enabled: true,
            })
            .collect();
        if !self
            .warnings
            .iter()
            .any(|w| w.contains("由服务名称映射还原"))
        {
            self.warnings.push(format!(
                "该文件未记录服务参数，已按服务名称映射还原 {} 个服务（地址/协议/远程取自映射）；\
                 用户名与密码需在导入后重新填写",
                self.profile.serves.len()
            ));
        }
    }

    /// 生成不含凭据的摘要。
    pub fn summary(&self) -> Summary {
        Summary {
            profile_name: self.profile.meta.name.clone(),
            remote_count: self.remotes.len(),
            mount_count: self.mounts_len(),
            serve_count: self.serves_len(),
            sensitives: self
                .remotes
                .iter()
                .filter(|r| !r.params.is_empty())
                .count(),
            sensitive: self.sensitive,
            needs_auth: self.remotes.iter().filter(|r| r.needs_auth).count(),
            exported_at: self.exported_at.clone(),
            warnings: self.warnings.clone(),
        }
    }
}

/// 计算「当前档案 → 导入文件」的差异。
pub fn diff(current: &ProfileDoc, incoming: &ProfileDoc, incoming_remotes: &[RemoteConf]) -> Diff {
    let cur_names: Vec<String> = current.remotes.iter().map(|r| r.name.clone()).collect();
    let new_names: Vec<String> = incoming_remotes.iter().map(|r| r.name.clone()).collect();
    let mut added = Vec::new();
    let mut kept = 0usize;
    for n in &new_names {
        if cur_names.iter().any(|c| c.eq_ignore_ascii_case(n)) {
            kept += 1;
        } else {
            added.push(n.clone());
        }
    }
    let removed: Vec<String> = cur_names
        .iter()
        .filter(|c| !new_names.iter().any(|n| n.eq_ignore_ascii_case(c)))
        .cloned()
        .collect();
    Diff {
        remotes_added: added,
        remotes_removed: removed,
        remotes_kept: kept,
        mounts_before: current.mounts.len(),
        mounts_after: incoming.mounts.len(),
        serves_before: current.serves.len(),
        serves_after: incoming.serves.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MountProfile, RemoteNote};

    fn sample() -> RcguiConf {
        let mut c = RcguiConf::default();
        c.profile.meta.name = "工作".into();
        c.profile.mounts.push(MountProfile {
            remote: "teldrive".into(),
            point: "T:".into(),
            ..Default::default()
        });
        c.profile.remotes.push(RemoteNote {
            name: "teldrive".into(),
            rtype: "teldrive".into(),
            usage: String::new(),
            needs_auth: false,
        });
        c.remotes.push(RemoteConf {
            name: "teldrive".into(),
            rtype: "teldrive".into(),
            params: Map::new(),
            needs_auth: false,
        });
        c
    }

    #[test]
    fn validates_own_output() {
        assert!(sample().validate().is_ok());
    }

    #[test]
    fn rejects_foreign_json() {
        let mut c = sample();
        c.kind = "something-else".into();
        assert!(matches!(c.validate(), Err(BundleError::NotBundle { .. })));
    }

    #[test]
    fn rejects_future_bundle_version() {
        let mut c = sample();
        c.schema_version = BUNDLE_SCHEMA_VERSION + 1;
        assert!(matches!(
            c.validate(),
            Err(BundleError::VersionTooNew { .. })
        ));
    }

    #[test]
    fn rejects_bad_remote_names() {
        let mut c = sample();
        c.remotes.push(RemoteConf {
            name: "bad:name".into(),
            rtype: "local".into(),
            params: Map::new(),
            needs_auth: false,
        });
        assert!(matches!(c.validate(), Err(BundleError::Invalid(_))));

        let mut c2 = sample();
        c2.remotes.push(RemoteConf {
            name: "TELDRIVE".into(), // 与 teldrive 大小写重复
            rtype: "local".into(),
            params: Map::new(),
            needs_auth: false,
        });
        assert!(matches!(c2.validate(), Err(BundleError::Invalid(_))));
    }

    #[test]
    fn rejects_script_path_escape() {
        let mut c = sample();
        c.script_manifest.push(ScriptEntry {
            path: "../evil.ps1".into(),
            sha256: String::new(),
            purpose: String::new(),
        });
        assert!(matches!(c.validate(), Err(BundleError::Invalid(_))));

        let mut c2 = sample();
        c2.script_manifest.push(ScriptEntry {
            path: r"C:\Windows\evil.ps1".into(),
            sha256: String::new(),
            purpose: String::new(),
        });
        assert!(matches!(c2.validate(), Err(BundleError::Invalid(_))));
    }

    /// 早期导出文件的兼容：serves 为空、只有 serve_names 时不应报「服务 0」。
    #[test]
    fn legacy_bundle_with_only_serve_names_yields_serve() {
        let json = serde_json::json!({
            "kind": KIND,
            "schema_version": 1,
            "profile": {
                "serve_names": [{
                    "stype": "webdav", "remote": "teldrive",
                    "sub_path": "", "addr": "127.0.0.1:8080", "name": "Serve_01"
                }]
            }
        });
        let mut c: RcguiConf = serde_json::from_value(json).unwrap();
        assert_eq!(c.serves_len(), 0, "反序列化后 serves 仍为空");
        c.normalize_legacy();
        assert_eq!(c.serves_len(), 1);
        assert_eq!(c.summary().serve_count, 1);
        assert_eq!(c.profile.serves[0].stype, "webdav");
        assert_eq!(c.profile.serves[0].addr, "127.0.0.1:8080");
        assert!(c.warnings.iter().any(|w| w.contains("服务名称映射还原")));

        // 幂等：已有 serves 时不再改动
        let before = c.profile.serves.clone();
        c.normalize_legacy();
        assert_eq!(c.profile.serves, before);
    }

    #[test]
    fn rejects_oversized_documents() {        let mut c = sample();
        for i in 0..501 {
            c.profile.mounts.push(MountProfile {
                point: format!("M{i}:"),
                ..Default::default()
            });
        }
        assert!(matches!(c.validate(), Err(BundleError::Invalid(_))));
    }

    #[test]
    fn summary_never_needs_param_contents() {
        let mut c = sample();
        c.sensitive = true;
        c.remotes[0]
            .params
            .insert("token".into(), Value::String("secret".into()));
        let s = c.summary();
        assert_eq!(s.remote_count, 1);
        assert_eq!(s.sensitives, 1);
        assert!(s.sensitive);
        // 摘要结构里没有任何 params 字段
        assert!(!format!("{s:?}").contains("secret"));
    }

    #[test]
    fn diff_counts_added_and_removed() {
        let mut cur = ProfileDoc::default();
        cur.remotes.push(RemoteNote {
            name: "old".into(),
            ..Default::default()
        });
        let inc = sample();
        let d = diff(&cur, &inc.profile, &inc.remotes);
        assert_eq!(d.remotes_added, vec!["teldrive".to_string()]);
        assert_eq!(d.remotes_removed, vec!["old".to_string()]);
        assert_eq!(d.remotes_kept, 0);
        assert!(d.one_line().contains("远程 +1 / -1"));
    }

    #[test]
    fn roundtrip_json() {
        let c = sample();
        let text = serde_json::to_string_pretty(&c).unwrap();
        let back: RcguiConf = serde_json::from_str(&text).unwrap();
        assert_eq!(back, c);
    }
}

#[cfg(test)]
mod secret_policy_tests {
    use super::*;

    fn providers() -> Value {
        serde_json::json!({
            "providers": [{
                "Name": "drive",
                "Options": [
                    { "Name": "token", "IsPassword": true, "Sensitive": true },
                    { "Name": "client_secret", "Sensitive": true },
                    { "Name": "root_folder_id" },
                    { "Name": "team_drive", "IsPassword": false, "Sensitive": false }
                ]
            }, {
                "Name": "webdav",
                "Options": [
                    { "Name": "pass", "IsPassword": true },
                    { "Name": "url" }
                ]
            }]
        })
    }

    fn remote(rtype: &str, pairs: &[(&str, &str)]) -> RemoteConf {
        let mut params = Map::new();
        for (k, v) in pairs {
            params.insert((*k).to_string(), Value::String((*v).to_string()));
        }
        RemoteConf { name: "r".into(), rtype: rtype.into(), params, needs_auth: false }
    }

    #[test]
    fn provider_flags_are_union_with_keywords() {
        let idx = sensitive_fields(&providers());
        // 仅由 provider 标记：关键词规则命中不了
        assert!(is_sensitive_field("drive", "team_drive", &idx) == false);
        assert!(is_sensitive_field("drive", "client_secret", &idx));
        assert!(is_sensitive_field("drive", "token", &idx));
        // 仅由关键词命中：provider 没标记
        assert!(is_sensitive_field("drive", "pass", &idx));
        // 普通字段不受影响
        assert!(!is_sensitive_field("drive", "root_folder_id", &idx));
        // 未知后端：仍靠关键词兜底
        assert!(is_sensitive_field("unknown", "refresh_token", &idx));
    }

    #[test]
    fn default_export_strips_and_marks_needs_auth() {
        let mut c = RcguiConf::default();
        c.remotes.push(remote("drive", &[("root_folder_id", "abc"), ("token", "secret"), ("client_secret", "s")]));
        c.remotes.push(remote("local", &[("nounc", "true")]));
        c.apply_secret_policy(false, Some(&providers()));

        assert_eq!(c.remotes[0].params.len(), 1, "只应保留 root_folder_id");
        assert!(c.remotes[0].params.contains_key("root_folder_id"));
        assert!(c.remotes[0].needs_auth, "被剥离凭据的远程必须标待认证");
        assert!(!c.remotes[1].needs_auth, "无凭据的本地远程不应标待认证");
        assert!(!c.sensitive, "默认导出不应被标记为含凭据");
        assert!(c.warnings.iter().any(|w| w.contains("凭据未随包导出")));
    }

    #[test]
    fn explicit_opt_in_keeps_everything() {
        let mut c = RcguiConf::default();
        c.remotes.push(remote("drive", &[("token", "secret")]));
        c.apply_secret_policy(true, Some(&providers()));
        assert_eq!(c.remotes[0].params.len(), 1);
        assert!(!c.remotes[0].needs_auth);
        assert!(c.sensitive);
        assert!(c.warnings.iter().any(|w| w.contains("并非加密")));
    }

    #[test]
    fn works_without_provider_metadata() {
        let mut c = RcguiConf::default();
        c.remotes.push(remote("drive", &[("token", "t"), ("root_folder_id", "x")]));
        c.apply_secret_policy(false, None);
        assert_eq!(c.remotes[0].params.len(), 1, "无元数据时关键词规则仍须剥离 token");
        assert!(c.remotes[0].needs_auth);
    }
}