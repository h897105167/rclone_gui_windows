//! 插件配置（`config.json`）：随插件一起分发的元数据 + 声明式界面。
//!
//! 一个插件包是**目录**形态，标准三件套：
//!
//! ```text
//! myplugin/
//!   config.json      # 元数据（id / name / version / entry / tags / plugin_setting）
//!   check.sha256     # 完整性校验表："<sha256>  <相对路径>" 每行一条
//!   main.wasm        # 入口脚本（`entry` 指向它）
//! ```
//!
//! 设计要点：
//!
//! - 宿主**不认识任何具体插件**：插件页的界面完全由 `plugin_setting` 里的控件描述
//!   驱动（见 [`Control`]），因此新增插件不需要改 GUI 源码。
//! - `tags` 用于插件列表的检索，`capabilities` 用于能力授权：插件只能调用自己
//!   声明过的能力组，未声明的调用在宿主侧被拒绝。
//! - `plugin.json` / `manifest.sha256` 是上一代的文件名，仍可读取（见
//!   [`ConfigFileNames`]），但新插件一律用 `config.json` / `check.sha256`。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// 新一代文件名。
pub const CONFIG_JSON: &str = "config.json";
pub const CHECK_SHA256: &str = "check.sha256";
/// 上一代文件名（兼容读取，不作为新插件模板）。
pub const LEGACY_PLUGIN_JSON: &str = "plugin.json";
pub const LEGACY_MANIFEST: &str = "manifest.sha256";

/// 宿主认识的能力组。插件在 `capabilities` 里声明组名即可。
///
/// 单条能力（如 `fs.pick_file`）由 [`cap_of_op`] 映射到所属组，
/// 这样配置文件里写的是人话，校验时又能精确到具体操作。
pub const CAP_GROUPS: &[&str] = &[
    "kv",     // 插件私有键值存储
    "ui",     // toast / 确认框
    "fs",     // 选文件、读目录信息、算哈希、解压
    "http",   // HTTP 请求与下载
    "proc",   // 启动 / 停止 / 查询子进程
    "net",    // 端口探测
    "rclone", // 调用已连接的 rclone RC
    "env",    // 宿主环境信息（版本、架构、是否提权）
];

/// 操作 → 能力组。返回 `None` 表示宿主不认识这个操作。
///
/// 这张表必须和宿主 `cap_call` 的 match 臂一致：表里漏一个，插件调用时
/// 会先被 [`crate::Allowed`] 拦住，报「宿主不认识的能力」。`fs.inspect_exe`
/// / `fs.write_file` / `fs.remove_dir` 是早期命名，留作别名。
pub fn cap_of_op(op: &str) -> Option<&'static str> {
    Some(match op {
        "kv.get" | "kv.set" | "kv.delete" | "kv.get_all" => "kv",
        "ui.toast" | "ui.confirm" | "ui.copy" => "ui",

        "fs.meta" | "fs.sha256" | "fs.read_text" | "fs.read_b64" | "fs.write_text" | "fs.copy"
        | "fs.mkdir" | "fs.remove" | "fs.list_dir" | "fs.find" | "fs.unzip" | "fs.pick_file"
        | "fs.pick_dir" | "fs.open_dir" | "fs.pe_info" | "fs.signature" | "fs.search_exe" => "fs",
        // 早期命名，保留兼容
        "fs.inspect_exe" | "fs.write_file" | "fs.remove_dir" => "fs",

        "http.download" | "http.json" | "http.text" | "http.request" => "http",
        "proc.start" | "proc.stop" | "proc.status" | "proc.run" => "proc",
        "net.port_open" => "net",
        "rclone.call" | "rclone.upload_file" | "rclone.cat" => "rclone",
        "env.info" => "env",
        _ => return None,
    })
}

/// 配置文件的两种候选名（新的优先）。
#[derive(Debug, Clone, Copy)]
pub struct ConfigFileNames {
    pub config: &'static str,
    pub check: &'static str,
}

impl ConfigFileNames {
    pub const NEW: Self = Self {
        config: CONFIG_JSON,
        check: CHECK_SHA256,
    };
    pub const LEGACY: Self = Self {
        config: LEGACY_PLUGIN_JSON,
        check: LEGACY_MANIFEST,
    };

    /// 目录里实际使用的文件名组合（`config.json` 优先）。
    pub fn detect(dir: &std::path::Path) -> Option<Self> {
        if dir.join(CONFIG_JSON).is_file() {
            return Some(if dir.join(CHECK_SHA256).is_file() {
                Self::NEW
            } else {
                // 允许新版 config.json 配旧名校验表，方便手工迁移
                Self {
                    config: CONFIG_JSON,
                    check: LEGACY_MANIFEST,
                }
            });
        }
        if dir.join(LEGACY_PLUGIN_JSON).is_file() {
            return Some(Self::LEGACY);
        }
        None
    }
}

