//! 插件宿主侧实现：内置插件释放 / 安装 / 卸载，以及通用设置面板的合成。
//!
//! # 插件包格式（v2）
//!
//! ```text
//! <插件目录>/
//!   config.json     id / name / version / entry / tags / plugin_setting / capabilities
//!   check.sha256    目录内每个文件一行 `<sha256>  <相对路径>`（安装时逐条校验）
//!   main.wasm       入口脚本（WASI，stdin 收一个 JSON、stdout 回一个 JSON）
//! ```
//!
//! 「内置」只表示**随软件一起安装到用户电脑**，与用户自己装的插件在磁盘上完全同构，
//! 因此同样可以卸载。卸载后在本程序的 `plugin_removed` 里留一条"墓碑"，
//! 下次启动不再自动释放；用户点「恢复内置」时删掉墓碑并重新释放。
//!
//! # 面板协议
//!
//! 宿主向插件发 `{"action": "...", "form": {...}, "live": {...}}`，
//! 插件回 `{"id":1,"ops":[...],"result":{"status":200,"body":{...}}}`，`body` 支持：
//!
//! | 字段 | 含义 |
//! |------|------|
//! | `values` | 写回表单并落 kv（例如选完文件后的路径） |
//! | `checks` | 校验清单 `[{label,value,ok}]` |
//! | `status` / `lamp` | 状态行文本与指示灯（`ok` / `warn` / `err`） |
//! | `hints` | 按控件 key 覆盖提示文字（正文以 `⚠` 开头则用警告色） |
//! | `enabled` | 按控件 key 覆盖可用性 |
//! | `progress` | 0..100；负数为不确定进度 |
//! | `toast` | `{level,message}`：完成后立刻弹一条提示 |
//! | `rc` | `{url,user,pass,connect}`：告诉宿主该连到哪个 rclone 端点 |
//!
//! 动作（`action`）由 `config.json` 中 `kind: "action"` 控件的 `key` 决定，宿主不解释语义。

use crate::config;
use crate::kv::KvStore;
use crate::{caps, Ctx, PanelBtn, PanelRow, PluginRow};
use anyhow::{anyhow, bail, Context, Result};
use plugin_host::config::{ControlKind, FieldKind, PluginConfig, PluginSetting, CONFIG_JSON};
use plugin_host::{bundle, Allowed};
use serde_json::{json, Map, Value};
use slint::{ModelRc, SharedString, VecModel};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// 插件根目录（用户插件与内置插件都装在这里，二者同构）。
pub fn plugins_dir() -> PathBuf {
    config::data_dir().join("plugins")
}

/// 应用自身版本（用于 `min_gui_version` 校验）。
pub fn gui_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 随软件分发的内置插件目录（`<exe>/assets/plugins`）。
pub fn builtin_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join("assets").join("plugins");
    dir.is_dir().then_some(dir)
}

/// 内置插件 id 列表（读目录里每个子包的 config.json）。
pub fn builtin_ids() -> Vec<String> {
    let Some(root) = builtin_root() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || !dir.join(CONFIG_JSON).is_file() {
            continue;
        }
        if let Some(cfg) = bundle::read_config_lenient(&dir) {
            out.push(cfg.id);
        }
    }
    out.sort();
    out
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("复制失败: {}", from.display()))?;
        }
    }
    Ok(())
}

/// 首次运行（或用户点「恢复内置」）时把内置插件释放到插件目录。
///
/// 被用户卸载过的内置插件留了墓碑，这里**不会**复活它。
/// 返回本次实际释放的插件 id。
pub fn ensure_builtin(ctx: &Arc<Ctx>) -> Vec<String> {
    let Some(root) = builtin_root() else {
        return Vec::new();
    };
    let removed = ctx.cfg.lock().map(|c| c.plugin_removed.clone()).unwrap_or_default();
    let dest_root = plugins_dir();
    let mut installed = Vec::new();
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    for entry in rd.flatten() {
        let src = entry.path();
        if !src.is_dir() || !src.join(CONFIG_JSON).is_file() {
            continue;
        }
        let Some(cfg) = bundle::read_config_lenient(&src) else {
            continue;
        };
        if removed.iter().any(|r| r == &cfg.id) {
            continue; // 用户卸载过，尊重选择
        }
        let dest = dest_root.join(plugin_host::config::sanitize_dir_name(&cfg.id));
        // 已安装且版本不低于内置版本 → 不覆盖（用户可能升级过）
        if let Some(cur) = bundle::read_config_lenient(&dest) {
            if !plugin_host::package::is_newer(&cfg.version, &cur.version) {
                continue;
            }
        }
        let first_time = !dest.exists();
        if let Err(e) = copy_dir(&src, &dest) {
            eprintln!("释放内置插件 {} 失败: {e:#}", cfg.id);
            continue;
        }
        // 首次释放的内置插件默认启用：它们替代的是旧版里「恒开」的内置功能，
        // 默认关掉会让升级后的用户莫名其妙地失去原来的能力。
        // （用户手动停用过的不会被覆盖 —— 那条记录已经存在。）
        if first_time {
            if let Ok(mut c) = ctx.cfg.lock() {
                if !c.plugin_enabled.contains_key(&cfg.id) {
                    c.plugin_enabled.insert(cfg.id.clone(), true);
                    let _ = config::save(&c);
                }
            }
        }
        installed.push(cfg.id);
    }
    installed
}

