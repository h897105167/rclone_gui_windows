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
/// private_key / api_key / sas_url / bearer / refresh_* / api_hash / access_key，
/// 以及独立的 `key`、`pwd`，还有任何以 `_key`、`_token`、`_pass`、`_secret`
/// 结尾的名字（插件的命名习惯多落在这几个后缀上，比如 imagehost 的 `short_key`）。
///
/// 副作用是 `public_key` 这类"名字像凭据、其实可以公开"的字段也会被剥掉；
/// 这是有意的取舍——漏剥一次就是凭据外泄，多剥一次只是导入后要重填。
/// 插件 manifest 自己声明的密码字段另算，见 [`PluginRef::secret_keys`]。
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
        "api_hash",
        "access_key",
        "auth_key",
        "cookie",
    ];
    const SUFFIXES: &[&str] = &["_key", "_token", "_pass", "_secret"];
    k == "key"
        || k == "pwd"
        || NEEDLES.iter().any(|n| k.contains(n))
        || SUFFIXES.iter().any(|s| k.ends_with(s))
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
                let flagged = o
                    .get("IsPassword")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false)
                    || o.get("Sensitive")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
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

/// 递归删掉 JSON 里名字像凭据的叶子，删掉的叶子数写进 `removed`。
///
/// 插件把设置按自己的形状存（imagehost 实测是 `{"form":{…,"short_key":…}}`，
/// kvault 是 `{"site":{"token":…}}`），层级和键名都不归我们管，所以只能一路走到
/// 叶子逐个判断：`declared`（插件 manifest 声明的密码字段）与关键词规则取并集。
/// `_state` 是插件自己的过程状态，出现在任何层级都不导出。
fn strip_secret_leaves(value: &mut Value, declared: &HashSet<String>, removed: &mut usize) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let lk = k.to_ascii_lowercase();
                if lk == "_state" || declared.contains(&lk) || is_secret_key(&k) {
                    if let Some(v) = map.remove(&k) {
                        *removed += count_leaves(&v);
                    }
                } else if let Some(v) = map.get_mut(&k) {
                    strip_secret_leaves(v, declared, removed);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                strip_secret_leaves(v, declared, removed);
            }
        }
        _ => {}
    }
}

/// 一个值里有多少个叶子（标量算 1 个，空容器也算 1 个，免得删了个空对象却报 0 项）。
fn count_leaves(value: &Value) -> usize {
    match value {
        Value::Object(m) => m.values().map(count_leaves).sum::<usize>().max(1),
        Value::Array(a) => a.iter().map(count_leaves).sum::<usize>().max(1),
        _ => 1,
    }
}

/// 删掉剥离之后只剩骨架的空对象/空数组。
///
/// 不删也不泄露，但导出里留着 `"form": {}` 会让人以为「这个插件的表单没配置」，
/// 而实际情况是「配置都在，只是凭据被剥了」。
fn prune_empty(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let drop_it = match map.get_mut(&k) {
                    Some(v) => {
                        prune_empty(v);
                        is_empty_container(v)
                    }
                    None => false,
                };
                if drop_it {
                    map.remove(&k);
                }
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                prune_empty(v);
            }
            items.retain(|v| !is_empty_container(v));
        }
        _ => {}
    }
}

