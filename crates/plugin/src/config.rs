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
///
/// 这张表**只放能被执行的东西**：一次能力组对应一组插件可发起的操作。
/// 像 `remote_types`（在"新建远程"里加派生类型）这类纯元数据声明不进这里
/// ——它描述的是配置内容，不是插件能调用的 API，要求它"申请权限"会让插件
/// 作者以为派生类型需要授权，而实际权限边界在内核能否支持 base_type 上。
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

        "fs.meta" | "fs.sha256" | "fs.read_text" | "fs.read_b64" | "fs.write_text"
        | "fs.write_b64" | "fs.copy" | "fs.mkdir" | "fs.remove" | "fs.list_dir" | "fs.find"
        | "fs.unzip" | "fs.pick_file" | "fs.pick_dir" | "fs.open_dir" | "fs.pe_info"
        | "fs.signature" | "fs.search_exe" => "fs",
        // 早期命名，保留兼容
        "fs.inspect_exe" | "fs.write_file" | "fs.remove_dir" => "fs",

        "http.download" | "http.json" | "http.text" | "http.request" | "http.upload" => "http",
        "proc.start" | "proc.stop" | "proc.status" | "proc.run" => "proc",
        "net.port_open" => "net",
        "rclone.call" | "rclone.upload_file" | "rclone.cat" | "rclone.remotes" => "rclone",
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
    /// 图片：插件在 `body.images.<key>` 里下发绝对路径，宿主读盘解码后显示
    Image,
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
            ControlKind::Image => "image",
            ControlKind::Divider => "divider",
        }
    }
}

/// 字段的输入类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    /// 多行文本框：换行有意义的数据（例如一行一个 token）
    Multiline,
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
            FieldKind::Multiline => "multiline",
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

// ---------------- 多语言字段 ----------------