// ---------------- plugin_setting：声明式界面 ----------------

/// 控件的展现形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    /// 分组标题 + 分隔线
    Group,
    /// 输入字段
    Field,
    /// 按钮
    Action,
    /// 校验清单（✓ / ✗ 多行），由插件返回内容
    Checks,
    /// 一行状态（支持一个指示灯）
    Status,
    /// 说明文字（可换行）
    Hint,
    /// 进度条
    Progress,
    /// 纯分隔线
    Divider,
}

impl ControlKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlKind::Group => "group",
            ControlKind::Field => "field",
            ControlKind::Action => "action",
            ControlKind::Checks => "checks",
            ControlKind::Status => "status",
            ControlKind::Hint => "hint",
            ControlKind::Progress => "progress",
            ControlKind::Divider => "divider",
        }
    }
}

/// 字段的输入类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Password,
    Number,
    Bool,
    /// 下拉框，选项来自 `options`
    Select,
    /// 文件/目录路径：旁边自动带一个「浏览」按钮
    Path,
}

impl FieldKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKind::Text => "text",
            FieldKind::Password => "password",
            FieldKind::Number => "number",
            FieldKind::Bool => "bool",
            FieldKind::Select => "select",
            FieldKind::Path => "path",
        }
    }
}

/// 按钮的视觉权重。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ButtonStyle {
    #[default]
    Secondary,
    Primary,
    Danger,
}

impl ButtonStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            ButtonStyle::Secondary => "secondary",
            ButtonStyle::Primary => "primary",
            ButtonStyle::Danger => "danger",
        }
    }
}

fn d_true() -> bool {
    true
}

/// 一个界面控件。
///
/// 所有字段都是可选的：不同 `kind` 只用其中一部分，多余字段被忽略，
/// 这样配置文件可以写得稀疏，也不怕将来加字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Control {
    #[serde(default)]
    pub kind: String,
    /// 稳定标识：字段存值、按钮回调、状态取值都用它
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub label: String,
    /// 说明文字 / 占位内容（`hint` 的正文也放这里）
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub placeholder: String,
    /// 字段类型（`kind == "field"` 时）
    #[serde(default)]
    pub field: String,
    /// 下拉选项
    #[serde(default)]
    pub options: Vec<String>,
    /// 按钮权重（`kind == "action"` 时）
    #[serde(default)]
    pub style: ButtonStyle,
    /// 同一行的控件共享同一个非空 `line`（连续且相等才会合并成一行）
    #[serde(default)]
    pub line: String,
    /// 固定宽度（像素，0 = 自适应）
    #[serde(default)]
    pub width: i32,
    /// 是否显示指示灯（`kind == "status"` 时）
    #[serde(default)]
    pub lamp: bool,
    /// 选择文件/目录时的对话框标题
    #[serde(default)]
    pub dialog_title: String,
    /// 扩展名过滤（`path` 字段用），如 `["exe"]`
    #[serde(default)]
    pub filters: Vec<String>,
    /// 默认值（字符串；bool 用 "true"/"false"）
    #[serde(default)]
    pub default: String,
    /// 初始是否可用（运行期可被插件覆盖）
    #[serde(default = "d_true")]
    pub enabled: bool,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            kind: String::new(),
            key: String::new(),
            label: String::new(),
            text: String::new(),
            placeholder: String::new(),
            field: String::new(),
            options: Vec::new(),
            style: ButtonStyle::Secondary,
            line: String::new(),
            width: 0,
            lamp: false,
            dialog_title: String::new(),
            filters: Vec::new(),
            default: String::new(),
            enabled: true,
        }
    }
}

impl Control {
    pub fn kind_enum(&self) -> Result<ControlKind> {
        serde_json::from_value::<ControlKind>(Value::String(self.kind.clone()))
            .map_err(|_| anyhow::anyhow!("未知控件类型: {}", self.kind))
    }

    pub fn field_enum(&self) -> Result<FieldKind> {
        serde_json::from_value::<FieldKind>(Value::String(self.field.clone()))
            .map_err(|_| anyhow::anyhow!("未知字段类型: {}", self.field))
    }
}