fn is_empty_container(value: &Value) -> bool {
    match value {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
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
    ///
    /// 代理凭据同样纳入本策略（`profile.proxies[].user/pass`）：
    /// 实测 rclone 把代理密码以**百分号编码的明文**写进 `rclone.conf`，
    /// 连 obscure 都不是，所以默认导出必须剥离，且剥离后要在档案里标出来，
    /// 让导入方知道「这条代理配置需要重新填凭据」。
    pub fn apply_secret_policy(&mut self, include_secrets: bool, providers: Option<&Value>) {
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
        // ---- 代理凭据 ----
        let mut stripped_proxies = 0usize;
        for p in &mut self.profile.proxies {
            if include_secrets {
                continue;
            }
            if !p.user.is_empty() || !p.pass.is_empty() {
                stripped_proxies += 1;
            }
            p.user = String::new();
            p.pass = String::new();
        }
        // ---- 插件设置 ----
        // 插件设置是插件自己命名的键、层级也不固定（`form.<key>`、`site.token`…），
        // 所以递归走到叶子：manifest 声明的密码字段 ∪ 关键词规则。
        // `_state` 是插件自己写的过程状态，换台机器没意义，任何情况下都不导出。
        // 剥不掉也不能当作"已安全"，所以插件的 `enabled` 不因这里的结果改变。
        let mut stripped_plugin_keys = 0usize;
        for p in &mut self.plugins {
            // `_state` 是过程状态：不剥成"安全"，而是根本不写出去。
            p.settings.remove("_state");
            if include_secrets || p.settings.is_empty() {
                continue;
            }
            let declared: HashSet<String> = p
                .secret_keys
                .iter()
                .map(|k| k.to_ascii_lowercase())
                .collect();
            let mut whole = Value::Object(std::mem::take(&mut p.settings));
            strip_secret_leaves(&mut whole, &declared, &mut stripped_plugin_keys);
            prune_empty(&mut whole);
            if let Value::Object(m) = whole {
                p.settings = m;
            }
        }
        // ---- 备份密码 ----
        // 置 `None` 而不是空串：空串会被导入端当成"用户想把密码清空"，
        // 而这里的意思只是"这份文件没带密码"。
        let mut stripped_backup_pass = false;
        if let Some(a) = self.app.as_mut() {
            if !include_secrets && a.backup_pass.is_some() {
                a.backup_pass = None;
                stripped_backup_pass = true;
            }
        }
        self.sensitive = include_secrets
            && (self.remotes.iter().any(|r| !r.params.is_empty())
                || self
                    .profile
                    .proxies
                    .iter()
                    .any(|p| !p.user.is_empty() || !p.pass.is_empty())
                || self.plugins.iter().any(|p| !p.settings.is_empty())
                || self
                    .app
                    .as_ref()
                    .map(|a| a.backup_pass.is_some())
                    .unwrap_or(false));
        self.warnings
            .retain(|w| !w.contains(rcgui_i18n::t("已排除")));
        if include_secrets && self.sensitive {
            self.warnings.push(
                rcgui_i18n::t("该文件包含远程凭据（OAuth token / 密码）与代理账号。rclone 的 obscure 只是可逆混淆、\
                 并非加密；代理密码更是以明文（百分号编码）保存，请存放在受控位置")
                    .to_string(),
            );
        }
        if stripped_any > 0 {
            self.warnings.push(rcgui_i18n::f("已排除 {} 个远程的凭据字段；这些远程在导入后标记为「凭据未随包导出」（可能需要重新认证）\
                 （表示凭据未随包导出，不代表其认证状态已被验证）", &[&(stripped_any)]));
        }
        if stripped_proxies > 0 {
            self.warnings.push(rcgui_i18n::f(
                "已排除 {} 条代理配置的账号密码；导入后需要重新填写才能连接代理",
                &[&(stripped_proxies)],
            ));
        }
        if stripped_plugin_keys > 0 {
            self.warnings.push(rcgui_i18n::f(
                "已排除 {} 项插件设置中的凭据字段；相关插件导入后可能需要重新配置",
                &[&(stripped_plugin_keys)],
            ));
        }
        if stripped_backup_pass {
            self.warnings.push(
                rcgui_i18n::t("已排除 WebDAV 备份密码；导入后需要重新填写才能推送到备份地址")
                    .to_string(),
            );
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

/// 插件身份与来源记录。
///
/// **只记录"装的是哪个插件"，不携带插件二进制**：包体可能几十 MB，把它塞进
/// 一份人可读的配置里既难传也难审。导入时按这里记录的 id + 来源重新安装，
/// 商店来源会校验 sha256 后再落盘。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PluginRef {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 导出时的版本；导入后由实际安装的包版本决定，仅用于提示"版本不同"
    #[serde(default)]
    pub version: String,
    /// 来源：`"builtin"`（随程序）| `"store"`（商店清单）| `"local"`（本地文件/目录）
    #[serde(default)]
    pub source: String,
    /// 商店清单里的下载地址；非商店来源为空
    #[serde(default)]
    pub url: String,
    /// 期望的包 sha256（小写十六进制）；空表示不做哈希校验（本地来源）
    #[serde(default)]
    pub sha256: String,
    /// 导出时是否启用。导入后**不会**自动启用高权限插件，见导入流程。
    #[serde(default)]
    pub enabled: bool,
    /// 插件自己的设置，键名来自插件 kv；可能含凭据，按 include_secrets 决定是否写出
    #[serde(default)]
    pub settings: Map<String, Value>,
    /// 插件 manifest 声明为密码类字段（`control.field == "password"`）的键名，小写。
    ///
    /// 不写进文件：它只服务于导出时的凭据剥离，导入方应当按**自己那边**装着的
    /// 包重新判断，而不是相信对方的声明。插件里那些名字古怪的字段（`short_key`）
    /// 靠这份声明才认得出来。
    #[serde(skip)]
    pub secret_keys: Vec<String>,
}

/// 可移植的程序设置。
///
/// 只放「换一台机器也成立」的项：主题、语言、托盘行为、内核模式与路径、
/// 轻量模式阈值、带宽上限。窗口位置、最近路径等本机状态不放，否则换个屏幕
/// 打开就跑到视野外。
///
/// **每个字段都是 `Option`**，因为「这份文件没有提到这一项」和「这一项的值是
/// 空/零」是两件事：早期 v1/v2 配置文件根本没有 `app` 段，手写的文件也可能
/// 只写了其中几项。用 `Option` 才能做到「只覆盖文件里确实写了的项」，否则
/// 一次导入就会把用户自己选的内核路径、主题清成默认值。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_mode: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent_idx: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autostart: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silent_start: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_to_tray: Option<bool>,
    /// `"managed"` | `"external"`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core_update_check: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light_minutes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bw_up: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bw_up_unit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bw_down: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bw_down_unit: Option<i32>,
    /// WebDAV 备份地址与用户名（凭据本身在 `backup_pass`，默认剥离）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_user: Option<String>,
    /// 备份密码。**默认不导出**，只有用户显式要求包含凭据时才写出。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_pass: Option<String>,
}

impl AppSettings {
    /// 文件里实际写了的项（用于导入预览「会覆盖哪些设置」）。
    pub fn present(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        macro_rules! push {
            ($f:expr, $n:expr) => {
                if $f.is_some() {
                    v.push($n);
                }
            };
        }
        push!(self.theme_mode, "界面主题");
        push!(self.accent_idx, "强调色");
        push!(self.language, "语言");
        push!(self.autostart, "开机启动");
        push!(self.silent_start, "静默启动");
        push!(self.close_to_tray, "关闭到托盘");
        push!(self.core_mode, "内核模式");
        push!(self.core_path, "内核路径");
        push!(self.core_update_check, "内核更新检查");
        push!(self.light_enabled, "轻量模式");
        push!(self.light_minutes, "轻量阈值");
        push!(self.bw_up, "上行限速");
        push!(self.bw_up_unit, "上行限速单位");
        push!(self.bw_down, "下行限速");
        push!(self.bw_down_unit, "下行限速单位");
        push!(self.backup_url, "备份地址");
        push!(self.backup_user, "备份用户名");
        push!(self.backup_pass, "备份密码");
        v
    }
}

/// 同步任务的可移植部分。`enabled` 会随包导入，但导入后不会立即执行——
/// 调度器按间隔计算下次时间，避免导入的瞬间对用户网络发起一轮全量传输。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SyncTaskRef {
    pub id: String,
    pub name: String,
    pub local: String,
    pub remote: String,
    /// `"up"` | `"down"`
    pub direction: String,
    /// `"copy"`（不删目标）| `"sync"`（镜像删除）
    pub mode: String,
    pub enabled: bool,
    /// 本机文件夹有变化就提前跑一轮。
    ///
    /// `None` 表示文件里没写这一项，导入端照「本地新建任务」的规则处理：只要是在
    /// 本机新建的任务，默认就跟着变化跑（镜像任务除外，见宿主的 `sync_watch`）。
    /// 写成 `Some(true)` / `Some(false)` 就照抄，不再按默认猜。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_sync: Option<bool>,
    pub interval_hours: u32,
}

/// 一条「这台远程归哪个插件管」的记录，随配置文件一起走。
///
/// 归属本身由插件声明加上用户当时的选择决定，这里带的是导出那一刻的结论。带上它的
/// 唯一理由是：插件被卸载之后，把这份文件拿到另一台机器（或者同一台机器重装）再导入，
/// 宿主仍能认出这台远程属于哪个插件；否则它会被当成普通远程，用原生协议去读写一份
/// 插件托管的数据。
///
/// 导入端不照抄这条记录：它会拿本机远程的实际参数核对标记键，对不上就不登记，并把
/// 原因告诉用户（`crates/app/src/remote_owners.rs` 里的 `decide_import`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RemoteOwnerRef {
    /// rclone 远程名（不含 `:`）。
    pub remote: String,
    /// 接管这台远程的插件 id。
    pub plugin: String,
    /// 插件在远程参数里用的标记键，例如 `kvault`。
    pub marker: String,
    /// 插件当时的显示名，插件不在时用来说明「它归谁」。
    pub label: String,
    /// 这条记录当初怎么来的：`declared` / `legacy` / `scan` / `import`。
    pub source: String,
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
    /// 插件身份/版本/来源/启用状态与设置（无二进制）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<PluginRef>,
    /// 可移植的程序设置（None 表示这份文件不携带程序设置）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppSettings>,
    /// 同步文件夹任务
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sync_tasks: Vec<SyncTaskRef>,
    /// 哪些远程归哪个插件管（导出那一刻的登记表快照）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_owners: Vec<RemoteOwnerRef>,
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
            plugins: Vec::new(),
            app: None,
            sync_tasks: Vec::new(),
            remote_owners: Vec::new(),
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
                "{}",
                rcgui_i18n::f(
                    "不是 rcgui.conf 文件（kind = \"{}\"，应为 \"{}\"）",
                    &[&(found), &(KIND)]
                )
            ),
            Self::VersionTooNew { found, supported } => write!(
                f,
                "{}",
                rcgui_i18n::f(
                    "文件格式版本 {} 高于本程序支持的 {}，请升级 RCGUI",
                    &[&(found), &(supported)]
                )
            ),
            Self::Invalid(m) => write!(f, "{}", rcgui_i18n::f("文件内容不合法: {}", &[&(m)])),
        }
    }
}