/// 恢复被卸载的随程序插件：清墓碑 + 重新释放。
///
/// 两种情况：
/// - 内置三件套：从 exe 同级 `assets/plugins/<包>/` 复制回插件目录；
/// - `imagehost`：它是**编进 exe 的**单文件 wasm，重新跑一次
///   [`crate::embed_plugins`] 就会写回来（那条路径本身不看墓碑）。
pub fn restore_builtin(ctx: &Arc<Ctx>, id: &str) -> Result<()> {
    {
        let mut cfg = ctx.cfg.lock().unwrap();
        cfg.plugin_removed.retain(|r| r != id);
        cfg.plugin_enabled.insert(id.to_string(), true);
        let _ = config::save(&cfg);
    }
    if id == "imagehost" {
        let removed = ctx
            .cfg
            .lock()
            .map(|c| c.plugin_removed.clone())
            .unwrap_or_default();
        crate::embed_plugins(&removed);
        let p = plugins_dir().join("imagehost.wasm");
        return if p.is_file() {
            Ok(())
        } else {
            Err(anyhow!("无法恢复 imagehost：内嵌 wasm 写出失败"))
        };
    }
    let root = builtin_root().ok_or_else(|| anyhow!("找不到内置插件目录（assets/plugins）"))?;
    let mut found = None;
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            let dir = entry.path();
            if let Some(c) = bundle::read_config_lenient(&dir) {
                if c.id == id {
                    found = Some(dir);
                    break;
                }
            }
        }
    }
    let src = found.ok_or_else(|| anyhow!("内置插件 {id} 不在安装包里"))?;
    let dest = plugins_dir().join(plugin_host::config::sanitize_dir_name(id));
    if dest.exists() {
        std::fs::remove_dir_all(&dest).ok();
    }
    copy_dir(&src, &dest)
}

/// 卸载插件：删插件本体（目录或单文件 .wasm）+ 该插件的 kv 数据 + 清启用记录。
///
/// `path` 是 [`Row::path`] —— 插件在磁盘上的**真实位置**。不能用 id 反推：
/// - 新格式插件是 `plugins/<id>/` 目录；
/// - 单文件插件是 `plugins/<id>.wasm`，而 `plugins/<id>/` 是**它的 kv 目录**，
///   按 id 反推会只删掉 kv、把 wasm 留下来（看起来「卸载了但插件还在」）。
///
/// 随程序发布的插件（内置三件套 + 内嵌的 imagehost）额外写墓碑，避免下次启动被
/// `ensure_builtin` / `embed_plugins` 又释放回来。
pub fn uninstall(ctx: &Arc<Ctx>, id: &str, path: &str) -> Result<PathBuf> {
    let removed = remove_plugin_files(Path::new(path), &plugins_dir(), id)?;
    {
        let mut cfg = ctx.cfg.lock().unwrap();
        cfg.plugin_enabled.remove(id);
        if ships_with_app(id) && !cfg.plugin_removed.iter().any(|r| r == id) {
            cfg.plugin_removed.push(id.to_string());
        }
        let _ = config::save(&cfg);
    }
    Ok(removed)
}

/// 只做文件系统那部分：删插件本体 + 它的 kv 目录。抽出来是为了能单测
/// （[`uninstall`] 还要动 `Ctx` 里的配置，测试里不好造）。
fn remove_plugin_files(path: &Path, plugins_root: &Path, id: &str) -> Result<PathBuf> {
    let removed = if path.is_dir() {
        plugin_host::bundle::uninstall(path)?
    } else if path.is_file() {
        std::fs::remove_file(path).map_err(|e| anyhow!("删除 {} 失败: {e}", path.display()))?;
        path.to_path_buf()
    } else {
        // 路径已不存在：按目录再试一次，仍不行就报错（不要静默成功）
        plugin_host::bundle::uninstall(&plugins_root.join(plugin_host::config::sanitize_dir_name(id)))?
    };
    // 单文件插件的 kv 目录与 wasm 同名，删完本体要顺手清掉
    let kv_dir = plugins_root.join(plugin_host::config::sanitize_dir_name(id));
    if kv_dir.is_dir() && !removed.starts_with(&kv_dir) {
        std::fs::remove_dir_all(&kv_dir).ok();
    }
    Ok(removed)
}

/// 这个 id 是不是「随程序一起发」的（卸载后需要墓碑，否则会被自动释放回来）。
///
/// 除内置三件套外，还有一个特例：`imagehost` 是 `include_bytes!` 编进 exe 的
/// 单文件插件，由 [`crate::embed_plugins`] 在启动时写出，它不在 `assets/plugins` 里。
pub fn ships_with_app(id: &str) -> bool {
    id == "imagehost" || builtin_ids().iter().any(|b| b == id)
}

