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
#[derive(Debug, Clone)]
pub struct Preview {
    pub source: PathBuf,
    pub summary: Summary,
    pub diff: Diff,
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
            let params = it.get("params").cloned().unwrap_or_else(|| serde_json::json!({}));
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
            rcgui_i18n::t("挂载/服务由当前运行中的 rclone 状态还原：serve 密码未包含（rclone 不返回），\
             挂载的只读与缓存模式优先沿用 RCGUI 记录、否则取默认值（只读=否、缓存=writes）")
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
            rcgui_i18n::t("导出时未连接 rclone：文件不含任何远程参数，导入后需要重新连接并手工补齐")
                .to_string(),
        ),
    }

    // 凭据策略统一在模型层实现：关键词规则 ∪ provider 元数据标记（IsPassword/Sensitive）
    conf.apply_secret_policy(include_secrets, providers);
    conf
}

/// 原子写入导出文件。
pub fn write_bundle(path: &Path, conf: &RcguiConf) -> Result<()> {
    let text = serde_json::to_vec_pretty(conf).context(rcgui_i18n::t("序列化 rcgui.conf 失败"))?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).with_context(|| rcgui_i18n::f("创建目录 {} 失败", &[&(dir.display())]))?;
        }
    }
    let tmp = path.with_extension("tmp-export");
    std::fs::write(&tmp, &text).with_context(|| rcgui_i18n::f("写入 {} 失败", &[&(tmp.display())]))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("{}", rcgui_i18n::f("保存 {} 失败: {}", &[&(path.display()), &(e)]))
    })?;
    Ok(())
}

/// 解析 + 校验 + 生成摘要/差异。**不写任何文件。**
pub fn preview_file(path: &Path, current: &ProfileDoc) -> Result<(RcguiConf, Preview)> {
    let raw = std::fs::read(path).with_context(|| rcgui_i18n::f("读取 {} 失败", &[&(path.display())]))?;
    // 容忍 BOM（外部编辑器可能加）
    let raw: &[u8] = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &raw[3..]
    } else {
        &raw
    };
    // 先做「是不是 rcgui.conf」的硬校验：RcguiConf 用了容器级 serde(default)，
    // 任意 JSON 都能被填成默认包，必须在反序列化前看原始 kind。
    let probe: serde_json::Value =
        serde_json::from_slice(raw).map_err(|e| anyhow!("{}", rcgui_i18n::f("不是合法的 JSON: {}", &[&(e)])))?;
    match probe.get("kind").and_then(|k| k.as_str()) {
        Some(rgui) if rgui == rcgui_profile::BUNDLE_KIND => {}
        Some(other) => {
            return Err(anyhow!("{}", rcgui_i18n::f("不是 rcgui.conf 文件（kind = \"{}\"，应为 \"{}\"）", &[&(other), &(rcgui_profile::BUNDLE_KIND)])))
        }
        None => {
            return Err(anyhow!("{}", rcgui_i18n::f("不是 rcgui.conf 文件（缺少 kind 字段，应为 \"{}\"）", &[&(rcgui_profile::BUNDLE_KIND)])))
        }
    }
    let mut conf: RcguiConf = serde_json::from_slice(raw)
        .map_err(|e| anyhow!("{}", rcgui_i18n::f("不是合法的 rcgui.conf（JSON 解析失败）: {}", &[&(e)])))?;
    conf.validate().map_err(|e| anyhow!("{e}"))?;
    // 兼容早期导出：serves 为空但 serve_names 有内容时，用映射还原出服务条目，
    // 否则预览会错误地显示「服务 0」。
    conf.normalize_legacy();

    let diff = rcgui_profile::bundle_diff(current, &conf.profile, &conf.remotes);
    let preview = Preview {
        source: path.to_path_buf(),
        summary: conf.summary(),
        diff,
    };
    Ok((conf, preview))
}

/// 备份当前档案文件（导入前调用）。返回备份路径；档案尚不存在时返回 `None`。
pub fn backup_profile() -> Result<Option<PathBuf>> {
    let p = profiles::profile_path();
    if !p.exists() {
        return Ok(None);
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let bak = p.with_file_name(format!("default.json.bak-{stamp}"));
    std::fs::copy(&p, &bak).with_context(|| rcgui_i18n::f("备份档案到 {} 失败", &[&(bak.display())]))?;
    Ok(Some(bak))
}

/// 把导入文件应用到本地档案（含备份 + 回滚点）。**不写 rclone.conf。**
pub fn apply(conf: &RcguiConf, cfg: &mut AppConfig) -> Result<PathBuf> {
    let bak = backup_profile()?
        .ok_or_else(|| anyhow!("{}", rcgui_i18n::t("找不到当前档案文件；为避免覆盖未知数据，已中止导入")))?;
    // apply_doc + 远程元信息合并都在 profiles 里完成，保证只有一个写入口
    profiles::replace_from_bundle(conf, cfg)?;
    Ok(bak)
}

/// 预览的简短人类可读描述（用于 toast）。
pub fn describe(p: &Preview) -> String {
    let s = &p.summary;
    let fname = p
        .source
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("rcgui.conf");
    let mut out = rcgui_i18n::f("{} → 档案「{}」：远程 {} · 挂载 {} · 服务 {}", &[&(fname), &(if s.profile_name.is_empty() {
            rcgui_i18n::t("（未命名）")
        } else {
            &s.profile_name
        }), &(s.remote_count), &(s.mount_count), &(s.serve_count)]);
    out.push_str(&format!("；{}", p.diff.one_line()));
    if s.sensitive {
        out.push_str(&rcgui_i18n::f("；⚠ 含敏感数据；{} 个远程的凭据未随包导出（可能需要重新认证）", &[&(s.needs_auth)]));
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
        let cfg = cfg_with_mount();
        // 离线导出：无 dump、无实时挂载/服务 → 不含远程参数。
        // 内存里遗留的 auto_mounts 仍会被导出（1 项），但服务为 0：
        // 这正是「只有 serve、没有预设」时用户会看到的现象（导出侧已用实时状态修复）。
        let offline = build_bundle(&cfg, None, false, None, &[], &[]);
        assert!(!offline.sensitive);
        assert!(!offline.warnings.is_empty());
        assert_eq!(offline.summary().mount_count, 1);
        assert_eq!(offline.summary().serve_count, 0);

        // 在线导出：有 dump 且含 token → 标记敏感
        let dump = serde_json::json!({
            "teldrive": { "type": "teldrive", "token": "abc" }
        });
        let online = build_bundle(&cfg, Some(&dump), true, None, &[], &[]);
        assert!(online.sensitive);
        assert_eq!(online.remotes.len(), 1);
        assert!(online
            .warnings
            .iter()
            .any(|w| w.contains("并非加密")));
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
        let conf = build_bundle(&cfg, None, false, None, &serves, &mounts);
        assert_eq!(conf.summary().mount_count, 1);
        assert_eq!(conf.summary().serve_count, 1);
        conf.validate().unwrap();
    }

    #[test]
    fn live_serves_are_exported_from_running_state() {
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
        let conf = build_bundle(&cfg, Some(&dump), false, None, &serves, &mounts);
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
        let conf = build_bundle(&cfg, None, false, None, &[], &[]);
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
}
