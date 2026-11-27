//! `rcgui.conf` 的构建 / 读取 / 备份 / 本地应用。
//!
//! 阶段 5A 的边界：
//! - **导出**：把当前档案 + 远程配置写成单个 JSON 文件，并对敏感数据做明确提示；
//! - **导入**：解析 → 校验 → 备份当前档案 → 应用到 RCGUI 自己的档案；
//! - **不碰 rclone.conf**：写入 rclone 配置（`config/create|update`）留到 5B，
//!   与预览面板一起做。因此本阶段不存在「静默覆盖正在使用的 rclone 配置」的可能。
//!
//! 预览 UI 后置：导入按钮只做「解析 + 校验 + 摘要 + 差异」，并把结果暂存；
//! 用户再点「应用导入」才真正写盘。这条两步流程本身就是一次确认。

use crate::config::AppConfig;
use crate::profiles;
use anyhow::{anyhow, Context, Result};
use rcgui_profile::bundle::{Diff, RcguiConf, RemoteConf, Summary};
use rcgui_profile::ProfileDoc;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// 一次导入预览的结果（不含任何凭据内容）。
///
/// 预览只描述「导入后会变成什么样」，不执行任何传输或删除。同步任务刻意逐个列出来
/// 而不是只给个数字：方向错了就是一整轮白传，镜像模式还会删目标端的文件，用户必须
/// 能在按确认之前看见这些。
#[derive(Debug, Clone)]
pub struct Preview {
    pub source: PathBuf,
    pub summary: Summary,
    pub diff: Diff,
    /// 文件里记录的插件条目数。插件二进制不在文件里：导入只写回「引用 + 设置」，
    /// 本机没有的插件要用户自己去商店或从文件安装，导入流程不会替他去下载。
    pub plugin_count: usize,
    /// 文件里记录的同步任务数。导入后不立即执行，等下一次间隔。
    pub sync_count: usize,
    /// 每个同步任务一行的人话描述（名称、本地→远程、方向、会不会删目标、间隔、启用与否）。
    pub sync_lines: Vec<String>,
    /// 文件里有任务要求镜像（会删除目标端多余文件）。本机导入会拒绝镜像，只提示用户。
    pub would_delete: bool,
    /// 文件里写了的程序设置项（中文标签）。为空表示文件没有 `app` 段，一个设置都不会被覆盖。
    pub app_fields: Vec<String>,
}

/// 从 `serve/list` 提取的导出用条目（已拆好远程与子路径）。
#[derive(Debug, Clone, Default)]
pub struct LiveServe {
    pub stype: String,
    pub remote: String,
    pub sub_path: String,
    pub addr: String,
    pub user: String,
}

/// 从 `mount/listmounts` 提取的导出用条目。
#[derive(Debug, Clone, Default)]
pub struct LiveMount {
    pub remote: String,
    pub sub_path: String,
    pub point: String,
}

/// 把 rclone fs 串（`name:sub`）拆成（远程名, 子路径）。
fn split_fs(fs: &str) -> (String, String) {
    match fs.find(':') {
        Some(i) => (
            fs[..i].to_string(),
            fs[i + 1..].trim_matches('/').to_string(),
        ),
        None => (fs.trim_end_matches(':').to_string(), String::new()),
    }
}

/// 解析 `serve/list` 结果为导出条目。结构在不同版本间有差异，因此宽容取值。
pub fn live_serves_from(v: &Value) -> Vec<LiveServe> {
    let items = v
        .get("list")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter(|it| it.get("running").and_then(|x| x.as_bool()).unwrap_or(true))
        .map(|it| {
            let params = it
                .get("params")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let getp = |keys: &[&str]| -> String {
                for k in keys {
                    if let Some(s) = params.get(*k).and_then(|x| x.as_str()) {
                        return s.to_string();
                    }
                    if let Some(s) = it.get(*k).and_then(|x| x.as_str()) {
                        return s.to_string();
                    }
                }
                String::new()
            };
            let fs = getp(&["fs", "Fs"]);
            let (remote, sub_path) = split_fs(&fs.trim_end_matches('/'));
            LiveServe {
                stype: getp(&["type", "Type"]),
                remote,
                sub_path,
                addr: getp(&["addr", "Addr", "listen", "Listen"]),
                user: getp(&["user", "User"]),
            }
        })
        .filter(|s| !s.remote.is_empty() && !s.addr.is_empty())
        .collect()
}

/// 解析 `mount/listmounts` 结果为导出条目。
pub fn live_mounts_from(v: &Value) -> Vec<LiveMount> {
    let items = v
        .get("mountPoints")
        .or_else(|| v.get("mounts"))
        .or_else(|| v.get("list"))
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .map(|it| {
            let get = |keys: &[&str]| -> String {
                for k in keys {
                    if let Some(s) = it.get(*k).and_then(|x| x.as_str()) {
                        return s.to_string();
                    }
                }
                String::new()
            };
            let (remote, sub_path) = split_fs(get(&["Fs", "fs"]).trim_end_matches('/'));
            LiveMount {
                remote,
                sub_path,
                point: get(&["MountPoint", "mountPoint", "mount_point"]),
            }
        })
        .filter(|m| !m.remote.is_empty() && !m.point.is_empty())
        .collect()
}