/// `plugin_setting`：把界面写成配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PluginSetting {
    /// 设置界面版本号，便于以后演进
    #[serde(default)]
    pub version: u32,
    /// 折叠行右侧显示的摘要（可选文案）
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub controls: Vec<Control>,
    /// 启动时自动执行的动作（如 `auto_start`）。空则不执行。
    #[serde(default)]
    pub on_start: String,
    /// 退出程序时要执行的动作（如 `stop_on_exit`）。空则不执行。
    ///
    /// **注意**：退出路径不调用 wasm（历史上曾被卡死的 await 拖垮退出），
    /// 宿主只按插件持久化的开关值决定要不要做，具体动作由宿主通用实现。
    #[serde(default)]
    pub on_exit: String,
}

impl PluginSetting {
    /// 校验控件描述，尽早发现插件作者的笔误。
    ///
    /// `controls` 允许为空：插件可以只是一个"运行入口"，不提供设置界面
    /// （旧格式 `plugin.json` 升级上来的插件就属于这一类）。
    pub fn validate(&self) -> Result<()> {
        let mut keys = BTreeSet::new();
        for (i, c) in self.controls.iter().enumerate() {
            let at = format!("plugin_setting.controls[{i}]");
            let kind = c.kind_enum().with_context(|| format!("{at}.kind 不合法"))?;
            match kind {
                ControlKind::Divider => continue,
                _ => {
                    if c.key.trim().is_empty() {
                        bail!("{at} 缺少 key（{:?} 控件必须能按 key 取值/回调）", kind);
                    }
                    if !keys.insert(c.key.clone()) {
                        bail!("{at} 的 key 重复：{}", c.key);
                    }
                }
            }
            if kind == ControlKind::Field {
                let fk = c.field_enum().with_context(|| format!("{at}.field 不合法"))?;
                if fk == FieldKind::Select && c.options.is_empty() {
                    bail!("{at} 是下拉框但没有 options");
                }
            }
            if kind == ControlKind::Action && c.label.trim().is_empty() {
                bail!("{at} 是按钮但没有 label");
            }
        }
        Ok(())
    }
}

// ---------------- 插件本体 ----------------

/// `config.json` 的内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PluginConfig {
    /// 全局唯一标识；同时用作插件目录名（净化后）
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 语义化版本
    #[serde(default)]
    pub version: String,
    /// 入口文件（相对插件根目录），一般是 `main.wasm`
    #[serde(default)]
    pub entry: String,
    #[serde(default)]
    pub plugin_setting: PluginSetting,

    // ---- 以下为可选元数据 ----
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// 检索用的标签（插件商店按标签精确搜索）
    #[serde(default)]
    pub tags: Vec<String>,
    /// 需要宿主授权的能力组，见 [`CAP_GROUPS`]
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// 最低宿主版本；空 = 不限
    #[serde(default)]
    pub min_gui_version: String,
    /// 平台限制；空或 `windows`
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub license: String,
    /// 是否为随程序安装的内置插件（由宿主写入，用户可以卸载）
    #[serde(default)]
    pub builtin: bool,
    /// 是否声明了 HTTP 入口：为 `true` 时宿主把 `/plugin/<id>/*` 转发给插件。
    ///
    /// 路由、鉴权、响应格式全部由插件自己决定；宿主只负责转发与超时。
    #[serde(default)]
    pub http_entry: bool,
}