/// 已被卸载、仍可从安装包里恢复的内置插件：`(id, name, version)`。
///
/// 为什么需要它：卸载后插件卡片会从列表消失，卡片里的「恢复内置」按钮也就跟着
/// 消失，用户将再无入口把内置插件找回来。插件页顶部据此渲染一行「恢复内置」。
/// 判定条件同时要求「有墓碑」与「安装包里确实还带着这个包」，避免列出恢复不了的条目。
pub fn removed_builtins(ctx: &Arc<Ctx>) -> Vec<(String, String, String)> {
    let removed = ctx
        .cfg
        .lock()
        .map(|c| c.plugin_removed.clone())
        .unwrap_or_default();
    if removed.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(root) = builtin_root() {
        if let Ok(rd) = std::fs::read_dir(&root) {
            for entry in rd.flatten() {
                let dir = entry.path();
                let Some(cfg) = bundle::read_config_lenient(&dir) else {
                    continue;
                };
                if !removed.iter().any(|r| r == &cfg.id) {
                    continue;
                }
                out.push((cfg.id.clone(), cfg.name.clone(), cfg.version.clone()));
            }
        }
    }
    // imagehost 是编进 exe 的单文件插件，不在 assets/plugins 里，
    // 名字/版本只能就地写死（它就是「图床」，版本跟 wasm 的 manifest 一致）。
    if removed.iter().any(|r| r == "imagehost")
        && !out.iter().any(|(id, _, _)| id == "imagehost")
    {
        out.push(("imagehost".into(), "图床".into(), String::new()));
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

// ---------------- 列表 ----------------

/// 一行插件信息（纯 Rust 数据，便于在异步任务间传递；UI 侧再转成 `PluginRow`）。
#[derive(Clone, Default)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub version: String,
    pub desc: String,
    pub loaded: bool,
    pub enabled: bool,
    pub kind: String,
    pub author: String,
    pub entry: String,
    pub verified: bool,
    pub builtin: bool,
    pub has_panel: bool,
    pub tags: Vec<String>,
    pub lamp: String,
    pub status: String,
    pub path: String,
}

impl Row {
    /// 检索用的合并文本（名称 / id / 标签 / 描述，均已小写）。
    fn blob(&self) -> String {
        let mut s = format!(
            "{} {} {} {}",
            self.name,
            self.id,
            self.tags.join(" "),
            self.desc
        );
        s.make_ascii_lowercase();
        s
    }

    pub fn to_slint(&self) -> PluginRow {
        let tags: Vec<SharedString> = self.tags.iter().map(|t| t.as_str().into()).collect();
        PluginRow {
            id: self.id.as_str().into(),
            name: self.name.as_str().into(),
            version: self.version.as_str().into(),
            desc: self.desc.as_str().into(),
            loaded: self.loaded,
            enabled: self.enabled,
            kind: self.kind.as_str().into(),
            author: self.author.as_str().into(),
            entry: self.entry.as_str().into(),
            verified: self.verified,
            builtin: self.builtin,
            has_panel: self.has_panel,
            tags: ModelRc::from(Rc::new(VecModel::from(tags))),
            lamp: self.lamp.as_str().into(),
            status: self.status.as_str().into(),
            path: self.path.as_str().into(),
        }
    }
}

/// kv 里的状态字段（插件上次回传的 lamp / status）。
fn kv_str(store: &KvStore, id: &str, key: &str) -> String {
    store
        .get_all(id)
        .get("_state")
        .and_then(|s| s.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 扫描全部插件（新格式 + 旧格式插件包 + 单文件 wasm）。
pub async fn scan_rows(ctx: &Arc<Ctx>) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let enabled_map = ctx.cfg.lock().map(|c| c.plugin_enabled.clone()).unwrap_or_default();
    let builtin = builtin_ids();
    let root = plugins_dir();
    let gv = gui_version();
    let mut seen: Vec<String> = Vec::new();

    // 1) 新格式插件包（目录 + config.json）
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            // 单文件插件（<id>.wasm）在下面单独处理
            if plugin_host::config::ConfigFileNames::detect(&dir).is_none() {
                continue;
            }
            let is_builtin = |id: &str| builtin.iter().any(|b| b == id);
            match bundle::load_from_dir(&dir, &gv) {
                Ok((cfg, _rep)) => {
                    let id = cfg.id.clone();
                    seen.push(id.clone());
                    let panel = !cfg.plugin_setting.controls.is_empty();
                    rows.push(Row {
                        enabled: enabled_map.get(&id).copied().unwrap_or(false),
                        lamp: kv_str(&ctx.store, &id, "lamp"),
                        status: kv_str(&ctx.store, &id, "status"),
                        loaded: true,
                        verified: true,
                        builtin: is_builtin(&id),
                        has_panel: panel,
                        kind: "package".into(),
                        author: cfg.author.clone(),
                        entry: cfg.entry.clone(),
                        tags: cfg.tags.clone(),
                        desc: cfg.description.clone(),
                        name: cfg.name.clone(),
                        version: cfg.version.clone(),
                        path: dir.display().to_string(),
                        id,
                    });
                }
                Err(e) => {
                    // 校验失败也要列出来，否则用户看不出「为什么插件没生效」
                    let Some(cfg) = bundle::read_config_lenient(&dir) else {
                        continue;
                    };
                    let id = cfg.id.clone();
                    seen.push(id.clone());
                    rows.push(Row {
                        enabled: false,
                        lamp: "err".into(),
                        status: format!("{e:#}"),
                        loaded: false,
                        verified: false,
                        builtin: is_builtin(&id),
                        has_panel: !cfg.plugin_setting.controls.is_empty(),
                        kind: "package".into(),
                        author: cfg.author.clone(),
                        entry: cfg.entry.clone(),
                        tags: cfg.tags.clone(),
                        desc: cfg.description.clone(),
                        name: cfg.name.clone(),
                        version: cfg.version.clone(),
                        path: dir.display().to_string(),
                        id,
                    });
                }
            }
        }
    }

    // 2) 旧格式插件包（plugin.json + manifest.sha256）
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("plugin.json").is_file() {
                continue;
            }
            let (meta, verified, status) = match plugin_host::package::load_from_dir(&dir, &gv) {
                Ok((m, _)) => (m, true, String::new()),
                Err(e) => match read_plugin_json_lenient(&dir) {
                    Some(m) => (m, false, format!("{e:#}")),
                    None => continue,
                },
            };
            if seen.iter().any(|s| s == &meta.id) {
                continue;
            }
            let enabled = enabled_map.get(&meta.id).copied().unwrap_or(false);
            seen.push(meta.id.clone());
            rows.push(Row {
                enabled: enabled && verified,
                lamp: if verified { String::new() } else { "err".into() },
                loaded: verified,
                verified,
                has_panel: false,
                builtin: false,
                kind: "package".into(),
                author: meta.author.clone(),
                entry: meta.entry_file.clone(),
                tags: Vec::new(),
                desc: meta.description.clone(),
                name: meta.name.clone(),
                version: meta.version.clone(),
                status,
                path: dir.display().to_string(),
                id: meta.id.clone(),
            });
        }
    }

    // 3) 单文件 WASM 插件（旧机制，保留兼容）
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("wasm") || !p.is_file() {
                continue;
            }
            if let Ok(m) = ctx.host.load_manifest(&p).await {
                if seen.iter().any(|s| s == &m.id) {
                    continue;
                }
                seen.push(m.id.clone());
                // 单文件 wasm 没有 config.json，**没有声明式面板**。
                // imagehost 的设置面板是 Slint 里历史硬编码的一段（`if p.id == "imagehost"`），
                // 与通用面板无关：这里若置 true，通用面板会去读不存在的 config.json 并报错。
                let has_panel = false;
                // imagehost 是 include_bytes! 编进 exe 的，也算「随程序发布」：
                // 打上内置徽标，卸载后才有「恢复内置」入口。
                let ships = m.id == "imagehost";
                rows.push(Row {
                    enabled: enabled_map.get(&m.id).copied().unwrap_or(true),
                    kind: "wasm".into(),
                    loaded: true,
                    verified: true,
                    builtin: ships,
                    has_panel,
                    tags: Vec::new(),
                    author: String::new(),
                    entry: String::new(),
                    lamp: String::new(),
                    status: String::new(),
                    desc: m.description.clone(),
                    name: m.name.clone(),
                    version: m.version.clone(),
                    path: p.display().to_string(),
                    id: m.id.clone(),
                });
            }
        }
    }

    rows.sort_by(|a, b| (!a.builtin, &a.name).cmp(&(!b.builtin, &b.name)));
    rows
}