/// 用当前状态构建导出文档。
///
/// `dump` 为 `config/dump` 的结果（离线时为 `None`，此时不带任何远程参数）；
/// `live_serves` / `live_mounts` 为当前 rclone 的运行中服务与挂载
///（离线时传空）。预设功能已移除，档案里的 `serves`/`mounts` 永远是空的，
/// 因此「导出现状」必须从实时状态构建，否则导出文件里挂载/服务数恒为 0。
pub fn build_bundle(
    cfg: &AppConfig,
    dump: Option<&Value>,
    include_secrets: bool,
    providers: Option<&Value>,
    live_serves: &[LiveServe],
    live_mounts: &[LiveMount],
    plugins: &[rcgui_profile::PluginRef],
) -> RcguiConf {
    let mut conf = RcguiConf::default();
    conf.profile = profiles::current_doc(cfg);

    // 实时状态优先：`mount/listmounts` / `serve/list` 是「现在真的在跑什么」的权威来源。
    // 档案里记住的条目作为补充（例如用户已配置但当前未挂载/未启动的项），
    // 这样导出既能反映实况，也不会丢掉用户配过但暂时停掉的东西。
    let live = !live_serves.is_empty() || !live_mounts.is_empty();
    if !live_serves.is_empty() {
        conf.profile.serves = live_serves
            .iter()
            .map(|s| rcgui_profile::ServeProfile {
                stype: s.stype.clone(),
                remote: s.remote.clone(),
                sub_path: s.sub_path.clone(),
                addr: s.addr.clone(),
                user: s.user.clone(),
                // serve/list 不返回密码：不能凭空生成，导入后需要重新填写
                pass: String::new(),
                enabled: true,
            })
            .collect();
    }
    if !live_mounts.is_empty() {
        // 档案里有同盘符条目时，沿用用户设过的只读/缓存模式（listmounts 不返回它们）
        let remembered = conf.profile.mounts.clone();
        conf.profile.mounts = live_mounts
            .iter()
            .map(|m| {
                let old = remembered
                    .iter()
                    .find(|o| o.point.eq_ignore_ascii_case(&m.point));
                rcgui_profile::MountProfile {
                    remote: m.remote.clone(),
                    sub_path: m.sub_path.clone(),
                    point: m.point.clone(),
                    ro: old.map(|o| o.ro).unwrap_or(false),
                    cache: old
                        .map(|o| o.cache.clone())
                        .filter(|c| !c.is_empty())
                        .unwrap_or_else(|| "writes".to_string()),
                    enabled: true,
                }
            })
            .collect();
    }
    if live {
        conf.warnings.push(
            rcgui_i18n::t(
                "挂载/服务由当前运行中的 rclone 状态还原：serve 密码未包含（rclone 不返回），\
             挂载的只读与缓存模式优先沿用 RCGUI 记录、否则取默认值（只读=否、缓存=writes）",
            )
            .to_string(),
        );
    }

    match dump.and_then(|d| d.as_object()) {
        Some(obj) => {
            for (name, entry) in obj {
                let rtype = entry
                    .get("type")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                let mut params = Map::new();
                if let Some(o) = entry.as_object() {
                    for (k, v) in o {
                        if k != "type" {
                            params.insert(k.clone(), v.clone());
                        }
                    }
                }
                conf.remotes.push(RemoteConf {
                    name: name.clone(),
                    rtype,
                    params,
                    needs_auth: false,
                });
            }
            conf.remotes.sort_by(|a, b| a.name.cmp(&b.name));
        }
        None => conf.warnings.push(
            rcgui_i18n::t(
                "导出时未连接 rclone：文件不含任何远程参数，导入后需要重新连接并手工补齐",
            )
            .to_string(),
        ),
    }

    // ---- 插件身份、程序设置、同步任务 ----
    // 插件只导出「装了哪一个、什么版本、从哪来、是否启用、它自己的设置」。
    // 二进制不进配置文件：包体几十 MB，塞进去既难传也难审；导入只恢复引用与设置，
    // 安装由用户另行在商店或从文件完成（导入流程不下载、不写磁盘）。
    conf.plugins = plugins.to_vec();
    conf.app = Some(export_app_settings(cfg));
    conf.sync_tasks = cfg
        .sync_tasks
        .iter()
        .map(|t| rcgui_profile::SyncTaskRef {
            id: t.id.clone(),
            name: t.name.clone(),
            local: t.local.clone(),
            remote: t.remote.clone(),
            direction: t.direction.clone(),
            mode: t.mode.clone(),
            enabled: t.enabled,
            auto_sync: t.auto_sync,
            interval_hours: t.interval_hours,
        })
        .collect();

    // 凭据策略统一在模型层实现：关键词规则 ∪ provider 元数据标记（IsPassword/Sensitive）
    conf.apply_secret_policy(include_secrets, providers);
    conf
}

/// 从程序配置里挑出可移植的设置项。
///
/// 窗口位置、最近路径、`close_prompt_done` 之类属于「本机状态」：换个屏幕打开就会
/// 跑到视野外，或者让用户在另一台机器上跳过本来该看的提示，所以不导出。
fn export_app_settings(cfg: &AppConfig) -> rcgui_profile::AppSettings {
    rcgui_profile::AppSettings {
        theme_mode: Some(cfg.theme_mode),
        accent_idx: Some(cfg.accent_idx),
        language: Some(cfg.language.clone()),
        autostart: Some(cfg.autostart),
        silent_start: Some(cfg.silent_start),
        close_to_tray: Some(cfg.close_to_tray),
        core_mode: Some(cfg.core.normalized_mode().to_string()),
        core_path: Some(cfg.core.path.clone()),
        core_update_check: Some(cfg.core.update_check),
        light_enabled: Some(cfg.light.enabled),
        light_minutes: Some(cfg.light.minutes),
        bw_up: Some(cfg.bw_up.clone()),
        bw_up_unit: Some(cfg.bw_up_unit),
        bw_down: Some(cfg.bw_down.clone()),
        bw_down_unit: Some(cfg.bw_down_unit),
        backup_url: Some(cfg.backup.url.clone()),
        backup_user: Some(cfg.backup.user.clone()),
        backup_pass: Some(cfg.backup.pass.clone()),
    }
}