impl PluginConfig {
    /// 校验必需字段与取值合法性。
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("config.json 缺少 id");
        }
        if !valid_id(&self.id) {
            bail!(
                "config.json 的 id 不合法（只能用字母、数字、`.`、`-`、`_`，2–64 个字符）：{}",
                self.id
            );
        }
        if self.name.trim().is_empty() {
            bail!("config.json 缺少 name");
        }
        if self.version.trim().is_empty() {
            bail!("config.json 缺少 version");
        }
        semver::Version::parse(self.version.trim())
            .with_context(|| format!("config.json 的 version 不是合法语义化版本：{}", self.version))?;
        if !self.min_gui_version.trim().is_empty() {
            semver::Version::parse(self.min_gui_version.trim()).with_context(|| {
                format!("config.json 的 min_gui_version 不是合法语义化版本：{}", self.min_gui_version)
            })?;
        }
        if self.entry.trim().is_empty() {
            bail!("config.json 缺少 entry（一般是 main.wasm）");
        }
        // `platform` 只用来挡「明显不是这台机器的插件」。宿主本身是 Windows 应用，
        // 而插件跑在 WASI 沙箱里：`any`（或不填）与 `windows` 都能跑，其余拒绝。
        let platform = self.platform.trim();
        if !platform.is_empty() && platform != "windows" && platform != "any" {
            bail!("插件声明的平台不受支持：{platform}（仅支持 windows / any）");
        }
        if self.tags.len() > 32 {
            bail!("config.json 的 tags 过多（{} > 32）", self.tags.len());
        }
        for t in &self.tags {
            if t.trim().is_empty() {
                bail!("config.json 的 tags 里有空标签");
            }
            if t.chars().count() > 24 {
                bail!("tag 过长（> 24 字符）：{t}");
            }
        }
        for c in &self.capabilities {
            if !CAP_GROUPS.contains(&c.as_str()) {
                bail!(
                    "config.json 声明了宿主不认识的能力组：{c}（可用：{}）",
                    CAP_GROUPS.join(" / ")
                );
            }
        }
        self.plugin_setting.validate()?;
        Ok(())
    }

    /// 宿主版本是否满足 `min_gui_version`。
    pub fn compatible_with(&self, gui_version: &str) -> Result<()> {
        if self.min_gui_version.trim().is_empty() {
            return Ok(());
        }
        let need = semver::Version::parse(self.min_gui_version.trim())?;
        let have = semver::Version::parse(gui_version.trim())
            .with_context(|| format!("宿主版本号无法解析：{gui_version}"))?;
        if have < need {
            bail!(
                "插件需要宿主版本 >= {}，当前为 {}",
                self.min_gui_version,
                gui_version
            );
        }
        Ok(())
    }

    /// 插件是否声明了某项能力组。
    pub fn allows(&self, group: &str) -> bool {
        self.capabilities.iter().any(|c| c == group)
    }

    /// 入口是不是 WASM 脚本。
    pub fn entry_is_wasm(&self) -> bool {
        self.entry.to_ascii_lowercase().ends_with(".wasm")
    }

    /// 折叠行上的类型徽标文本。
    pub fn kind_badge(&self) -> &'static str {
        if self.entry_is_wasm() {
            "WASM"
        } else {
            "程序"
        }
    }

    /// 合并成用于搜索的一行小写文本（名称 + id + 标签 + 描述）。
    pub fn search_blob(&self) -> String {
        let mut s = format!("{} {} {} {}", self.name, self.id, self.tags.join(" "), self.description);
        s = s.to_lowercase();
        s
    }
}