/// 从「可能是单语、也可能按语言分列」的值里挑出当前语言的那一条。
///
/// 插件**自带**译文（宿主不再维护插件词典），`config.json` 的展示字段因此允许两种写法：
/// ```json
/// "label": "上传"
/// "label": { "zh": "上传", "en": "Upload" }
/// ```
/// 取值顺序：当前界面语言 → 该语言的区域变体（`en-US`）→ 英文 → 任意非空值。
/// 缺语言时落到英文，这条规则和插件 wasm 里的 `i18n` 模块保持一致。
pub fn pick_localized(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            let want = rcgui_i18n::lang().tag(); // "zh" / "en"
            if let Some(Value::String(s)) = map.get(want) {
                return s.clone();
            }
            // 区域变体：en-US、zh-CN 之类；键的大小写不敏感
            let lower = want.to_ascii_lowercase();
            for (k, val) in map {
                if k.to_ascii_lowercase().starts_with(&lower) {
                    if let Value::String(s) = val {
                        return s.clone();
                    }
                }
            }
            if let Some(Value::String(s)) = map.get("en") {
                return s.clone();
            }
            for val in map.values() {
                if let Value::String(s) = val {
                    return s.clone();
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// `serde` 适配：字段既接受字符串，也接受语言映射。
fn de_localized<'de, D>(d: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Value::deserialize(d)?;
    Ok(pick_localized(&v))
}

/// 从「可能是纯数组、也可能按语言分列」的值里挑出当前语言的那一组。
///
/// ```json
/// "tags": ["rclone", "图床"]
/// "tags": { "zh": ["rclone", "图床"], "en": ["rclone", "image host"] }
/// ```
///
/// 取语言的顺序与 [`pick_localized`] 一致。
pub fn pick_localized_list(v: &Value) -> Vec<String> {
    pick_localized_list_for(v, rcgui_i18n::lang().tag())
}

/// [`pick_localized_list`] 的显式语言版本（测试用，避免动全局语言）。
fn pick_localized_list_for(v: &Value, want: &str) -> Vec<String> {
    fn strings(v: &Value) -> Option<Vec<String>> {
        let arr = v.as_array()?;
        Some(
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
        )
    }
    match v {
        Value::Array(_) => strings(v).unwrap_or_default(),
        Value::Object(map) => {
            if let Some(list) = map.get(want).and_then(strings) {
                return list;
            }
            let lower = want.to_ascii_lowercase();
            for (k, val) in map {
                if k.to_ascii_lowercase().starts_with(&lower) {
                    if let Some(list) = strings(val) {
                        return list;
                    }
                }
            }
            if let Some(list) = map.get("en").and_then(strings) {
                return list;
            }
            for val in map.values() {
                if let Some(list) = strings(val) {
                    return list;
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// `serde` 适配：列表字段既接受字符串数组，也接受语言映射。
fn de_localized_list<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Value::deserialize(d)?;
    Ok(pick_localized_list(&v))
}

/// 下拉框的一个候选：显示文案可以翻译，落盘值必须是插件认识的原值。
///
/// 两种写法等价：
/// ```json
/// "options": ["不选择", "模板一"]
/// "options": [
///   { "value": "不选择", "label": { "zh": "不选择", "en": "No short link" } },
///   { "value": "模板一", "label": { "zh": "模板一", "en": "Template 1" } }
/// ]
/// ```
/// 第一种写法里值即文案（老插件照旧）；第二种写法把「给人看」和「给插件」分开，
/// 宿主显示 `label`、落盘 `value`，于是英文界面下插件收不到英文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Choice {
    /// 简写：值即显示文案
    Plain(String),
    /// 全写：`value` 落盘，`label` 显示（支持语言映射）
    Full {
        #[serde(default)]
        value: String,
        #[serde(default)]
        label: Value,
    },
}

impl Choice {
    /// 落盘值（插件认识的原值）。
    pub fn value(&self) -> &str {
        match self {
            Choice::Plain(s) => s,
            Choice::Full { value, .. } => value,
        }
    }

    /// 当前语言下要显示的文案；没写 `label` 时退回 `value`。
    pub fn label(&self) -> String {
        match self {
            Choice::Plain(s) => s.clone(),
            Choice::Full { value, label } => {
                let l = pick_localized(label);
                if l.trim().is_empty() {
                    value.clone()
                } else {
                    l
                }
            }
        }
    }
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
    /// 多语言：可写字符串，也可写 `{"zh": …, "en": …}`
    #[serde(default, deserialize_with = "de_localized")]
    pub label: String,
    /// 说明文字 / 占位内容（`hint` 的正文也放这里）；支持语言映射
    #[serde(default, deserialize_with = "de_localized")]
    pub text: String,
    /// 多语言：可写字符串，也可写语言映射
    #[serde(default, deserialize_with = "de_localized")]
    pub placeholder: String,
    /// 字段类型（`kind == "field"` 时）
    #[serde(default)]
    pub field: String,
    /// 下拉选项：字符串，或 `{ "value": …, "label": … }`（见 [`Choice`]）
    ///
    /// 前者值即文案；后者把「落盘值」和「显示文案」分开，英文界面下插件仍收到中文原值。
    #[serde(default)]
    pub options: Vec<Choice>,
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
    /// 选择文件/目录时的对话框标题（支持语言映射）
    #[serde(default, deserialize_with = "de_localized")]
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
    /// 选项由插件运行时下发（`body.choices`）——此时 `select` 允许 `options` 为空
    #[serde(default)]
    pub dynamic: bool,
    /// 所属分页 id（对应 `plugin_setting.pages[].id`）。空 = 归到默认页。
    #[serde(default)]
    pub page: String,
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
            dynamic: false,
            page: String::new(),
        }
    }
}

impl Control {
    pub fn kind_enum(&self) -> Result<ControlKind> {
        serde_json::from_value::<ControlKind>(Value::String(self.kind.clone()))
            .map_err(|_| anyhow::anyhow!("{}", rcgui_i18n::f("未知控件类型: {}", &[&(self.kind)])))
    }

    pub fn field_enum(&self) -> Result<FieldKind> {
        serde_json::from_value::<FieldKind>(Value::String(self.field.clone()))
            .map_err(|_| anyhow::anyhow!("{}", rcgui_i18n::f("未知字段类型: {}", &[&(self.field)])))
    }
}

/// 设置界面的一个分页标签。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Page {
    /// 稳定 id；[`Control::page`] 引用它
    #[serde(default)]
    pub id: String,
    /// 标签文字（支持语言映射）；界面直接显示它
    #[serde(default, deserialize_with = "de_localized")]
    pub label: String,
}

/// `plugin_setting`：把界面写成配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PluginSetting {
    /// 设置界面版本号，便于以后演进
    #[serde(default)]
    pub version: u32,
    /// 折叠行右侧显示的摘要（可选文案，支持语言映射）
    #[serde(default, deserialize_with = "de_localized")]
    pub summary: String,
    /// 分页声明。空 = 不分页，所有控件同屏显示（旧插件的行为）。
    #[serde(default)]
    pub pages: Vec<Page>,
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
        let mut page_ids = BTreeSet::new();
        for (i, p) in self.pages.iter().enumerate() {
            if p.id.trim().is_empty() {
                bail!(
                    "{}",
                    rcgui_i18n::f("plugin_setting.pages[{}] 缺少 id", &[&(i)])
                );
            }
            if p.label.trim().is_empty() {
                bail!(
                    "{}",
                    rcgui_i18n::f("plugin_setting.pages[{}]（{}）缺少 label", &[&(i), &(p.id)])
                );
            }
            if !page_ids.insert(p.id.clone()) {
                bail!(
                    "{}",
                    rcgui_i18n::f("plugin_setting.pages 的 id 重复：{}", &[&(p.id)])
                );
            }
        }
        let mut keys = BTreeSet::new();
        for (i, c) in self.controls.iter().enumerate() {
            let at = format!("plugin_setting.controls[{i}]");
            let kind = c
                .kind_enum()
                .with_context(|| rcgui_i18n::f("{}.kind 不合法", &[&(at)]))?;
            // 声明了分页就必须存在：写错 id 会让控件凭空消失，比报错更难查。
            if !c.page.trim().is_empty() && !page_ids.contains(&c.page) {
                bail!(
                    "{}",
                    rcgui_i18n::f(
                        "{} 的 page 未在 plugin_setting.pages 里声明：{}",
                        &[&(at), &(c.page)]
                    )
                );
            }
            match kind {
                ControlKind::Divider => continue,
                _ => {
                    if c.key.trim().is_empty() {
                        bail!(
                            "{}",
                            rcgui_i18n::f(
                                "{} 缺少 key（{} 控件必须能按 key 取值/回调）",
                                &[&(at), &(format!("{:?}", kind))]
                            )
                        );
                    }
                    if !keys.insert(c.key.clone()) {
                        bail!(
                            "{}",
                            rcgui_i18n::f("{} 的 key 重复：{}", &[&(at), &(c.key)])
                        );
                    }
                }
            }
            if kind == ControlKind::Field {
                let fk = c
                    .field_enum()
                    .with_context(|| rcgui_i18n::f("{}.field 不合法", &[&(at)]))?;
                if fk == FieldKind::Select && c.options.is_empty() && !c.dynamic {
                    bail!("{}", rcgui_i18n::f("{} 是下拉框但没有 options（选项由插件运行时下发时请写 \"dynamic\": true）", &[&(at)]));
                }
            }
            if kind == ControlKind::Action && c.label.trim().is_empty() {
                bail!("{}", rcgui_i18n::f("{} 是按钮但没有 label", &[&(at)]));
            }
        }
        Ok(())
    }
}