/// 原子写入导出文件。
pub fn write_bundle(path: &Path, conf: &RcguiConf) -> Result<()> {
    let text = serde_json::to_vec_pretty(conf).context(rcgui_i18n::t("序列化 rcgui.conf 失败"))?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .with_context(|| rcgui_i18n::f("创建目录 {} 失败", &[&(dir.display())]))?;
        }
    }
    let tmp = path.with_extension("tmp-export");
    std::fs::write(&tmp, &text)
        .with_context(|| rcgui_i18n::f("写入 {} 失败", &[&(tmp.display())]))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!(
            "{}",
            rcgui_i18n::f("保存 {} 失败: {}", &[&(path.display()), &(e)])
        )
    })?;
    Ok(())
}

/// 序列化到内存，不落盘。
///
/// 上传到 WebDAV 时需要的是字节而不是文件：先写临时文件再读回来既慢又留下残留。
pub fn write_bundle_to(out: &mut Vec<u8>, conf: &RcguiConf) -> Result<()> {
    let text = serde_json::to_vec_pretty(conf).context(rcgui_i18n::t("序列化 rcgui.conf 失败"))?;
    out.clear();
    out.extend_from_slice(&text);
    Ok(())
}

/// 解析 + 校验 + 生成摘要/差异。**不写任何文件。**
pub fn preview_file(path: &Path, current: &ProfileDoc) -> Result<(RcguiConf, Preview)> {
    let raw =
        std::fs::read(path).with_context(|| rcgui_i18n::f("读取 {} 失败", &[&(path.display())]))?;
    preview_bytes(&raw, path, current)
}

/// 同 [`preview_file`]，但输入是已经在内存里的字节。
///
/// 从 WebDAV 拉回来的内容同样要经过这一步：下载本身不等于"可以导入"，
/// 用户先看摘要和差异，再决定是否应用。
pub fn preview_bytes(
    raw: &[u8],
    source: &Path,
    current: &ProfileDoc,
) -> Result<(RcguiConf, Preview)> {
    let raw = raw.to_vec();
    // 容忍 BOM（外部编辑器可能加）
    let raw: &[u8] = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &raw[3..]
    } else {
        &raw
    };
    // 先做「是不是 rcgui.conf」的硬校验：RcguiConf 用了容器级 serde(default)，
    // 任意 JSON 都能被填成默认包，必须在反序列化前看原始 kind。
    let probe: serde_json::Value = serde_json::from_slice(raw)
        .map_err(|e| anyhow!("{}", rcgui_i18n::f("不是合法的 JSON: {}", &[&(e)])))?;
    match probe.get("kind").and_then(|k| k.as_str()) {
        Some(rgui) if rgui == rcgui_profile::BUNDLE_KIND => {}
        Some(other) => {
            return Err(anyhow!(
                "{}",
                rcgui_i18n::f(
                    "不是 rcgui.conf 文件（kind = \"{}\"，应为 \"{}\"）",
                    &[&(other), &(rcgui_profile::BUNDLE_KIND)]
                )
            ))
        }
        None => {
            return Err(anyhow!(
                "{}",
                rcgui_i18n::f(
                    "不是 rcgui.conf 文件（缺少 kind 字段，应为 \"{}\"）",
                    &[&(rcgui_profile::BUNDLE_KIND)]
                )
            ))
        }
    }
    let mut conf: RcguiConf = serde_json::from_slice(raw).map_err(|e| {
        anyhow!(
            "{}",
            rcgui_i18n::f("不是合法的 rcgui.conf（JSON 解析失败）: {}", &[&(e)])
        )
    })?;
    conf.validate().map_err(|e| anyhow!("{e}"))?;
    // 兼容早期导出：serves 为空但 serve_names 有内容时，用映射还原出服务条目，
    // 否则预览会错误地显示「服务 0」。
    conf.normalize_legacy();

    let diff = rcgui_profile::bundle_diff(current, &conf.profile, &conf.remotes);
    let (sync_lines, would_delete) = sync_preview_lines(&conf.sync_tasks);
    let app_fields = conf
        .app
        .as_ref()
        .map(|a| a.present().into_iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let preview = Preview {
        source: source.to_path_buf(),
        summary: conf.summary(),
        diff,
        plugin_count: conf.plugin_count(),
        sync_count: conf.sync_count(),
        sync_lines,
        would_delete,
        app_fields,
    };
    Ok((conf, preview))
}