impl std::error::Error for BundleError {}

/// 导入前给用户看的摘要（不含任何凭据内容）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
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

impl std::fmt::Display for Summary {
    /// 一行摘要，供命令行输出（界面上用 `conf_io::describe` 拼更长的说明）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.profile_name.trim();
        if name.is_empty() {
            write!(
                f,
                "远程 {} · 挂载 {} · 服务 {}",
                self.remote_count, self.mount_count, self.serve_count
            )
        } else {
            write!(
                f,
                "档案「{}」：远程 {} · 挂载 {} · 服务 {}",
                name, self.remote_count, self.mount_count, self.serve_count
            )
        }
    }
}

/// 与当前档案的差异（按名字/身份比较，不做内容级 diff）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
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
        let mut s = rcgui_i18n::f(
            "远程 +{} / -{} / 保留 {}；挂载 {}→{}，服务 {}→{}",
            &[
                &(self.remotes_added.len()),
                &(self.remotes_removed.len()),
                &(self.remotes_kept),
                &(self.mounts_before),
                &(self.mounts_after),
                &(self.serves_before),
                &(self.serves_after),
            ],
        );
        if !self.remotes_removed.is_empty() {
            s.push_str(&rcgui_i18n::f(
                "；将被移除的远程：{}",
                &[&(self.remotes_removed.join("、"))],
            ));
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
                rcgui_i18n::t("条目数量超出上限（远程 ≤200，挂载/服务各 ≤500）").to_string(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for r in &self.remotes {
            let name = r.name.trim();
            if name.is_empty() {
                return Err(BundleError::Invalid(
                    rcgui_i18n::t("存在空远程名").to_string(),
                ));
            }
            if name.contains(':') || name.contains(' ') {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "远程名 \"{}\" 不能包含冒号或空格",
                    &[&(name)],
                )));
            }
            if !seen.insert(name.to_ascii_lowercase()) {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "远程名 \"{}\" 重复",
                    &[&(name)],
                )));
            }
            if r.rtype.trim().is_empty() {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "远程 \"{}\" 缺少后端类型",
                    &[&(name)],
                )));
            }
        }
        // 脚本清单：只做路径安全检查。导入阶段不会执行也不会写盘。
        for s in &self.script_manifest {
            if s.path.is_empty() {
                return Err(BundleError::Invalid(
                    rcgui_i18n::t("脚本清单存在空路径").to_string(),
                ));
            }
            let p = std::path::Path::new(&s.path);
            if p.is_absolute()
                || s.path.starts_with('/')
                || s.path.starts_with('\\')
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "脚本路径 \"{}\" 不允许绝对路径或 ..",
                    &[&(s.path)],
                )));
            }
        }
        // 插件清单：只校验身份字段。这里**不**判断插件是否存在、是否兼容——
        // 那要读磁盘，属于导入预览阶段的事（validate 不做 IO）。
        if self.plugins.len() > 64 {
            return Err(BundleError::Invalid(
                rcgui_i18n::t("插件条目超出上限（≤64）").to_string(),
            ));
        }
        for p in &self.plugins {
            let id = p.id.trim();
            if id.is_empty() {
                return Err(BundleError::Invalid(
                    rcgui_i18n::t("插件清单存在空 id").to_string(),
                ));
            }
            if id.chars().any(|c| c.is_control() || c == '/' || c == '\\') {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "插件 id \"{}\" 含非法字符",
                    &[&(id)],
                )));
            }
            if p.version.trim().is_empty() {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "插件 \"{}\" 缺少版本号",
                    &[&(id)],
                )));
            }
            for k in p.settings.keys() {
                if k.trim().is_empty() {
                    return Err(BundleError::Invalid(rcgui_i18n::f(
                        "插件 \"{}\" 的设置项存在空键名",
                        &[&(id)],
                    )));
                }
            }
        }
        // 同步任务：路径检查只做安全形状判断（是否绝对路径随机器而变，不能在这里拒绝）。
        if self.sync_tasks.len() > 64 {
            return Err(BundleError::Invalid(
                rcgui_i18n::t("同步任务超出上限（≤64）").to_string(),
            ));
        }
        let mut seen_tasks = std::collections::HashSet::new();
        for t in &self.sync_tasks {
            if t.local.trim().is_empty() || t.remote.trim().is_empty() {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "同步任务 \"{}\" 缺少本地或远程路径",
                    &[&(t.name)],
                )));
            }
            if !t.id.is_empty() && !seen_tasks.insert(t.id.to_ascii_lowercase()) {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "同步任务 id \"{}\" 重复",
                    &[&(t.id)],
                )));
            }
            let dir = t.direction.to_ascii_lowercase();
            if !dir.is_empty() && dir != "up" && dir != "down" {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "同步任务 \"{}\" 的方向 \"{}\" 不合法（只能是 up 或 down）",
                    &[&(t.name), &(t.direction)],
                )));
            }
            let mode = t.mode.to_ascii_lowercase();
            if !mode.is_empty() && mode != "copy" && mode != "sync" {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "同步任务 \"{}\" 的模式 \"{}\" 不合法（只能是 copy 或 sync）",
                    &[&(t.name), &(t.mode)],
                )));
            }
            if t.interval_hours > 720 {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "同步任务 \"{}\" 的间隔 {} 小时过大（上限 720）",
                    &[&(t.name), &(t.interval_hours)],
                )));
            }
        }
        // 远程归属记录：只是「哪个名字归哪个插件」的对照表，形状不对就整份文件不受理，
        // 免得导入端拿一条看不懂的记录去改本机的登记表。
        if self.remote_owners.len() > 200 {
            return Err(BundleError::Invalid(
                rcgui_i18n::t("远程归属记录超出上限（≤200）").to_string(),
            ));
        }
        for o in &self.remote_owners {
            let name = o.remote.trim();
            if name.is_empty() || o.plugin.trim().is_empty() {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "远程归属记录 \"{}\" 缺少远程名或插件 id",
                    &[&(o.remote)],
                )));
            }
            // rclone 的远程名不能带冒号与空白，与远程列表的规则一致。
            if name.contains(':') || name.chars().any(char::is_whitespace) {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "远程归属记录里的远程名 \"{}\" 不合法",
                    &[&(o.remote)],
                )));
            }
            if o.plugin
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\')
            {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "远程归属记录 \"{}\" 的插件 id \"{}\" 不合法",
                    &[&(o.remote), &(o.plugin)],
                )));
            }
            // 标记键就是 rclone 的参数名：空着、带 `=`、或带控制字符都没法拿去查参数。
            let marker = o.marker.trim();
            if marker.is_empty() || marker.contains('=') || marker.chars().any(char::is_control) {
                return Err(BundleError::Invalid(rcgui_i18n::f(
                    "远程归属记录 \"{}\" 的标记键 \"{}\" 不合法",
                    &[&(o.remote), &(o.marker)],
                )));
            }
        }
        // 程序设置：枚举值必须认识，否则宁可报错也不要在界面上显示一个选不中的值。
        if let Some(a) = self.app.as_ref() {
            if let Some(mode) = a.core_mode.as_deref() {
                let m = mode.trim().to_ascii_lowercase();
                if !m.is_empty() && m != "managed" && m != "external" {
                    return Err(BundleError::Invalid(rcgui_i18n::f(
                        "程序设置的内核模式 \"{}\" 不合法（只能是 managed 或 external）",
                        &[&(mode)],
                    )));
                }
            }
            if let Some(minutes) = a.light_minutes {
                if minutes > 1440 {
                    return Err(BundleError::Invalid(rcgui_i18n::f(
                        "程序设置的轻量模式阈值 {} 分钟过大（上限 1440）",
                        &[&(minutes)],
                    )));
                }
            }
        }
        Ok(())
    }

    /// 文件里记录的插件数量（预览文案用）。
    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    /// 文件里记录的同步任务数量（预览文案用）。
    pub fn sync_count(&self) -> usize {
        self.sync_tasks.len()
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
            .any(|w| w.contains(rcgui_i18n::t("由服务名称映射还原")))
        {
            self.warnings.push(rcgui_i18n::f(
                "该文件未记录服务参数，已按服务名称映射还原 {} 个服务（地址/协议/远程取自映射）；\
                 用户名与密码需在导入后重新填写",
                &[&(self.profile.serves.len())],
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
            sensitives: self.remotes.iter().filter(|r| !r.params.is_empty()).count(),
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
    fn rejects_oversized_documents() {
        let mut c = sample();
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
        RemoteConf {
            name: "r".into(),
            rtype: rtype.into(),
            params,
            needs_auth: false,
        }
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
        c.remotes.push(remote(
            "drive",
            &[
                ("root_folder_id", "abc"),
                ("token", "secret"),
                ("client_secret", "s"),
            ],
        ));
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
        c.remotes
            .push(remote("drive", &[("token", "t"), ("root_folder_id", "x")]));
        c.apply_secret_policy(false, None);
        assert_eq!(
            c.remotes[0].params.len(),
            1,
            "无元数据时关键词规则仍须剥离 token"
        );
        assert!(c.remotes[0].needs_auth);
    }

    fn proxy_with_creds() -> crate::ProxyProfile {
        crate::ProxyProfile {
            id: "p1".into(),
            name: "公司代理".into(),
            protocol: crate::ProxyProtocol::Http,
            host: "10.0.0.1".into(),
            port: 8080,
            user: "alice".into(),
            pass: "hunter2".into(),
        }
    }

    /// 代理密码是**明文**（实测 rclone 连 obscure 都不做），默认导出必须剥离。
    #[test]
    fn default_export_strips_proxy_credentials() {
        let mut c = RcguiConf::default();
        c.profile.proxies.push(proxy_with_creds());
        c.apply_secret_policy(false, None);
        let p = &c.profile.proxies[0];
        assert_eq!(p.user, "", "代理用户名必须被剥离");
        assert_eq!(p.pass, "", "代理密码必须被剥离");
        // 其余字段要保留，否则导入后用户还要重填地址
        assert_eq!(p.host, "10.0.0.1");
        assert_eq!(p.port, 8080);
        assert_eq!(p.name, "公司代理");
        assert!(c.warnings.iter().any(|w| w.contains("代理配置的账号密码")));
        // 没勾「包含凭据」时不应把文件标成敏感
        assert!(!c.sensitive);
    }

    #[test]
    fn opt_in_keeps_proxy_credentials_and_flags_sensitive() {
        let mut c = RcguiConf::default();
        c.profile.proxies.push(proxy_with_creds());
        c.apply_secret_policy(true, None);
        assert_eq!(c.profile.proxies[0].pass, "hunter2");
        assert!(c.sensitive, "含代理凭据的包必须标为敏感");
        assert!(c.warnings.iter().any(|w| w.contains("并非加密")));
        assert!(c.warnings.iter().any(|w| w.contains("代理密码更是以明文")));
    }

    /// 没有代理的包，行为必须与改动前完全一致（不能因为加了代理提示而多出 warning）。
    #[test]
    fn no_proxy_means_no_proxy_warnings() {
        let mut c = RcguiConf::default();
        c.remotes.push(remote("local", &[("nounc", "true")]));
        c.apply_secret_policy(false, None);
        assert!(!c.warnings.iter().any(|w| w.contains("代理配置的账号密码")));
        assert!(!c.sensitive);
    }

    /// 代理密码为空、只有用户名时也算凭据，同样要剥离。
    #[test]
    fn proxy_with_username_only_is_still_stripped() {
        let mut c = RcguiConf::default();
        let mut p = proxy_with_creds();
        p.pass = String::new();
        c.profile.proxies.push(p);
        c.apply_secret_policy(false, None);
        assert_eq!(c.profile.proxies[0].user, "");
        assert!(c.warnings.iter().any(|w| w.contains("代理配置的账号密码")));
    }

    /// 代理凭据剥离必须幂等（重复调用不会叠加 warning 或误伤）。
    #[test]
    fn proxy_strip_is_idempotent() {
        let mut c = RcguiConf::default();
        c.profile.proxies.push(proxy_with_creds());
        c.apply_secret_policy(false, None);
        c.apply_secret_policy(false, None);
        let warned = c
            .warnings
            .iter()
            .filter(|w| w.contains("代理配置的账号密码"))
            .count();
        assert_eq!(warned, 0, "第二次调用时已无凭据可剥离，不应再报警告");
    }

    fn plugin(id: &str, declared: &[&str], settings: Value) -> PluginRef {
        PluginRef {
            id: id.into(),
            name: id.into(),
            version: "0.1.0".into(),
            source: "local".into(),
            url: String::new(),
            sha256: String::new(),
            enabled: true,
            settings: settings.as_object().cloned().unwrap_or_default(),
            secret_keys: declared.iter().map(|k| (*k).to_string()).collect(),
        }
    }

    /// 插件设置的真实形状是嵌套的（`{"form":{…}}`、`{"site":{…}}`），
    /// 默认导出必须走到底层把凭据剥掉，包括 manifest 声明、但关键词认不出来的键。
    #[test]
    fn plugin_settings_strip_nested_and_declared_secrets() {
        let mut c = RcguiConf::default();
        c.plugins.push(plugin(
            "imagehost",
            &["short_key", "uglyfield"],
            serde_json::json!({
                "form": {
                    "remote": "img",
                    "upload_path": "/i",
                    "short_key": "k-123",
                    "uglyfield": "sensitive-without-keyword"
                },
                "site": { "host": "example.test", "token": "t-456" },
                "_state": { "pct": 10 }
            }),
        ));
        c.apply_secret_policy(false, None);

        let s = &c.plugins[0].settings;
        assert_eq!(s["form"]["remote"], serde_json::json!("img"));
        assert_eq!(s["form"]["upload_path"], serde_json::json!("/i"));
        assert!(s["form"].get("short_key").is_none(), "声明的密码字段要剥掉");
        assert!(
            s["form"].get("uglyfield").is_none(),
            "关键词认不出来时靠 manifest 声明识别"
        );
        assert_eq!(s["site"]["host"], serde_json::json!("example.test"));
        assert!(s["site"].get("token").is_none(), "嵌套的关键词凭据要剥掉");
        assert!(s.get("_state").is_none(), "过程状态任何情况下都不导出");
        assert!(c
            .warnings
            .iter()
            .any(|w| w.contains("插件设置中的凭据字段")));
        assert!(!c.sensitive);
    }

    /// 数组里的对象同样要递归处理：插件把多组账号放在数组里是常见形状。
    #[test]
    fn plugin_settings_strip_inside_arrays() {
        let mut c = RcguiConf::default();
        c.plugins.push(plugin(
            "multi",
            &[],
            serde_json::json!({
                "accounts": [
                    { "name": "a", "password": "p1" },
                    { "name": "b", "refresh_token": "r2" }
                ]
            }),
        ));
        c.apply_secret_policy(false, None);

        let accounts = c.plugins[0].settings["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0]["name"], serde_json::json!("a"));
        assert!(accounts[0].get("password").is_none());
        assert_eq!(accounts[1]["name"], serde_json::json!("b"));
        assert!(accounts[1].get("refresh_token").is_none());
    }

    /// 用户显式勾了"含凭据"时设置原样写出，只标 `sensitive`；`_state` 仍然不导出。
    #[test]
    fn plugin_settings_opt_in_keeps_values() {
        let mut c = RcguiConf::default();
        c.plugins.push(plugin(
            "imagehost",
            &["short_key"],
            serde_json::json!({
                "form": { "remote": "img", "short_key": "k-123" },
                "_state": { "pct": 10 }
            }),
        ));
        c.apply_secret_policy(true, None);

        let s = &c.plugins[0].settings;
        assert_eq!(s["form"]["short_key"], serde_json::json!("k-123"));
        assert!(s.get("_state").is_none());
        assert!(c.sensitive);
        assert!(c.warnings.iter().any(|w| w.contains("并非加密")));
    }

    /// 只被 `form` 包住、键名又很短的凭据（imagehost 的 `short_key`）靠后缀规则也要命中。
    #[test]
    fn is_secret_key_covers_short_and_hash_names() {
        for k in [
            "short_key",
            "access_key",
            "api_hash",
            "client_secret",
            "refresh_token",
            "bearer",
            "pwd",
        ] {
            assert!(is_secret_key(k), "{k} 应被识别为凭据");
        }
        for k in ["remote", "upload_path", "url", "user", "root_folder_id"] {
            assert!(!is_secret_key(k), "{k} 不是凭据");
        }
    }
}