fn read_plugin_json_lenient(dir: &Path) -> Option<plugin_host::package::PluginJson> {
    let text = std::fs::read_to_string(dir.join("plugin.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// 按检索词过滤（空词 = 全部）。
pub fn filter_rows(rows: &[Row], needle: &str) -> Vec<Row> {
    let n = needle.trim().to_ascii_lowercase();
    if n.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|r| r.blob().contains(&n))
        .cloned()
        .collect()
}

// ---------------- 调用插件 ----------------

/// 打开插件目录（explorer → cmd start 回退），是 `fs.open_dir` 与「打开目录」按钮的共用实现。
pub fn open_path(p: &str) -> Result<()> {
    let mut ok = std::process::Command::new("explorer.exe").arg(p).spawn().is_ok();
    if !ok {
        ok = std::process::Command::new("cmd.exe")
            .args(["/c", "start", "", p])
            .spawn()
            .is_ok();
    }
    if ok {
        Ok(())
    } else {
        bail!("无法打开 {p}")
    }
}

/// 表单取值（kv 里的 `form` 对象，键为控件 key，值统一按字符串存）。
pub fn form_of(store: &KvStore, id: &str) -> Map<String, Value> {
    store
        .get_all(id)
        .get("form")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// 任意 JSON 值按字符串取（表单里的值统一按字符串用）。
fn value_as_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 传给插件的**有效表单**：声明式 `default` 打底，kv 里真正存过的非空值覆盖。
///
/// 为什么不能只传 [`form_of`]：面板渲染在字段为空时用控件 `default` 兜底
/// （见 `compose` 的 `Field` 分支），所以界面上「用户名 gui」看起来是填好的；
/// 但插件拿到的一直是 kv 里存过的键，声明式默认值从没进过 form。插件按空值行动，
/// 就会启动一个**没有任何鉴权**的 rclone —— rc 只放行 `NoAuth` 接口
/// （`core/version` / `rc/list`），挂载、服务、参数读取全部 403。
/// 实测：一键 rclone 的 `rc_user` 默认值 `"gui"` 就是这样丢的，5 项自动恢复全失败。
///
/// 规则与渲染兜底**保持一致**：空串视同未填（回落 `default`），
/// 这样 UI 显示什么、插件就收到什么。默认值只在内存里合并，**不写回磁盘**
/// —— 写回会把「用户没填过」变成「用户填过」，以后改插件默认值就再也生效不了。
pub fn effective_form(setting: &PluginSetting, store: &KvStore, id: &str) -> Map<String, Value> {
    let stored = form_of(store, id);
    let mut form = Map::new();
    for c in &setting.controls {
        if c.default.is_empty() {
            continue;
        }
        let filled = stored
            .get(&c.key)
            .map(value_as_str)
            .filter(|s| !s.is_empty());
        let v = filled.unwrap_or_else(|| c.default.clone());
        if !v.is_empty() {
            form.insert(c.key.clone(), Value::String(v));
        }
    }
    // 控件没声明默认值、但 form 里已经存在的键（用户填过的、插件 `values` 回写的）
    // 一律原样带上，不能被合并过程吃掉。
    for (k, v) in stored {
        form.entry(k).or_insert(v);
    }
    form
}

/// 写一个表单字段（用户输入时立刻落盘，避免崩溃丢配置）。
pub fn set_form_field(store: &KvStore, id: &str, key: &str, value: &str) {
    let mut form = form_of(store, id);
    form.insert(key.to_string(), Value::String(value.to_string()));
    let _ = store.set(id, "form", Value::Object(form));
}

/// 把插件目录 + config 找出来（只处理新格式）。
pub fn loaded_config(id: &str) -> Option<(PathBuf, PluginConfig)> {
    let dir = plugins_dir().join(plugin_host::config::sanitize_dir_name(id));
    let cfg = bundle::read_config_lenient(&dir)?;
    Some((dir, cfg))
}

/// 调一次插件：`action` 由 config.json 的 action 控件 key 决定。
async fn call_plugin(
    ctx: &Arc<Ctx>,
    id: &str,
    action: &str,
    extra: Value,
) -> Result<Value> {
    let (dir, cfg) = loaded_config(id).ok_or_else(|| anyhow!("找不到插件 {id}"))?;
    if !cfg.entry_is_wasm() {
        bail!("插件 {id} 的入口不是 WASM，无法提供设置面板");
    }
    let entry = dir.join(&cfg.entry);
    if !entry.is_file() {
        bail!("入口文件不存在: {}", entry.display());
    }
    let mut params = json!({
        "action": action,
        // 必须是合并过声明式默认值的有效表单：插件看到的要和界面显示的一致，
        // 否则「界面上填好了、插件收到空值」（见 effective_form 的说明）。
        "form": Value::Object(effective_form(&cfg.plugin_setting, &ctx.store, id)),
    });
    if let Some(obj) = params.as_object_mut() {
        if let Some(extra) = extra.as_object() {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    let allowed = Allowed::from_groups(&cfg.capabilities);
    let out = ctx
        .host
        .invoke_with(&entry, "request", params, Some(ctx.caps()), id, &allowed)
        .await?;
    // result.body 是插件的业务返回；没有 body 时按空对象处理
    Ok(out.get("body").cloned().unwrap_or(Value::Null))
}

/// 处理插件返回的 `body`：写回表单/状态，返回需要提示的消息。
/// 面板按钮的纯数据形式（`PanelBtn` 含 `SharedString`，不可跨线程）。
#[derive(Clone, Default)]
pub struct BtnData {
    pub label: String,
    pub action: String,
    pub style: String,
    pub enabled: bool,
}

/// 面板行的纯数据形式。
///
/// `PanelRow`（Slint 生成的结构体）里带 `ModelRc`，内部是 `Rc`，
/// 既不是 `Send` 也不能跨线程移动；而面板是在 tokio 工作线程上合成、
/// 再通过 `upgrade_in_event_loop` 送回 UI 线程的。因此这里用一份纯数据
/// 中转，UI 线程闭包内部再调用 [`panel_to_slint`] 转成 Slint 结构体。
#[derive(Clone, Default)]
pub struct PanelData {
    pub kind: String,
    pub key: String,
    pub label: String,
    pub text: String,
    pub value: String,
    pub field_kind: String,
    pub placeholder: String,
    pub options: Vec<String>,
    pub selected_index: i32,
    pub buttons: Vec<BtnData>,
    pub progress: i32,
    pub ok: bool,
    pub lamp: String,
    pub warning: bool,
    pub enabled: bool,
}

impl PanelData {
    fn to_slint(&self) -> PanelRow {
        let options: Vec<SharedString> = self.options.iter().map(|o| o.as_str().into()).collect();
        let buttons: Vec<PanelBtn> = self
            .buttons
            .iter()
            .map(|b| PanelBtn {
                label: b.label.as_str().into(),
                action: b.action.as_str().into(),
                style: b.style.as_str().into(),
                enabled: b.enabled,
            })
            .collect();
        PanelRow {
            kind: self.kind.as_str().into(),
            key: self.key.as_str().into(),
            label: self.label.as_str().into(),
            text: self.text.as_str().into(),
            value: self.value.as_str().into(),
            field_kind: self.field_kind.as_str().into(),
            placeholder: self.placeholder.as_str().into(),
            options: ModelRc::from(Rc::new(VecModel::from(options))),
            selected_index: self.selected_index,
            buttons: ModelRc::from(Rc::new(VecModel::from(buttons))),
            progress: self.progress,
            ok: self.ok,
            lamp: self.lamp.as_str().into(),
            warning: self.warning,
            enabled: self.enabled,
        }
    }
}

/// 把纯数据面板转成 Slint 模型（**必须在 UI 线程调用**）。
pub fn panel_to_slint(rows: &[PanelData]) -> Vec<PanelRow> {
    rows.iter().map(|r| r.to_slint()).collect()
}

/// 把扫描结果转成 Slint 模型（**必须在 UI 线程调用**，`PluginRow` 含 `ModelRc`）。
pub fn rows_to_slint(rows: &[Row]) -> Vec<PluginRow> {
    rows.iter().map(|r| r.to_slint()).collect()
}
fn apply_body(ctx: &Arc<Ctx>, id: &str, body: &Value) -> Option<(i32, String)> {
    // 1) values → 表单
    if let Some(values) = body.get("values").and_then(|v| v.as_object()) {
        let mut form = form_of(&ctx.store, id);
        for (k, v) in values {
            let s = value_as_str(v);
            form.insert(k.clone(), Value::String(s));
        }
        let _ = ctx.store.set(id, "form", Value::Object(form));
    }
    // 2) 状态
    let mut state = Map::new();
    if let Some(s) = body.get("status").and_then(|v| v.as_str()) {
        state.insert("status".into(), Value::String(s.to_string()));
    }
    if let Some(s) = body.get("lamp").and_then(|v| v.as_str()) {
        state.insert("lamp".into(), Value::String(s.to_string()));
    }
    if let Some(c) = body.get("checks") {
        state.insert("checks".into(), c.clone());
    }
    if !state.is_empty() {
        let mut old = ctx
            .store
            .get_all(id)
            .get("_state")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        for (k, v) in state {
            old.insert(k, v);
        }
        let _ = ctx.store.set(id, "_state", Value::Object(old));
    }
    // 3) 提示
    body.get("toast").and_then(|t| {
        let msg = t.get("message").and_then(|m| m.as_str())?.to_string();
        let kind = match t.get("level").and_then(|l| l.as_str()) {
            Some("err") => 2,
            Some("warn") => 1,
            _ => 0,
        };
        Some((kind, msg))
    })
}

/// 把 `rc` 声明应用到连接设置（插件决定该连哪里，宿主只负责执行）。
async fn apply_rc(
    ctx: &Arc<Ctx>,
    ui: &slint::Weak<crate::MainWindow>,
    body: &Value,
) -> Result<()> {
    let Some(rc) = body.get("rc") else { return Ok(()) };
    let url = rc.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if url.is_empty() {
        return Ok(());
    }
    let user = rc.get("user").and_then(|v| v.as_str()).unwrap_or("");
    let pass = rc.get("pass").and_then(|v| v.as_str()).unwrap_or("");
    {
        let mut cfg = ctx.cfg.lock().unwrap();
        cfg.rclone.url = url.to_string();
        cfg.rclone.user = user.to_string();
        cfg.rclone.pass = pass.to_string();
        let _ = config::save(&cfg);
    }
    if rc.get("connect").and_then(|v| v.as_bool()).unwrap_or(false) {
        // 插件把 rclone 拉起来之后由它给出 rc 端点，宿主负责连上——并且**必须**顺手
        // 恢复档案里记住的挂载/服务：冷启动时 rclone 还没起来，启动流程里的
        // `if ready` 分支被跳过（那时 `ready == false`，走的是「让插件去启动」），
        // 于是 replay 再也没人调用，用户看到的是「一键 rclone 启动成功，但挂载/服务
        // 列表是空的」（会话 16 宿主实测）。
        let out = crate::connect_report(ctx).await;
        let live = out.view.live;
        crate::apply_conn_view(ui, out.view);
        if live {
            crate::spawn_restore(ctx, ui);
        } else {
            // 插件刚 spawn 出来的 rclone 可能还没 bind 上端口（TCG 客机实测能慢
            // 20 秒）：交给后台按间隔重试，连上后自动补挂载/服务恢复。
            crate::spawn_connect_retry(ctx, ui);
        }
    }
    Ok(())
}

// ---------------- 面板合成 ----------------

fn control_enabled(body: &Value, key: &str, default: bool) -> bool {
    body.get("enabled")
        .and_then(|e| e.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

fn hint_text(body: &Value, key: &str, fallback: &str) -> String {
    body.get("hints")
        .and_then(|h| h.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or(fallback)
        .to_string()
}

/// 某个字段的下拉候选（插件通过 `body.choices.<key>` 提供）。
///
/// 目前只有 path 字段用它（历史路径）；返回空列表时前端不显示 ▾。
fn choices_for(body: &Value, key: &str) -> Vec<String> {
    body.get("choices")
        .and_then(|c| c.get(key))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}


/// 合成面板行：声明式控件 + 插件运行时状态 → 一维列表。
fn compose(setting: &PluginSetting, body: &Value, form: &Map<String, Value>, id: &str) -> Vec<PanelData> {
    let _ = id;
    let mut out: Vec<PanelData> = Vec::new();
    let mut pending: Vec<BtnData> = Vec::new();
    let mut pending_line = String::new();

    fn flush(out: &mut Vec<PanelData>, pending: &mut Vec<BtnData>, line: &mut String) {
        if pending.is_empty() {
            return;
        }
        out.push(PanelData {
            kind: "actions".into(),
            key: line.clone(),
            buttons: std::mem::take(pending),
            enabled: true,
            ..Default::default()
        });
        line.clear();
    }

    for c in &setting.controls {
        let kind = c.kind_enum().unwrap_or(ControlKind::Group);
        match kind {
            ControlKind::Group => {
                flush(&mut out, &mut pending, &mut pending_line);
                let text = hint_text(body, &c.key, &c.label);
                let mut row = simple_row("group", &c.key, "");
                row.label = c.label.clone();
                row.text = text;
                out.push(row);
            }
            ControlKind::Divider => {
                flush(&mut out, &mut pending, &mut pending_line);
                out.push(simple_row("divider", &c.key, ""));
            }
            ControlKind::Status => {
                flush(&mut out, &mut pending, &mut pending_line);
                let lamp = body
                    .get("lamp")
                    .and_then(|v| v.as_str())
                    .unwrap_or(if c.lamp { "warn" } else { "" })
                    .to_string();
                let text = body
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&c.text)
                    .to_string();
                out.push(PanelData {
                    lamp,
                    text,
                    label: c.label.clone(),
                    ..simple_row("status", &c.key, "")
                });
            }
            ControlKind::Hint => {
                flush(&mut out, &mut pending, &mut pending_line);
                let text = hint_text(body, &c.key, &c.text);
                out.push(PanelData {
                    warning: text.starts_with('⚠'),
                    text,
                    ..simple_row("hint", &c.key, "")
                });
            }
            ControlKind::Progress => {
                flush(&mut out, &mut pending, &mut pending_line);
                // 只有插件真的在报进度时才显示这一行。
                // 长任务（下载 / 解压 / 安装）的进度由宿主 caps::ProgressHub 广播到页面
                // 顶部，插件一次调用只跑一轮、拿不到自己内部的中间态，所以这里不做兜底 ——
                // 否则空闲时会留下一条永远 0% 的空进度条。
                if let Some(p) = body.get("progress").and_then(|v| v.as_i64()) {
                    out.push(PanelData {
                        progress: p as i32,
                        label: c.label.clone(),
                        ..simple_row("progress", &c.key, "")
                    });
                }
            }
            ControlKind::Checks => {
                flush(&mut out, &mut pending, &mut pending_line);
                let checks = body
                    .get("checks")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if checks.is_empty() {
                    let text = if c.text.is_empty() {
                        "尚未校验".to_string()
                    } else {
                        c.text.clone()
                    };
                    out.push(PanelData {
                        warning: true,
                        text,
                        ..simple_row("hint", &c.key, "")
                    });
                } else {
                    for ch in checks {
                        out.push(PanelData {
                            ok: ch.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
                            label: ch
                                .get("label")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            value: ch
                                .get("value")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            ..simple_row("check", &c.key, "")
                        });
                    }
                }
            }
            ControlKind::Field => {
                flush(&mut out, &mut pending, &mut pending_line);
                let value = form
                    .get(&c.key)
                    .map(value_as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| c.default.clone());
                let sel = c
                    .options
                    .iter()
                    .position(|o| o == &value)
                    .map(|i| i as i32)
                    .unwrap_or(0);
                // 路径字段的候选来自插件运行时（`choices.<key>`，如历史路径）。
                // 静态 `options` 只对 select 有意义，这里不能混用：select 用
                // `selected_index` 定位，path 只是给个下拉候选。
                let field_kind = FieldKind::as_str(c.field_enum().unwrap_or(FieldKind::Text));
                let options = if field_kind == "path" {
                    choices_for(body, &c.key)
                } else {
                    c.options.clone()
                };
                out.push(PanelData {
                    kind: "field".into(),
                    key: c.key.clone(),
                    label: c.label.clone(),
                    value,
                    field_kind: field_kind.into(),
                    placeholder: c.placeholder.clone(),
                    options,
                    selected_index: sel,
                    enabled: control_enabled(body, &c.key, c.enabled),
                    ..Default::default()
                });
            }
            ControlKind::Action => {
                let line = c.line.clone();
                if !line.is_empty() {
                    if !pending_line.is_empty() && line != pending_line {
                        flush(&mut out, &mut pending, &mut pending_line);
                    }
                    pending_line = line;
                } else {
                    flush(&mut out, &mut pending, &mut pending_line);
                }
                pending.push(BtnData {
                    label: c.label.clone(),
                    action: c.key.clone(),
                    style: plugin_host::config::ButtonStyle::as_str(c.style).into(),
                    enabled: control_enabled(body, &c.key, c.enabled),
                });
            }
        }
    }
    flush(&mut out, &mut pending, &mut pending_line);
    out
}

fn simple_row(kind: &str, key: &str, text: &str) -> PanelData {
    PanelData {
        kind: kind.into(),
        key: key.into(),
        text: text.into(),
        enabled: true,
        ..Default::default()
    }
}


/// 拉取并合成某个插件的面板（`action` 默认 "state"）。
pub async fn build_panel(ctx: &Arc<Ctx>, id: &str) -> Result<Vec<PanelData>> {
    let Some((_dir, cfg)) = loaded_config(id) else {
        bail!("找不到插件 {id} 的 config.json");
    };
    if cfg.plugin_setting.controls.is_empty() {
        return Ok(Vec::new());
    }
    let body = match call_plugin(ctx, id, "state", Value::Null).await {
        Ok(b) => {
            apply_body(ctx, id, &b);
            b
        }
        // 取状态失败不该让面板整个消失：仍然渲染声明式控件，只是没有运行时状态
        Err(e) => json!({ "status": format!("{e:#}"), "lamp": "err" }),
    };
    // 与传给插件的同一份有效表单：界面显示的值必须等于插件实际拿到的值
    let form = effective_form(&cfg.plugin_setting, &ctx.store, id);
    Ok(compose(&cfg.plugin_setting, &body, &form, id))
}

/// 执行面板上的一个动作按钮。
pub async fn run_action(
    ctx: &Arc<Ctx>,
    id: &str,
    action: &str,
    ui: &slint::Weak<crate::MainWindow>,
) -> Result<()> {
    let enabled = ctx
        .cfg
        .lock()
        .map(|c| c.plugin_enabled.get(id).copied().unwrap_or(false))
        .unwrap_or(false);
    if !enabled {
        bail!("插件 {id} 已停用，请先启用再操作");
    }
    let body = call_plugin(ctx, id, action, Value::Null).await?;
    if let Some((kind, msg)) = apply_body(ctx, id, &body) {
        crate::toast(ui, kind, msg);
    }
    apply_rc(ctx, ui, &body).await?;
    Ok(())
}

/// 触发插件自启动（宿主只负责按插件的回复去连接 rclone）。
pub async fn autostart(ctx: &Arc<Ctx>, ui: &slint::Weak<crate::MainWindow>) {
    let enabled = ctx.cfg.lock().map(|c| c.plugin_enabled.clone()).unwrap_or_default();
    let rows = scan_rows(ctx).await;
    for r in rows {
        if !r.has_panel || !r.verified {
            continue;
        }
        if !enabled.get(&r.id).copied().unwrap_or(false) {
            continue;
        }
        match call_plugin(ctx, &r.id, "autostart", Value::Null).await {
            Ok(body) => {
                if let Some((kind, msg)) = apply_body(ctx, &r.id, &body) {
                    crate::toast(ui, kind, msg);
                }
                if let Err(e) = apply_rc(ctx, ui, &body).await {
                    eprintln!("自动连接失败: {e:#}");
                }
            }
            Err(e) => eprintln!("插件 {} 自启动失败: {e:#}", r.id),
        }
    }
}

/// 卸载前做一点善后：让插件自己收尾（停进程等），失败不影响删除。
pub async fn shutdown_plugin(ctx: &Arc<Ctx>, id: &str) {
    let Some((_dir, cfg)) = loaded_config(id) else {
        return;
    };
    if !cfg.entry_is_wasm() {
        return;
    }
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        call_plugin(ctx, id, "shutdown", Value::Null),
    )
    .await;
    // 兜底：宿主自己登记过的子进程一律停掉
    caps::stop_plugin_procs(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str, tags: &[&str], desc: &str) -> Row {
        Row {
            id: id.into(),
            name: name.into(),
            version: "0.1.0".into(),
            desc: desc.into(),
            loaded: true,
            enabled: true,
            kind: "package".into(),
            author: String::new(),
            entry: "main.wasm".into(),
            verified: true,
            builtin: false,
            has_panel: true,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            lamp: String::new(),
            status: String::new(),
            path: String::new(),
        }
    }

    /// 检索必须能命中标签——这是用户要求「用标签而不是类型」的目的：
    /// 插件商店里按下标签就能精准找到插件，而不是按载体/类型分类。
    #[test]
    fn search_matches_tags() {
        let rows = vec![
            row("onerclone", "一键 rclone", &["下载", "进程"], "启动本机 rclone"),
            row("imagehost", "图床", &["图床", "上传"], "基于 rclone 的图床"),
        ];
        // 只出现在 tags 里、不在这两个插件的名称/id/描述里的词
        let hit = filter_rows(&rows, "进程");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].id, "onerclone");

        let hit = filter_rows(&rows, "上传");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].id, "imagehost");
    }

    #[test]
    fn search_is_case_insensitive_and_trims() {
        let rows = vec![row("onerclone", "一键 rclone", &["RCD"], "启动本机 rclone")];
        assert_eq!(filter_rows(&rows, "  RcD ").len(), 1);
        assert_eq!(filter_rows(&rows, "rclone").len(), 1);
        // 空检索词 = 不过滤
        assert_eq!(filter_rows(&rows, "   ").len(), 1);
        // 不匹配任何字段
        assert_eq!(filter_rows(&rows, "nothing-here").len(), 0);
    }

    /// 内置插件必须排在最前（列表排序是 (is_builtin, name) 的逆序 cmp）。
    #[test]
    fn builtin_rows_sort_first() {
        let mut rows = vec![
            row("zzz", "AAA 用户插件", &[], ""),
            row("onerclone", "一键 rclone", &[], ""),
        ];
        rows[1].builtin = true;
        rows.sort_by(|a, b| (!a.builtin, &a.name).cmp(&(!b.builtin, &b.name)));
        assert_eq!(rows[0].id, "onerclone");
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rcgui-plugins-test-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 单文件插件（imagehost 就是这样装着的）卸载时必须删掉 `.wasm` 本体，
    /// 而不是只删掉同名的 kv 目录 —— 那会变成「界面上没了、磁盘上还在」。
    #[test]
    fn uninstall_removes_single_file_plugin() {
        let root = tmp_dir("wasm");
        let wasm = root.join("imagehost.wasm");
        std::fs::write(&wasm, b"\0asm").unwrap();
        let kv_dir = root.join("imagehost");
        std::fs::create_dir_all(&kv_dir).unwrap();
        std::fs::write(kv_dir.join("kv.json"), b"{}").unwrap();

        let removed = remove_plugin_files(&wasm, &root, "imagehost").unwrap();

        assert_eq!(removed, wasm);
        assert!(!wasm.exists(), "wasm 本体必须被删");
        assert!(!kv_dir.exists(), "kv 目录也必须被删");
        std::fs::remove_dir_all(&root).ok();
    }

    /// 目录型插件走 bundle::uninstall，整个包目录连同里面的 kv.json 一起消失。
    #[test]
    fn uninstall_removes_package_dir() {
        let root = tmp_dir("pkg");
        let dir = root.join("onerclone");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), b"{}").unwrap();
        std::fs::write(dir.join("kv.json"), b"{}").unwrap();

        let removed = remove_plugin_files(&dir, &root, "onerclone").unwrap();

        assert_eq!(removed, dir);
        assert!(!dir.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// `imagehost` 是 include_bytes! 编进 exe 的，不满足 `builtin_ids()`
    /// （它不在 assets/plugins 里）。`ships_with_app` 必须把它算进来，
    /// 否则卸载后不写墓碑，下次启动 `embed_plugins` 又把 wasm 写回来。
    #[test]
    fn imagehost_counts_as_ships_with_app() {
        assert!(ships_with_app("imagehost"));
    }

    /// 声明式默认值必须真的进到传给插件的 form 里。
    ///
    /// 这是「界面上用户名 gui 显示得好好的、插件却收到空值」那个 bug 的根因：
    /// 插件据此启动了**无鉴权**的 rclone，除 core/version 外全部接口 403，
    /// 5 项自动恢复（挂载 T:/Z:/E:、webdav 8080/8081）全失败。
    #[test]
    fn effective_form_merges_declared_defaults() {
        let dir = tmp_dir("form");
        let store = KvStore::new(dir.clone());
        let setting: PluginSetting = serde_json::from_value(json!({
            "controls": [
                {"kind": "field", "key": "rc_user", "label": "用户名", "field": "text", "default": "gui"},
                {"kind": "field", "key": "port", "label": "端口", "field": "number", "default": "5572"},
                {"kind": "field", "key": "no_auth", "label": "本地不校验", "field": "bool", "default": "false"},
                {"kind": "field", "key": "exe", "label": "程序路径", "field": "path"},
                {"kind": "status", "key": "conn", "label": "运行状态", "lamp": true}
            ]
        }))
        .unwrap();

        // ① 什么都没存过：拿到全部默认值，且默认值绝不落盘
        let f = effective_form(&setting, &store, "onerclone");
        assert_eq!(f.get("rc_user").unwrap(), "gui");
        assert_eq!(f.get("port").unwrap(), "5572");
        assert_eq!(f.get("no_auth").unwrap(), "false");
        assert!(!f.contains_key("exe"), "没有默认值的字段不该凭空造键");
        assert!(form_of(&store, "onerclone").is_empty(), "默认值不能写回磁盘");

        // ② 用户填过：用户值优先，其余字段仍走默认值
        store
            .set(
                "onerclone",
                "form",
                json!({"rc_user": "alice", "exe": "C:\\rclone.exe"}),
            )
            .unwrap();
        let f = effective_form(&setting, &store, "onerclone");
        assert_eq!(f.get("rc_user").unwrap(), "alice");
        assert_eq!(f.get("exe").unwrap(), "C:\\rclone.exe");
        assert_eq!(f.get("port").unwrap(), "5572");

        // ③ 空串按「未填」处理，与面板渲染的兜底语义一致（显示什么就传什么）
        store
            .set("onerclone", "form", json!({"rc_user": ""}))
            .unwrap();
        let f = effective_form(&setting, &store, "onerclone");
        assert_eq!(f.get("rc_user").unwrap(), "gui");

        // ④ 控件没声明、form 里已有的键原样带出去（插件 values 回写、自定义字段）
        store.set("onerclone", "form", json!({"extra": 7})).unwrap();
        let f = effective_form(&setting, &store, "onerclone");
        assert_eq!(f.get("extra").unwrap(), &json!(7));

        std::fs::remove_dir_all(&dir).ok();
    }
}