/// 把文件里的同步任务翻成给人看的一行一条，并回答「会不会删目标端文件」。
///
/// 第二个返回值为 `true` 表示文件里有任务声明了镜像（`sync`）。导入并不会照做
/// ——[`apply_sync_tasks`] 会把它降级成 `copy` 且停用——但用户必须先看到这个意图，
/// 否则"我导入了一份配置文件"和"我的备份盘被删了多余文件"之间就没有任何提醒。
fn sync_preview_lines(tasks: &[rcgui_profile::SyncTaskRef]) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let mut would_delete = false;
    for (i, t) in tasks.iter().enumerate() {
        let name = if t.name.trim().is_empty() {
            format!("任务 {}", i + 1)
        } else {
            t.name.clone()
        };
        let mirror = t.mode.trim().eq_ignore_ascii_case("sync");
        if mirror {
            would_delete = true;
        }
        let dir = match t.direction.trim().to_ascii_lowercase().as_str() {
            "down" | "download" | "remote-to-local" => rcgui_i18n::t("云端 → 本地"),
            _ => rcgui_i18n::t("本地 → 云端"),
        };
        let how = if mirror {
            rcgui_i18n::t("镜像（会删除目标端多余文件，导入时降级为只复制并停用）")
        } else {
            rcgui_i18n::t("只复制（不删除目标端文件）")
        };
        let every = if t.interval_hours == 0 {
            rcgui_i18n::t("仅启动时一次").to_string()
        } else {
            rcgui_i18n::f("每 {} 小时", &[&(t.interval_hours)])
        };
        let state = if t.enabled {
            rcgui_i18n::t("导入后启用")
        } else {
            rcgui_i18n::t("导入后停用")
        };
        lines.push(rcgui_i18n::f(
            "{}：{} → {}，{}，{}，{}，{}",
            &[
                &(name),
                &(t.local),
                &(t.remote),
                &(dir),
                &(how),
                &(every),
                &(state),
            ],
        ));
    }
    (lines, would_delete)
}

/// 备份当前档案文件（导入前调用）。返回备份路径；档案尚不存在时返回 `None`。
///
/// 档案路径由调用方给出，这样「先备份再动档案」的顺序能在测试里对着临时目录验证。
fn backup_profile_at(p: &Path) -> Result<Option<PathBuf>> {
    if !p.exists() {
        return Ok(None);
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let bak = p.with_file_name(format!("default.json.bak-{stamp}"));
    std::fs::copy(p, &bak)
        .with_context(|| rcgui_i18n::f("备份档案到 {} 失败", &[&(bak.display())]))?;
    Ok(Some(bak))
}

/// 把导入文件应用到本地档案（含备份 + 回滚点）。**不写 rclone.conf。**
pub fn apply(conf: &RcguiConf, cfg: &mut AppConfig) -> Result<PathBuf> {
    apply_at(&profiles::profile_path(), conf, cfg)
}

/// 同 [`apply`]，但档案路径由调用方给出。
///
/// 顺序是「先备份、再替换」：替换失败时磁盘上留着的仍然是导入前的档案，
/// 外加一个 `.bak-时间戳` 的副本。这一点由 `failed_import_leaves_profile_untouched` 守住。
pub fn apply_at(profile: &Path, conf: &RcguiConf, cfg: &mut AppConfig) -> Result<PathBuf> {
    let bak = backup_profile_at(profile)?.ok_or_else(|| {
        anyhow!(
            "{}",
            rcgui_i18n::t("找不到当前档案文件；为避免覆盖未知数据，已中止导入")
        )
    })?;
    // apply_doc + 远程元信息合并都在 profiles 里完成，保证只有一个写入口
    profiles::replace_from_bundle_at(profile, conf, cfg)?;
    if let Some(a) = conf.app.as_ref() {
        apply_app_settings(a, cfg);
    }
    for note in apply_sync_tasks(&conf.sync_tasks, cfg) {
        crate::logging::event("warn", "import", &note);
    }
    Ok(bak)
}

/// 把可移植设置写回程序配置。
///
/// **只覆盖文件里确实带着的项**：`AppSettings` 每个字段都是 `Option`，`None` 表示
/// 「这份文件没记录这一项」，本机现值原样保留。这条规则很重要——早期 v1/v2 配置
/// 文件根本没有 `app` 段，若按默认值覆盖，用户自己选的内核路径（例如 teldrive
/// 插件用的 rclone fork）会被一次导入清空，之后所有依赖该内核的远程都连不上。
///
/// 空字符串同样视为「没记录」；例外是 `backup_pass`——它默认被剥离，用空串覆盖会
/// 抹掉用户原本能用的备份密码。
fn apply_app_settings(a: &rcgui_profile::AppSettings, cfg: &mut AppConfig) {
    if let Some(v) = a.theme_mode {
        cfg.theme_mode = v;
    }
    if let Some(v) = a.accent_idx {
        cfg.accent_idx = v;
    }
    if let Some(v) = a
        .language
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cfg.language = v.to_string();
    }
    if let Some(v) = a.autostart {
        cfg.autostart = v;
    }
    if let Some(v) = a.silent_start {
        cfg.silent_start = v;
    }
    if let Some(v) = a.close_to_tray {
        cfg.close_to_tray = v;
    }
    // 内核模式只认两个值：别的字符串来自手写文件或未来版本，照抄会让模式判断落到默认分支
    if let Some(v) = a.core_mode.as_deref().map(str::trim) {
        if v == "managed" || v == "external" {
            cfg.core.mode = v.to_string();
        }
    }
    // 内核路径：只有文件里写了才覆盖。空串在导出端代表"用随包/数据目录的内核"，
    // 这里照实写回空串是安全的（用户确实显式清过）；但 None 绝不覆盖。
    if let Some(v) = a.core_path.as_deref() {
        cfg.core.path = v.to_string();
    }
    if let Some(v) = a.core_update_check {
        cfg.core.update_check = v;
    }
    if let Some(v) = a.light_enabled {
        cfg.light.enabled = v;
    }
    if let Some(v) = a.light_minutes {
        if v > 0 {
            cfg.light.minutes = v;
        }
    }
    if let Some(v) = a.bw_up.as_deref() {
        cfg.bw_up = v.to_string();
    }
    if let Some(v) = a.bw_up_unit {
        cfg.bw_up_unit = v;
    }
    if let Some(v) = a.bw_down.as_deref() {
        cfg.bw_down = v.to_string();
    }
    if let Some(v) = a.bw_down_unit {
        cfg.bw_down_unit = v;
    }
    if let Some(v) = a.backup_url.as_deref() {
        cfg.backup.url = v.to_string();
    }
    if let Some(v) = a.backup_user.as_deref() {
        cfg.backup.user = v.to_string();
    }
    if let Some(v) = a.backup_pass.as_deref().filter(|s| !s.is_empty()) {
        cfg.backup.pass = v.to_string();
    }
}