// ---------------- 插件本体 ----------------

/// 插件声明的一个派生远程类型。
///
/// 插件不能新增后端实现（真正的后端在 rclone 里），只能"预设参数"：
/// 用户新建 `id` 时，宿主按 `base_type` 创建配置，把 `defaults` 填进去。
/// 例：`id="k-vault"`, `base_type="webdav"`, `defaults={"vendor":"other"}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RemoteType {
    /// 新建远程时写进 `config` 的 `type` 值；必须全局唯一
    #[serde(default)]
    pub id: String,
    /// 新建类型下拉框里显示的文字（支持语言映射）；它只是显示名，不是 id
    #[serde(default, deserialize_with = "de_localized")]
    pub name: String,
    /// 底层 rclone 后端类型（如 `webdav`）；`defaults` 的参数名由它决定
    #[serde(default)]
    pub base_type: String,
    /// 预填参数。值只能是字符串：rclone 的配置项都是字符串，数字/布尔会被
    /// 它按自己的规则解析，宿主不做类型推断。
    #[serde(default)]
    pub defaults: std::collections::BTreeMap<String, String>,
}

/// `config.json` 的内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PluginConfig {
    /// 全局唯一标识；同时用作插件目录名（净化后）
    #[serde(default)]
    pub id: String,
    /// 插件显示名（支持语言映射）
    #[serde(default, deserialize_with = "de_localized")]
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
    /// 简介（支持语言映射）
    #[serde(default, deserialize_with = "de_localized")]
    pub description: String,
    /// 作者（支持语言映射；插件作者一般写纯文本）
    #[serde(default, deserialize_with = "de_localized")]
    pub author: String,
    /// 检索用的标签（插件商店按标签精确搜索；支持语言映射）
    #[serde(default, deserialize_with = "de_localized_list")]
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
    /// 派生远程类型：插件可以把"预设好参数的 rclone 后端"注册成新建类型。
    ///
    /// 只有插件处于启用状态时宿主才会把这些类型列进新建下拉框（见
    /// `crates/app/src/remote_types.rs`）；插件被停用或卸载后，用户已经
    /// 建好的远程配置仍然留在 rclone 的 `config` 里，不会丢数据。
    #[serde(default)]
    pub remote_types: Vec<RemoteType>,
}