/// id 只允许安全字符：既做目录名，也做 kv 命名空间，绝不能带路径分隔符。
pub fn valid_id(id: &str) -> bool {
    let n = id.chars().count();
    if !(2..=64).contains(&n) {
        return false;
    }
    if id.starts_with('.') || id.starts_with('-') {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// 把 id 净化成安全的目录名（与校验规则一致；额外兜底）。
pub fn sanitize_dir_name(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() {
        "_plugin".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> PluginConfig {
        serde_json::from_value(json!({
            "id": "onerclone",
            "name": "一键 rclone",
            "version": "0.2.0",
            "entry": "main.wasm",
            "description": "检测 / 安装 / 启动本机 rclone",
            "tags": ["rclone", "进程", "安装"],
            "capabilities": ["kv", "fs", "http", "proc", "net", "rclone", "ui", "env"],
            "plugin_setting": {
                "version": 1,
                "on_start": "auto_start",
                "on_exit": "stop_on_exit",
                "controls": [
                    {"kind": "group", "key": "g1", "label": "① rclone 程序"},
                    {"kind": "field", "key": "exe", "label": "程序路径", "field": "path",
                     "dialog_title": "选择 rclone.exe", "filters": ["exe"]},
                    {"kind": "action", "key": "validate", "label": "重新校验", "line": "exe-row"},
                    {"kind": "checks", "key": "checks"},
                    {"kind": "field", "key": "edition", "label": "版本", "field": "select",
                     "options": ["官方版", "增强版"], "default": "0"},
                    {"kind": "field", "key": "port", "label": "端口", "field": "number", "default": "5572"},
                    {"kind": "status", "key": "conn", "label": "连接状态", "lamp": true},
                    {"kind": "progress", "key": "progress"},
                    {"kind": "divider"}
                ]
            }
        }))
        .unwrap()
    }

    #[test]
    fn parses_full_config() {
        let c = sample();
        c.validate().unwrap();
        assert_eq!(c.id, "onerclone");
        assert_eq!(c.tags.len(), 3);
        assert!(c.allows("proc"));
        assert!(!c.allows("fs2"));
        assert_eq!(c.kind_badge(), "WASM");
        assert!(c.entry_is_wasm());
        assert!(c.search_blob().contains("rclone"));
        assert_eq!(c.plugin_setting.on_start, "auto_start");
        assert_eq!(c.plugin_setting.on_exit, "stop_on_exit");
    }

    #[test]
    fn default_entry_and_plugin_setting_are_empty() {
        let c = PluginConfig::default();
        assert!(c.entry.is_empty());
        assert!(c.plugin_setting.controls.is_empty());
        assert!(!c.builtin);
    }

    #[test]
    fn rejects_bad_ids() {
        for bad in ["", "a", "with space", "../evil", ".hidden", "-lead", "中文id"] {
            let mut c = sample();
            c.id = bad.into();
            assert!(c.validate().is_err(), "id 应该被拒绝: {bad}");
        }
        // 合法：点分、下划线、短横线
        for ok in ["com.example.p", "one_click", "one-click", "a1"] {
            let mut c = sample();
            c.id = ok.into();
            c.validate().unwrap_or_else(|e| panic!("id 应通过: {ok} -> {e}"));
        }
    }

    #[test]
    fn rejects_bad_version_and_entry() {
        let mut c = sample();
        c.version = "not-a-version".into();
        assert!(c.validate().is_err());

        let mut c = sample();
        c.entry = String::new();
        assert!(c.validate().is_err());

        let mut c = sample();
        c.min_gui_version = "x".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_unknown_capability() {
        let mut c = sample();
        c.capabilities.push("system.root".into());
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("system.root"), "实际: {err}");
    }

    #[test]
    fn rejects_bad_controls() {
        // 控件类型打错
        let mut c = sample();
        let mut bad = Control::default();
        bad.kind = "buton".into();
        bad.key = "x".into();
        c.plugin_setting.controls.push(bad);
        assert!(c.validate().is_err());

        // 重复 key
        let mut c = sample();
        let mut dup = c.plugin_setting.controls[1].clone();
        dup.key = "exe".into();
        c.plugin_setting.controls.push(dup);
        assert!(c.validate().unwrap_err().to_string().contains("重复"));

        // 下拉框缺 options
        let mut c = sample();
        c.plugin_setting.controls[4].options.clear();
        assert!(c.validate().is_err());

        // 按钮缺 label
        let mut c = sample();
        c.plugin_setting.controls[2].label.clear();
        assert!(c.validate().is_err());

        // 空 controls
        let mut c = sample();
        c.plugin_setting.controls.clear();
        assert!(c.validate().is_ok(), "控件列表可以为空（纯入口插件）");
    }

    #[test]
    fn checks_min_version() {
        let mut c = sample();
        c.min_gui_version = "2.0.0".into();
        assert!(c.compatible_with("2.1.0").is_ok());
        assert!(c.compatible_with("1.9.9").is_err());
        c.min_gui_version.clear();
        assert!(c.compatible_with("0.0.1").is_ok());
    }

    #[test]
    fn maps_ops_to_capability_groups() {
        assert_eq!(cap_of_op("fs.pick_file"), Some("fs"));
        assert_eq!(cap_of_op("proc.start"), Some("proc"));
        assert_eq!(cap_of_op("http.download"), Some("http"));
        assert_eq!(cap_of_op("rclone.call"), Some("rclone"));
        assert_eq!(cap_of_op("rm -rf"), None);
    }

    #[test]
    fn sanitizes_dir_names_safely() {
        assert_eq!(sanitize_dir_name("onerclone"), "onerclone");
        assert_eq!(sanitize_dir_name("a/b\\c"), "a_b_c");
        // 关键约束是「结果里不能有路径分隔符、不能以点开头」，
        // 具体把非法字符替换成什么不重要。
        for raw in ["../../x", "..", "a/b\\c", "C:\\abs\\path", "con"] {
            let s = sanitize_dir_name(raw);
            assert!(!s.contains('/') && !s.contains('\\'), "{raw} -> {s}");
            assert!(!s.starts_with('.'), "{raw} -> {s}");
            assert!(!s.is_empty(), "{raw} -> {s}");
        }
    }

    #[test]
    fn detects_file_name_pairs() {
        let base = std::env::temp_dir().join(format!("plugincfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // 什么都没有
        assert!(ConfigFileNames::detect(&base).is_none());

        // 只有旧名
        std::fs::write(base.join(LEGACY_PLUGIN_JSON), "{}").unwrap();
        let f = ConfigFileNames::detect(&base).unwrap();
        assert_eq!(f.config, LEGACY_PLUGIN_JSON);
        assert_eq!(f.check, LEGACY_MANIFEST);

        // 新名优先
        std::fs::write(base.join(CONFIG_JSON), "{}").unwrap();
        std::fs::write(base.join(CHECK_SHA256), "").unwrap();
        let f = ConfigFileNames::detect(&base).unwrap();
        assert_eq!(f.config, CONFIG_JSON);
        assert_eq!(f.check, CHECK_SHA256);

        let _ = std::fs::remove_dir_all(&base);
    }
}