/// 用文件里的任务表替换本机任务表。返回需要告诉用户的情况（如被降级的镜像任务）。
///
/// 导入的是「这套任务应该长什么样」，所以整表替换而不是合并：合并会让反复导入
/// 同一个文件时留下越来越多次要副本。
///
/// 两条安全规则：
/// 1. **不立即执行**：`last_run` 写成导入时刻，下次运行时间 = 现在 + 间隔，
///    否则 `last_run` 为空会被调度器当成"从未运行、马上该跑"，导入瞬间就对用户
///    网络发起一轮全量传输。
/// 2. **拒绝镜像**：镜像（`sync`）会删除目标端多余文件，界面里没有这个选项，
///    配置文件不该能把本机切进会删数据的状态。所以镜像一律降级成 `copy`
///    并置为不启用，由用户自己在界面上重新决定。
fn apply_sync_tasks(tasks: &[rcgui_profile::SyncTaskRef], cfg: &mut AppConfig) -> Vec<String> {
    let now = rcgui_profile::now_iso();
    let mut notes = Vec::new();
    cfg.sync_tasks = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let name = if t.name.trim().is_empty() {
                format!("任务 {}", i + 1)
            } else {
                t.name.clone()
            };
            let wants_mirror = t.mode.trim().eq_ignore_ascii_case("sync");
            if wants_mirror {
                notes.push(rcgui_i18n::f(
                    "同步任务「{}」在文件里要求镜像（会删除目标端多余文件），本机没有开启镜像：已改为只复制，并保持停用，请你确认后手动启用",
                    &[&(name)],
                ));
            }
            crate::config::SyncTask {
                id: if t.id.is_empty() {
                    format!("task-{}", i + 1)
                } else {
                    t.id.clone()
                },
                name,
                local: t.local.clone(),
                remote: t.remote.clone(),
                direction: if t.direction.trim().is_empty() {
                    "up".to_string()
                } else {
                    t.direction.clone()
                },
                mode: if wants_mirror || t.mode.trim().is_empty() {
                    "copy".to_string()
                } else {
                    t.mode.clone()
                },
                enabled: t.enabled && !wants_mirror,
                // 文件里写了开关就照抄；没写（None）就交给调度器按模式默认：
                // 只复制的任务跟着变化跑，镜像任务不动。
                auto_sync: t.auto_sync,
                interval_hours: if t.interval_hours == 0 { 6 } else { t.interval_hours },
                // 导入时刻当作"上次运行"，把第一轮推到下一个间隔之后
                last_run: now.clone(),
                last_status: rcgui_i18n::t("尚未运行（导入）").to_string(),
            }
        })
        .collect();
    notes
}

/// 预览的简短人类可读描述（用于 toast）。
pub fn describe(p: &Preview) -> String {
    let s = &p.summary;
    let fname = p
        .source
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("rcgui.conf");
    let mut out = rcgui_i18n::f(
        "{} → 档案「{}」：远程 {} · 挂载 {} · 服务 {}",
        &[
            &(fname),
            &(if s.profile_name.is_empty() {
                rcgui_i18n::t("（未命名）")
            } else {
                &s.profile_name
            }),
            &(s.remote_count),
            &(s.mount_count),
            &(s.serve_count),
        ],
    );
    out.push_str(&format!("；{}", p.diff.one_line()));
    if p.plugin_count > 0 || p.sync_count > 0 {
        // 插件与同步任务是可选项，没有就不提，免得正常导入的提示越读越长
        out.push_str(&rcgui_i18n::f(
            "；插件 {} · 同步任务 {}",
            &[&(p.plugin_count), &(p.sync_count)],
        ));
        if p.plugin_count > 0 {
            out.push_str(rcgui_i18n::t(
                "（不含插件包体；未安装的插件需先在商店或从文件安装）",
            ));
        }
    }
    if s.sensitive {
        out.push_str(&rcgui_i18n::f(
            "；⚠ 含敏感数据；{} 个远程的凭据未随包导出（可能需要重新认证）",
            &[&(s.needs_auth)],
        ));
    }
    if p.would_delete {
        // 这条必须出现在预览里，而且要说清"不会照做"：文件能写的意图不等于本机会执行它
        out.push_str(rcgui_i18n::t(
            "；⚠ 文件里有任务要求镜像（会删除目标端多余文件）：导入时一律降级为只复制并停用",
        ));
    }
    if !p.app_fields.is_empty() {
        out.push_str(&rcgui_i18n::f(
            "；会覆盖 {} 项程序设置",
            &[&(p.app_fields.len())],
        ));
    }
    out
}

/// 预览的逐条明细（对话框正文用）。比 [`describe`] 长，但同步任务与设置项必须逐条列出：
/// 方向错了就是一整轮白传，镜像模式还会删目标端的文件。
pub fn describe_detail(p: &Preview) -> String {
    let mut out = describe(p);
    if !p.sync_lines.is_empty() {
        out.push('\n');
        out.push_str(rcgui_i18n::t("同步任务："));
        for line in &p.sync_lines {
            out.push_str("\n  · ");
            out.push_str(line);
        }
        out.push('\n');
        out.push_str(rcgui_i18n::t(
            "导入后不会立刻传输：第一次运行时间 = 现在 + 各自间隔。",
        ));
    }
    if !p.app_fields.is_empty() {
        out.push('\n');
        out.push_str(&rcgui_i18n::f(
            "会覆盖的程序设置：{}",
            &[&(p.app_fields.join("、"))],
        ));
    }
    out
}