impl PluginConfig {
    /// 校验必需字段与取值合法性。
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("{}", rcgui_i18n::t("config.json 缺少 id"));
        }
        if !valid_id(&self.id) {
            bail!(
                "{}",
                rcgui_i18n::f(
                    "config.json 的 id 不合法（只能用字母、数字、`.`、`-`、`_`，2–64 个字符）：{}",
                    &[&(self.id)]
                )
            );
        }
        if self.name.trim().is_empty() {
            bail!("{}", rcgui_i18n::t("config.json 缺少 name"));
        }
        if self.version.trim().is_empty() {
            bail!("{}", rcgui_i18n::t("config.json 缺少 version"));
        }
        semver::Version::parse(self.version.trim()).with_context(|| {
            rcgui_i18n::f(
                "config.json 的 version 不是合法语义化版本：{}",
                &[&(self.version)],
            )
        })?;
        if !self.min_gui_version.trim().is_empty() {
            semver::Version::parse(self.min_gui_version.trim()).with_context(|| {
                rcgui_i18n::f(
                    "config.json 的 min_gui_version 不是合法语义化版本：{}",
                    &[&(self.min_gui_version)],
                )
            })?;
        }
        if self.entry.trim().is_empty() {
            bail!(
                "{}",
                rcgui_i18n::t("config.json 缺少 entry（一般是 main.wasm）")
            );
        }
        // `platform` 只用来挡「明显不是这台机器的插件」。宿主本身是 Windows 应用，
        // 而插件跑在 WASI 沙箱里：`any`（或不填）与 `windows` 都能跑，其余拒绝。
        let platform = self.platform.trim();
        if !platform.is_empty() && platform != "windows" && platform != "any" {
            bail!(
                "{}",
                rcgui_i18n::f(
                    "插件声明的平台不受支持：{}（仅支持 windows / any）",
                    &[&(platform)]
                )
            );
        }
        if self.tags.len() > 32 {
            bail!(
                "{}",
                rcgui_i18n::f("config.json 的 tags 过多（{} > 32）", &[&(self.tags.len())])
            );
        }
        for t in &self.tags {
            if t.trim().is_empty() {
                bail!("{}", rcgui_i18n::t("config.json 的 tags 里有空标签"));
            }
            if t.chars().count() > 24 {
                bail!("{}", rcgui_i18n::f("tag 过长（> 24 字符）：{}", &[&(t)]));
            }
        }
        for c in &self.capabilities {
            if !CAP_GROUPS.contains(&c.as_str()) {
                bail!(
                    "{}",
                    rcgui_i18n::f(
                        "config.json 声明了宿主不认识的能力组：{}（可用：{}）",
                        &[&(c), &(CAP_GROUPS.join(" / "))]
                    )
                );
            }
        }
        self.plugin_setting.validate()?;
        self.validate_remote_types()?;
        Ok(())
    }

    /// 校验派生远程类型声明。
    ///
    /// 这里**不要求插件申请任何能力组**：`remote_types` 是元数据（描述"用户新建远程时
    /// 可以选这个派生类型、它落在哪个 rclone 后端上"），不是插件能发起的操作。
    /// 真正的权限边界在内核是否支持 `base_type` 那个后端，以及插件本身是否启用。
    fn validate_remote_types(&self) -> Result<()> {
        if self.remote_types.is_empty() {
            return Ok(());
        }
        if self.remote_types.len() > 16 {
            bail!(
                "{}",
                rcgui_i18n::f(
                    "config.json 的 remote_types 过多（{} > 16）",
                    &[&(self.remote_types.len())]
                )
            );
        }
        let mut ids = BTreeSet::new();
        for (i, rt) in self.remote_types.iter().enumerate() {
            let at = format!("remote_types[{i}]");
            if !valid_id(&rt.id) {
                bail!(
                    "{}",
                    rcgui_i18n::f(
                        "{} 的 id 不合法（只能用字母、数字、`.`、`-`、`_`，2–64 个字符）：{}",
                        &[&(at), &(rt.id)]
                    )
                );
            }
            if !ids.insert(rt.id.clone()) {
                bail!(
                    "{}",
                    rcgui_i18n::f("remote_types 的 id 重复：{}", &[&(rt.id)])
                );
            }
            if rt.name.trim().is_empty() {
                bail!(
                    "{}",
                    rcgui_i18n::f("{} 缺少 name（新建类型下拉框要显示它）", &[&(at)])
                );
            }
            // base_type 必须是 rclone 自己的后端名：它会被原样写进 config 的 type 字段，
            // 写错了要到真正连远程时才会报错，所以这里只放行后端名的形状。
            let base = rt.base_type.trim();
            if base.is_empty() {
                bail!(
                    "{}",
                    rcgui_i18n::f("{} 缺少 base_type（如 webdav）", &[&(at)])
                );
            }
            // 先判自引用：派生类型的 id 通常带 `-`（如 `k-vault`），若先做字符形状检查，
            // 这种最常见的写错方式只会得到"不是合法的后端名"，看不出真正的问题。
            if base == rt.id {
                bail!(
                    "{}",
                    rcgui_i18n::f("{} 的 base_type 不能等于自己的 id（会绕过去重）", &[&(at)])
                );
            }
            if !base
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                bail!(
                    "{}",
                    rcgui_i18n::f("{} 的 base_type 不是合法的后端名：{}", &[&(at), &(base)])
                );
            }
            for (k, v) in &rt.defaults {
                if k.trim().is_empty() {
                    bail!("{}", rcgui_i18n::f("{} 的 defaults 里有空参数名", &[&(at)]));
                }
                if k == "type" {
                    bail!(
                        "{}",
                        rcgui_i18n::f(
                            "{} 的 defaults 不能设置 type（由 base_type 决定）",
                            &[&(at)]
                        )
                    );
                }
                if v.len() > 4096 {
                    bail!(
                        "{}",
                        rcgui_i18n::f("{} 的参数 {} 过长（> 4096 字节）", &[&(at), &(k)])
                    );
                }
            }
        }
        Ok(())
    }

    /// 宿主版本是否满足 `min_gui_version`。
    pub fn compatible_with(&self, gui_version: &str) -> Result<()> {
        if self.min_gui_version.trim().is_empty() {
            return Ok(());
        }
        let need = semver::Version::parse(self.min_gui_version.trim())?;
        let have = semver::Version::parse(gui_version.trim())
            .with_context(|| rcgui_i18n::f("宿主版本号无法解析：{}", &[&(gui_version)]))?;
        if have < need {
            bail!(
                "{}",
                rcgui_i18n::f(
                    "插件需要宿主版本 >= {}，当前为 {}",
                    &[&(self.min_gui_version), &(gui_version)]
                )
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
            rcgui_i18n::t("程序")
        }
    }

    /// 合并成用于搜索的一行小写文本（名称 + id + 标签 + 描述）。
    pub fn search_blob(&self) -> String {
        let mut s = format!(
            "{} {} {} {}",
            self.name,
            self.id,
            self.tags.join(" "),
            self.description
        );
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
        for bad in [
            "",
            "a",
            "with space",
            "../evil",
            ".hidden",
            "-lead",
            "中文id",
        ] {
            let mut c = sample();
            c.id = bad.into();
            assert!(c.validate().is_err(), "id 应该被拒绝: {bad}");
        }
        // 合法：点分、下划线、短横线
        for ok in ["com.example.p", "one_click", "one-click", "a1"] {
            let mut c = sample();
            c.id = ok.into();
            c.validate()
                .unwrap_or_else(|e| panic!("id 应通过: {ok} -> {e}"));
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
        assert_eq!(cap_of_op("http.upload"), Some("http"));
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

    /// 标签既可以用纯数组，也可以按语言分列（插件自带译文）。
    #[test]
    fn tags_accept_localized_lists() {
        let v = json!({
            "zh": ["进程", "安装"],
            "en": ["process", "install"]
        });
        assert_eq!(
            pick_localized_list_for(&v, "zh"),
            vec!["进程".to_string(), "安装".to_string()]
        );
        assert_eq!(
            pick_localized_list_for(&v, "en"),
            vec!["process".to_string(), "install".to_string()]
        );
        // 区域变体 en-US 命中 en
        assert_eq!(
            pick_localized_list_for(&v, "en-US"),
            vec!["process".to_string(), "install".to_string()]
        );
        // 缺当前语言时回落英文，再回落任意一组
        let only_en = json!({ "en": ["process"] });
        assert_eq!(
            pick_localized_list_for(&only_en, "zh"),
            vec!["process".to_string()]
        );
        // 老写法：纯数组
        let plain = json!(["rclone", "图床"]);
        assert_eq!(
            pick_localized_list_for(&plain, "en"),
            vec!["rclone".to_string(), "图床".to_string()]
        );
        // 非法输入不 panic
        assert!(pick_localized_list_for(&json!("rclone"), "en").is_empty());
        assert!(pick_localized_list_for(&json!({ "en": "rclone" }), "en").is_empty());
    }

    /// `tags` 字段接受两种写法：老插件（数组）与自带译文（语言映射）。
    #[test]
    fn tags_field_deserializes_both_forms() {
        let plain: PluginConfig = serde_json::from_value(json!({
            "id": "onerclone", "name": "一键 rclone", "version": "0.3.3",
            "entry": "main.wasm", "tags": ["rclone", "进程"]
        }))
        .unwrap();
        assert_eq!(plain.tags, vec!["rclone".to_string(), "进程".to_string()]);

        let localized: PluginConfig = serde_json::from_value(json!({
            "id": "onerclone", "name": "一键 rclone", "version": "0.3.3",
            "entry": "main.wasm",
            "tags": { "zh": ["进程"], "en": ["process"] }
        }))
        .unwrap();
        // 默认界面语言是中文
        assert_eq!(localized.tags, vec!["进程".to_string()]);
        assert!(localized.search_blob().contains("进程"));
    }

    /// 分页声明合法时，控件可以带 page。
    #[test]
    fn pages_accept_controls_with_declared_page() {
        let c: PluginConfig = serde_json::from_value(json!({
            "id": "com.example.paged", "name": "分页插件", "version": "1.0.0",
            "entry": "main.wasm",
            "plugin_setting": {
                "pages": [
                    {"id": "basic", "label": "基础"},
                    {"id": "advanced", "label": {"zh": "高级", "en": "Advanced"}}
                ],
                "controls": [
                    {"kind": "field", "key": "exe", "label": "路径", "field": "path", "page": "basic"},
                    {"kind": "field", "key": "port", "label": "端口", "field": "number", "page": "advanced"},
                    {"kind": "status", "key": "conn", "label": "状态"}
                ]
            }
        }))
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.plugin_setting.pages.len(), 2);
        assert_eq!(c.plugin_setting.pages[1].label, "高级");
        // 不声明 page 的控件归默认页，page 保持空串
        assert!(c.plugin_setting.controls[2].page.is_empty());
    }

    /// 控件引用了没声明的分页要报错：否则控件会在界面上凭空消失。
    #[test]
    fn control_with_unknown_page_is_rejected() {
        let c: PluginConfig = serde_json::from_value(json!({
            "id": "com.example.paged", "name": "分页插件", "version": "1.0.0",
            "entry": "main.wasm",
            "plugin_setting": {
                "pages": [{"id": "basic", "label": "基础"}],
                "controls": [{"kind": "status", "key": "conn", "label": "状态", "page": "nope"}]
            }
        }))
        .unwrap();
        let e = c.validate().unwrap_err().to_string();
        assert!(e.contains("未在 plugin_setting.pages 里声明"), "{e}");
    }

    /// 分页 id 重复 / 缺 label 都算插件作者的错。
    #[test]
    fn broken_page_declarations_are_rejected() {
        let dup: PluginConfig = serde_json::from_value(json!({
            "id": "com.example.paged", "name": "分页插件", "version": "1.0.0",
            "entry": "main.wasm",
            "plugin_setting": {"pages": [{"id": "a", "label": "A"}, {"id": "a", "label": "B"}]}
        }))
        .unwrap();
        assert!(dup.validate().unwrap_err().to_string().contains("id 重复"));

        let no_label: PluginConfig = serde_json::from_value(json!({
            "id": "com.example.paged", "name": "分页插件", "version": "1.0.0",
            "entry": "main.wasm",
            "plugin_setting": {"pages": [{"id": "a"}]}
        }))
        .unwrap();
        assert!(no_label
            .validate()
            .unwrap_err()
            .to_string()
            .contains("缺少 label"));
    }

    /// 派生远程类型：合法声明通过，并且保留 base_type / defaults。
    ///
    /// `capabilities` 里**不需要**写 `remote_types`：派生类型是元数据，不是可执行操作
    /// （见 [`CAP_GROUPS`]）。这里只申请真正会调用的 `kv`。
    #[test]
    fn remote_types_parse_and_validate() {
        let c: PluginConfig = serde_json::from_value(json!({
            "id": "com.example.kvault", "name": "K-Vault", "version": "1.0.0",
            "entry": "main.wasm",
            "capabilities": ["kv"],
            "remote_types": [{
                "id": "k-vault",
                "name": "K-Vault（WebDAV）",
                "base_type": "webdav",
                "defaults": {"vendor": "other", "url": "https://example.invalid"}
            }]
        }))
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.remote_types.len(), 1);
        assert_eq!(c.remote_types[0].id, "k-vault");
        assert_eq!(c.remote_types[0].base_type, "webdav");
        assert_eq!(
            c.remote_types[0].defaults.get("vendor").map(|s| s.as_str()),
            Some("other")
        );
    }

    /// 没有任何能力组的插件也能声明派生类型（元数据不需要授权）。
    #[test]
    fn remote_types_need_no_capability_group() {
        let c: PluginConfig = serde_json::from_value(json!({
            "id": "com.example.kvault", "name": "K-Vault", "version": "1.0.0",
            "entry": "main.wasm",
            "capabilities": [],
            "remote_types": [{"id": "k-vault", "name": "K-Vault", "base_type": "webdav"}]
        }))
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.remote_types.len(), 1);
    }

    /// 真实插件 `kvault` 的 config.json 必须能被宿主解析。
    ///
    /// 这条检查跨仓库契约：插件仓库独立发版，字段形状（本地化的 `name` 对象、
    /// `plugin_setting.pages`、`remote_types`）一旦和宿主解析器对不上，用户装上的
    /// 就是"看得见、加载不了"的插件。仓库不在时跳过——别的 checkout 或 CI 里
    /// 没有 `plugin_repos/` 目录不能因此变红。
    #[test]
    fn real_kvault_config_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugin_repos/rcgui-plugin-kvault/config.json");
        let Ok(raw) = std::fs::read(&path) else {
            eprintln!("跳过：没有 {}（不在完整 checkout 里）", path.display());
            return;
        };
        let c: PluginConfig = serde_json::from_slice(&raw)
            .unwrap_or_else(|e| panic!("{} 解析失败：{e}", path.display()));
        c.validate()
            .unwrap_or_else(|e| panic!("{} 校验失败：{e}", path.display()));
        assert_eq!(c.id, "kvault");
        // 该插件声明的能力组必须都是宿主认识的可执行组
        for cap in &c.capabilities {
            assert!(
                CAP_GROUPS.contains(&cap.as_str()),
                "宿主不认识的能力组：{cap}"
            );
        }
        // 派生类型仍然在（kvault 走 webdav + kvault 参数）
        assert_eq!(c.remote_types.len(), 1);
        assert_eq!(c.remote_types[0].id, "kvault");
        assert_eq!(c.remote_types[0].base_type, "webdav");
        // 分页标签齐全，否则界面会显示空页签
        assert!(!c.plugin_setting.pages.is_empty());
    }

    /// base_type 必须像后端名，且不能等于自己的 id。
    #[test]
    fn remote_types_reject_bad_base_type() {
        let with_cap = |rt: serde_json::Value| -> PluginConfig {
            serde_json::from_value(json!({
                "id": "com.example.kvault", "name": "K-Vault", "version": "1.0.0",
                "entry": "main.wasm",
                "capabilities": ["kv"],
                "remote_types": [rt]
            }))
            .unwrap()
        };
        let e = with_cap(json!({"id": "k-vault", "name": "KV", "base_type": "WebDAV"}))
            .validate()
            .unwrap_err()
            .to_string();
        assert!(e.contains("不是合法的后端名"), "{e}");

        let e = with_cap(json!({"id": "k-vault", "name": "KV", "base_type": "k-vault"}))
            .validate()
            .unwrap_err()
            .to_string();
        assert!(e.contains("不能等于自己的 id"), "{e}");

        // defaults 不允许改写 type
        let e = with_cap(json!({
            "id": "k-vault", "name": "KV", "base_type": "webdav",
            "defaults": {"type": "s3"}
        }))
        .validate()
        .unwrap_err()
        .to_string();
        assert!(e.contains("不能设置 type"), "{e}");
    }

    /// 旧插件（没有 pages / remote_types 字段）必须继续能解析。
    #[test]
    fn legacy_config_without_new_fields_still_parses() {
        let c: PluginConfig = serde_json::from_value(json!({
            "id": "onerclone", "name": "一键 rclone", "version": "0.2.0",
            "entry": "main.wasm",
            "plugin_setting": {"controls": [{"kind": "status", "key": "conn", "label": "状态"}]}
        }))
        .unwrap();
        c.validate().unwrap();
        assert!(c.plugin_setting.pages.is_empty());
        assert!(c.remote_types.is_empty());
        assert!(c.plugin_setting.controls[0].page.is_empty());
    }
}