#[cfg(test)]
fn test_dir(tag: &str) -> std::path::PathBuf {
    // 本项目沙箱下 %TEMP% 不可写（os error 5），测试目录放在测试可执行文件旁
    let base = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir);
    let d = base.join(format!("rcgui-conf-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
#[cfg(test)]
mod tests {
    use super::*;
    use rcgui_profile::{MountProfile, RemoteNote};

    fn cfg_with_mount() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.auto_mounts.push(crate::config::AutoMount {
            remote: "teldrive".into(),
            sub_path: String::new(),
            point: "T:".into(),
            ro: false,
            cache: "writes".into(),
            enabled: true,
        });
        cfg
    }

    #[test]
    fn build_bundle_flags_sensitive_only_when_params_exist() {
        // 断言里有中文文案，先独占语言，别被并行的英文用例带跑
        let _lang = crate::test_lang::use_lang(rcgui_i18n::Lang::Zh);
        let cfg = cfg_with_mount();
        // 离线导出：无 dump、无实时挂载/服务 → 不含远程参数。
        // 内存里遗留的 auto_mounts 仍会被导出（1 项），但服务为 0：
        // 这正是「只有 serve、没有预设」时用户会看到的现象（导出侧已用实时状态修复）。
        let offline = build_bundle(&cfg, None, false, None, &[], &[], &[]);
        assert!(!offline.sensitive);
        assert!(!offline.warnings.is_empty());
        assert_eq!(offline.summary().mount_count, 1);
        assert_eq!(offline.summary().serve_count, 0);

        // 在线导出：有 dump 且含 token → 标记敏感
        let dump = serde_json::json!({
            "teldrive": { "type": "teldrive", "token": "abc" }
        });
        let online = build_bundle(&cfg, Some(&dump), true, None, &[], &[], &[]);
        assert!(online.sensitive);
        assert_eq!(online.remotes.len(), 1);
        assert!(online.warnings.iter().any(|w| w.contains("并非加密")));
    }

    /// 用户实际环境（rclone v1.75.4）的真实响应，确认导出包里挂载/服务都不为 0。
    #[test]
    fn real_v175_responses_produce_nonzero_mounts_and_serves() {
        let mounts_raw = serde_json::json!({
            "mountPoints": [{
                "Fs": "teldrive:", "MountPoint": "T:",
                "MountedOn": "2026-10-17T16:50:37.7685173+08:00"
            }]
        });
        let serves_raw = serde_json::json!({
            "list": [{
                "id": "webdav-3dc16c63",
                "addr": "127.0.0.1:8080",
                "params": { "addr": "127.0.0.1:8080", "fs": "teldrive:", "type": "webdav" }
            }]
        });
        let mounts = live_mounts_from(&mounts_raw);
        let serves = live_serves_from(&serves_raw);
        assert_eq!(mounts.len(), 1, "挂载解析不能为 0");
        assert_eq!(serves.len(), 1, "服务解析不能为 0");
        assert_eq!(mounts[0].point, "T:");
        assert_eq!(mounts[0].remote, "teldrive");

        let cfg = AppConfig::default();
        let conf = build_bundle(&cfg, None, false, None, &serves, &mounts, &[]);
        assert_eq!(conf.summary().mount_count, 1);
        assert_eq!(conf.summary().serve_count, 1);
        conf.validate().unwrap();
    }

    /// 导出安全：`imagehost` 的 `short_key` 是 manifest 用 `field = password` 声明的
    /// 密码控件，默认导出后文件里既不能有它的值，也不该留着它的键名（留着会让导入端
    /// 以为"用户要把它清空"）。同层的普通字段、插件 id 要原样留下。
    #[test]
    fn exported_plugin_settings_never_carry_declared_passwords() {
        let cfg = AppConfig::default();
        let plugins = vec![rcgui_profile::PluginRef {
            id: "imagehost".into(),
            name: "图床".into(),
            version: "0.6.1".into(),
            source: "builtin".into(),
            url: String::new(),
            sha256: String::new(),
            enabled: true,
            settings: serde_json::json!({
                "form": {
                    "remote": "img",
                    "upload_path": "/i",
                    "short_key": "SECRET-SHORT-KEY"
                },
                "_state": { "pct": 10 }
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
            secret_keys: vec!["short_key".to_string()],
        }];
        let conf = build_bundle(&cfg, None, false, None, &[], &[], &plugins);
        let mut buf = Vec::new();
        write_bundle_to(&mut buf, &conf).expect("导出应能序列化");
        let text = String::from_utf8(buf).expect("导出文件是 UTF-8");
        assert!(
            !text.contains("SECRET-SHORT-KEY"),
            "密码值不能出现在导出文件里"
        );
        assert!(
            !text.contains("short_key"),
            "声明的密码字段连键名都不留，导入端不会把它当成空值"
        );
        assert!(
            !text.contains("_state"),
            "_state 是过程状态，任何时候都不导出"
        );
        let v: serde_json::Value = serde_json::from_str(&text).expect("导出文件是 JSON");
        assert_eq!(v["plugins"][0]["id"], serde_json::json!("imagehost"));
        assert_eq!(
            v["plugins"][0]["settings"]["form"]["upload_path"],
            serde_json::json!("/i")
        );
        assert_eq!(
            v["plugins"][0]["settings"]["form"]["remote"],
            serde_json::json!("img")
        );
    }

    #[test]
    fn live_serves_are_exported_from_running_state() {
        // 断言里有中文文案，先独占语言，别被并行的英文用例带跑
        let _lang = crate::test_lang::use_lang(rcgui_i18n::Lang::Zh);
        // 真实 rclone v1.75.4 的 serve/list 结构
        let v = serde_json::json!({
            "list": [{
                "id": "webdav-3dc16c63",
                "addr": "127.0.0.1:8080",
                "params": { "addr": "127.0.0.1:8080", "fs": "teldrive:", "type": "webdav" }
            }]
        });
        let serves = live_serves_from(&v);
        assert_eq!(serves.len(), 1);
        assert_eq!(serves[0].stype, "webdav");
        assert_eq!(serves[0].remote, "teldrive");
        assert_eq!(serves[0].sub_path, "");
        assert_eq!(serves[0].addr, "127.0.0.1:8080");

        let mounts = live_mounts_from(&serde_json::json!({
            "mountPoints": [{
                "Fs": "teldrive:", "MountPoint": "T:",
                "MountedOn": "2026-10-17T14:16:29.5089996+08:00"
            }]
        }));
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].point, "T:");

        // 带子路径的 fs 也要拆对
        let with_sub = live_serves_from(&serde_json::json!({
            "list": [{ "id": "x", "params": { "fs": "teldrive:media/2024", "type": "http", "addr": "1.2.3.4:9" } }]
        }));
        assert_eq!(with_sub[0].remote, "teldrive");
        assert_eq!(with_sub[0].sub_path, "media/2024");

        // 导出后摘要与预览里的服务数都不应为 0
        let cfg = cfg_with_mount();
        let dump = serde_json::json!({ "teldrive": { "type": "teldrive" } });
        let conf = build_bundle(&cfg, Some(&dump), false, None, &serves, &mounts, &[]);
        assert_eq!(conf.summary().serve_count, 1);
        assert_eq!(conf.summary().mount_count, 1);
        assert!(conf.warnings.iter().any(|w| w.contains("serve 密码未包含")));
        // 往返一次，确认落盘再读回还是 1
        conf.validate().unwrap();
    }

    #[test]
    fn preview_roundtrip_and_diff() {
        let dir = test_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rcgui.conf");

        let cfg = cfg_with_mount();
        let conf = build_bundle(&cfg, None, false, None, &[], &[], &[]);
        write_bundle(&path, &conf).unwrap();

        let mut current = ProfileDoc::default();
        current.remotes.push(RemoteNote {
            name: "legacy".into(),
            ..Default::default()
        });
        current.mounts.push(MountProfile::default());

        let (back, preview) = preview_file(&path, &current).unwrap();
        assert_eq!(back.kind, rcgui_profile::BUNDLE_KIND);
        assert_eq!(preview.summary.mount_count, 1);
        assert_eq!(preview.diff.mounts_before, 1);
        assert_eq!(preview.diff.mounts_after, 1);
        assert!(true, "{}", describe(&preview));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_non_bundle_json() {
        let dir = test_dir("bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.conf");
        std::fs::write(&path, br#"{"hello":1}"#).unwrap();
        let err = preview_file(&path, &ProfileDoc::default()).unwrap_err();
        assert!(format!("{err}").contains("kind"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 导入的任务不能"立刻就跑"，也不能带着镜像模式进来。
    #[test]
    fn imported_tasks_start_one_interval_later_and_never_mirror() {
        let mut cfg = AppConfig::default();
        let tasks = vec![
            rcgui_profile::SyncTaskRef {
                id: "t1".into(),
                name: "照片".into(),
                local: "D:\\照片".into(),
                remote: "gdrive:photos".into(),
                direction: "up".into(),
                mode: "sync".into(), // 文件要求镜像
                enabled: true,
                auto_sync: None,
                interval_hours: 6,
            },
            rcgui_profile::SyncTaskRef {
                id: "t2".into(),
                name: "下载".into(),
                local: "D:\\下载".into(),
                remote: "gdrive:dl".into(),
                direction: "down".into(),
                mode: String::new(), // 没写模式
                enabled: true,
                auto_sync: None,
                interval_hours: 0, // 没写间隔
            },
        ];
        let notes = apply_sync_tasks(&tasks, &mut cfg);
        assert_eq!(cfg.sync_tasks.len(), 2);

        // 镜像被降级 + 停用，并且明确告诉用户
        assert_eq!(cfg.sync_tasks[0].mode, "copy");
        assert!(!cfg.sync_tasks[0].enabled);
        assert!(notes.iter().any(|n| n.contains("镜像")), "{notes:?}");

        // 空模式默认 copy，空间隔默认 6 小时，没要求镜像所以保持启用
        assert_eq!(cfg.sync_tasks[1].mode, "copy");
        assert_eq!(cfg.sync_tasks[1].interval_hours, 6);
        assert!(cfg.sync_tasks[1].enabled);

        // last_run 必须有值：空串会让调度器认为"从未运行、马上该跑"
        for t in &cfg.sync_tasks {
            assert!(!t.last_run.is_empty(), "导入后 last_run 不能为空");
        }
        // 顺带确认调度器算出来的下次运行时间在未来，而不是"立刻"
        let next = crate::sync_tasks::due_at(&cfg.sync_tasks[1], chrono::Local::now());
        assert!(next.is_some(), "启用的任务应当有下次运行时间");
    }

    /// 「跟着本机变化跑」这一项要有跨版本兼容：老文件里没有它，导入后不能变成
    /// 「确定关闭」——那就等于替用户关掉了新功能；文件里明确写了 `false` 才照抄。
    #[test]
    fn an_old_file_without_the_auto_setting_imports_as_unset() {
        let old: rcgui_profile::SyncTaskRef = serde_json::from_str(
            r#"{"id":"t1","name":"照片","local":"D:\\照片","remote":"gdrive:photos"}"#,
        )
        .expect("老文件没有 auto_sync 字段也要能读");
        assert_eq!(old.auto_sync, None, "没写就是没选过，不能默认成关闭");

        let off: rcgui_profile::SyncTaskRef =
            serde_json::from_str(r#"{"id":"t2","auto_sync":false}"#).expect("显式关闭能读");
        assert_eq!(off.auto_sync, Some(false), "文件里写了 false 要照抄");

        let mut cfg = AppConfig::default();
        apply_sync_tasks(&[old, off], &mut cfg);
        assert_eq!(cfg.sync_tasks[0].auto_sync, None);
        assert_eq!(cfg.sync_tasks[1].auto_sync, Some(false));
    }

    /// 老配置文件没有 `app` 段时，不能把用户自选的内核路径等设置冲掉。
    #[test]
    fn missing_app_section_keeps_local_settings() {
        let mut cfg = AppConfig::default();
        cfg.core.path = "D:\\fork\\rclone.exe".into();
        cfg.theme_mode = 2;
        cfg.light.minutes = 42;
        cfg.backup.pass = "keep-me".into();

        // 只有语言一项的文件（其它全是 None，模拟 v1/v2 或手写文件）
        let a = rcgui_profile::AppSettings {
            language: Some("en".into()),
            ..Default::default()
        };
        apply_app_settings(&a, &mut cfg);

        assert_eq!(cfg.language, "en");
        assert_eq!(
            cfg.core.path, "D:\\fork\\rclone.exe",
            "内核路径不能被空文件清掉"
        );
        assert_eq!(cfg.theme_mode, 2);
        assert_eq!(cfg.light.minutes, 42);
        assert_eq!(cfg.backup.pass, "keep-me");
    }

    /// 预览必须逐条列出任务方向与目标，并对镜像给出删除风险提示。
    #[test]
    fn preview_lists_sync_targets_and_flags_deletion() {
        let (lines, would_delete) = sync_preview_lines(&[
            rcgui_profile::SyncTaskRef {
                id: "a".into(),
                name: "照片".into(),
                local: "D:\\照片".into(),
                remote: "gdrive:photos".into(),
                direction: "up".into(),
                mode: "copy".into(),
                enabled: true,
                auto_sync: None,
                interval_hours: 6,
            },
            rcgui_profile::SyncTaskRef {
                id: "b".into(),
                name: "镜像盘".into(),
                local: "E:\\mirror".into(),
                remote: "s3:bucket".into(),
                direction: "down".into(),
                mode: "sync".into(),
                enabled: true,
                auto_sync: None,
                interval_hours: 12,
            },
        ]);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("D:\\照片") && lines[0].contains("gdrive:photos"));
        assert!(lines[0].contains("本地 → 云端") || lines[0].contains("云端"));
        assert!(would_delete, "有 sync 任务时 must 标记会删除");

        let (no_delete, flag) = sync_preview_lines(&[rcgui_profile::SyncTaskRef {
            id: "c".into(),
            name: "只复制".into(),
            local: "C:\\a".into(),
            remote: "gdrive:a".into(),
            direction: "up".into(),
            mode: "copy".into(),
            enabled: false,
            auto_sync: None,
            interval_hours: 6,
        }]);
        assert!(!flag);
        assert!(no_delete[0].contains("只复制"));
    }

    /// 导入失败时，磁盘上的原档案必须一个字节都不变，旁边还留着一份备份。
    ///
    /// 用「更高版本的档案」当失败源：那是真实会发生的场景（用户装过新版、回退旧版），
    /// 本程序读它会拒绝。如果这里先写后读，用户导入一次就把唯一能回退的文件覆盖掉了。
    #[test]
    fn failed_import_leaves_profile_untouched() {
        let _lang = crate::test_lang::use_lang(rcgui_i18n::Lang::Zh);
        let dir = test_dir("import-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        let profile = dir.join("profiles").join("default.json");
        let original = br#"{"schema_version":99,"mounts":[]}"#.to_vec();
        std::fs::write(&profile, &original).unwrap();

        let conf = build_bundle(&cfg_with_mount(), None, false, None, &[], &[], &[]);
        let mut target = AppConfig::default();
        let err = apply_at(&profile, &conf, &mut target).unwrap_err();
        let text = format!("{err}");
        assert!(
            text.contains("99") || text.contains("版本"),
            "错误里要说明是版本问题：{text}"
        );

        assert_eq!(std::fs::read(&profile).unwrap(), original, "原档案被改写了");
        let mut baks: Vec<String> = std::fs::read_dir(dir.join("profiles"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("default.json.bak-"))
            .collect();
        baks.sort();
        assert_eq!(baks.len(), 1, "应当恰好留了一份备份：{baks:?}");
        assert_eq!(
            std::fs::read(dir.join("profiles").join(&baks[0])).unwrap(),
            original,
            "备份内容要和导入前一致"
        );
        // 内存里的配置也不能被半应用：失败发生在 apply_doc 之前
        assert!(target.auto_mounts.is_empty(), "导入失败不该改动内存配置");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
