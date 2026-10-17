//! 档案文档（`profiles/default.json`）⇄ 应用配置 的适配层。
//!
//! 阶段 4 的边界：**落盘结构换成了版本化的档案文档，界面代码一行不用改**。
//! `AppConfig` 里的 `auto_mounts / auto_serves / serve_names` 现在只是内存聚合视图，
//! 不再序列化进 `config.json`（见 `config.rs` 的 `skip_serializing`）；唯一权威落盘位置
//! 是本模块管理的档案文档。
//!
//! 这样做的好处：
//! - 不需要同时改 50 处 UI 读写点，回归风险低；
//! - 落盘只有一个写入口（`save_from_cfg`），不会出现「两份数据不一致」；
//! - 阶段 5 的导入导出可以直接用 `rcgui_profile::ProfileDoc`，无需再做一次结构迁移。

use crate::config::{AppConfig, AutoMount, AutoServe, ServeName};
use anyhow::{anyhow, Result};
use rcgui_profile::{
    ConnectionProfile, MountProfile, ProfileDoc, RemoteNote, ServeNameProfile, ServeProfile, Store,
};
use std::path::PathBuf;
use std::sync::Mutex;

/// 档案文件路径：`<数据目录>/profiles/default.json`
pub fn profile_path() -> PathBuf {
    crate::config::data_dir().join("profiles").join("default.json")
}

/// 当前档案里「凭据未随包导出」的远程名索引（UI 徽标用）。
/// 每次远程列表刷新读一次盘；文件很小，频率也低。
pub fn needs_auth_index() -> std::collections::HashMap<String, bool> {
    match store().load() {
        Ok(Some(doc)) => doc.needs_auth_index(),
        _ => std::collections::HashMap::new(),
    }
}

pub fn store() -> Store {
    Store::new(profile_path())
}

/// 一次性提示（迁移完成 / 档案版本过新等），由 UI 启动后取走并 toast。
static NOTICES: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn take_notices() -> Vec<String> {
    let mut g = NOTICES.lock().unwrap();
    std::mem::take(&mut *g)
}

/// 供数据目录迁移等模块登记一次性提示。
pub fn push_notice(msg: impl Into<String>) {
    notice(msg);
}

fn notice(msg: impl Into<String>) {
    NOTICES.lock().unwrap().push(msg.into());
}

/// 读取档案文档；不存在时把 `config.json` 里的旧字段迁移过来并落盘。
///
/// 返回 `None` 表示档案不可用（版本过新/损坏），此时调用方应保留 `cfg` 里的原值，
/// **不要**写盘，以免覆盖用户数据。
pub fn load_doc(legacy: &AppConfig) -> Option<ProfileDoc> {
    let s = store();
    match s.load() {
        Ok(Some(doc)) => Some(doc),
        Ok(None) => {
            let doc = cfg_to_doc(legacy, None);
            let empty = doc.mounts.is_empty()
                && doc.serves.is_empty()
                && doc.serve_names.is_empty()
                && doc.connections.is_empty();
            if !empty {
                match s.save(&doc) {
                    Ok(()) => notice(format!(
                        "已把挂载/服务预设迁移到档案：{}",
                        s.path().display()
                    )),
                    Err(e) => notice(format!("档案初始化失败（{e}），本次改动仅保存在内存")),
                }
            }
            Some(doc)
        }
        Err(e) => {
            notice(format!("{e}；本次不会写回档案，请处理后重启"));
            None
        }
    }
}

/// 把档案里的三个列表还原成 `AppConfig` 的内存字段。
pub fn apply_doc(doc: &ProfileDoc, cfg: &mut AppConfig) {
    cfg.auto_mounts = doc.mounts.iter().map(mount_to_auto).collect();
    cfg.auto_serves = doc.serves.iter().map(serve_to_auto).collect();
    cfg.serve_names = doc.serve_names.iter().map(name_to_auto).collect();
}

/// 生成「当前档案文档」（导出用）：内存中的 cfg + 磁盘上已有的 meta/remotes。
pub fn current_doc(cfg: &AppConfig) -> ProfileDoc {
    let existing = store().load().ok().flatten();
    cfg_to_doc(cfg, existing.as_ref())
}

/// 用导入文件替换本地档案（挂载 / 服务 / 预设名 / 远程元信息），并把结果同步回内存 cfg。
///
/// 只调用 `Store::save`，即仍然是「单一写入口」；调用方负责先备份。
pub fn replace_from_bundle(
    conf: &rcgui_profile::bundle::RcguiConf,
    cfg: &mut AppConfig,
) -> Result<()> {
    let s = store();
    let mut doc = match s.load() {
        Ok(Some(d)) => d,
        Ok(None) => ProfileDoc::default(),
        Err(e) => return Err(anyhow!("{e}")),
    };
    doc.mounts = conf.profile.mounts.clone();
    doc.serves = conf.profile.serves.clone();
    doc.serve_names = conf.profile.serve_names.clone();
    // 远程元信息以导入文件为准，但保留本地独有的条目：
    // 导入不应该让用户「已经连上的远程」从列表里消失。
    let mut notes = conf.profile.remotes.clone();
    for old in &doc.remotes {
        if !notes.iter().any(|n| n.name.eq_ignore_ascii_case(&old.name)) {
            notes.push(old.clone());
        }
    }
    doc.remotes = notes;
    if !conf.profile.meta.name.is_empty() {
        doc.meta.name = conf.profile.meta.name.clone();
    }
    s.save(&doc).map_err(|e| anyhow!("{e}"))?;
    apply_doc(&doc, cfg);
    Ok(())
}

/// 把 `AppConfig` 的当前状态写回档案文档（保留 meta / remotes / 活动连接等其它字段）。
pub fn save_from_cfg(cfg: &AppConfig) -> Result<()> {
    let s = store();
    let mut doc = match s.load() {
        Ok(Some(d)) => d,
        Ok(None) => ProfileDoc::default(),
        Err(e) => return Err(anyhow!("{e}")),
    };
    doc.mounts = cfg.auto_mounts.iter().map(auto_to_mount).collect();
    doc.serves = cfg.auto_serves.iter().map(auto_to_serve).collect();
    doc.serve_names = cfg.serve_names.iter().map(auto_to_name).collect();
    doc.connections = connections_from_cfg(cfg, &doc);
    if doc.active_connection.is_none() {
        doc.active_connection = doc.connections.first().map(|c| c.id.clone());
    }
    s.save(&doc).map_err(|e| anyhow!("{e}"))
}

/// 记录远程的本地副本（仅非敏感元信息：名字/类型/用量）。
///
/// 完整参数（可能含 OAuth token）不在这里落盘；导出 `rcgui.conf` 时再按需附带并提示。
pub fn set_remotes(rows: &[(String, String, String)]) {
    let s = store();
    let mut doc = match s.load() {
        Ok(Some(d)) => d,
        Ok(None) => ProfileDoc::default(),
        // 档案不可读时不要覆盖（可能是更高版本写的）
        Err(_) => return,
    };
    let notes: Vec<RemoteNote> = rows
        .iter()
        .map(|(name, rtype, usage)| RemoteNote {
            name: name.clone(),
            rtype: rtype.clone(),
            usage: usage.clone(),
            // 来自活连接的远程：凭据就在 rclone 里，不需要重新认证
            needs_auth: false,
        })
        .collect();
    if doc.remotes == notes {
        return; // 没有变化就不写盘
    }
    doc.remotes = notes;
    let _ = s.save(&doc);
}

// ---------------- 结构互转 ----------------

fn auto_to_mount(m: &AutoMount) -> MountProfile {
    MountProfile {
        remote: m.remote.clone(),
        sub_path: m.sub_path.clone(),
        point: m.point.clone(),
        ro: m.ro,
        cache: m.cache.clone(),
        enabled: m.enabled,
    }
}

fn mount_to_auto(m: &MountProfile) -> AutoMount {
    AutoMount {
        remote: m.remote.clone(),
        sub_path: m.sub_path.clone(),
        point: m.point.clone(),
        ro: m.ro,
        cache: if m.cache.is_empty() {
            "writes".to_string()
        } else {
            m.cache.clone()
        },
        enabled: m.enabled,
    }
}

fn auto_to_serve(s: &AutoServe) -> ServeProfile {
    ServeProfile {
        stype: s.stype.clone(),
        remote: s.remote.clone(),
        sub_path: s.sub_path.clone(),
        addr: s.addr.clone(),
        user: s.user.clone(),
        pass: s.pass.clone(),
        enabled: s.enabled,
    }
}

fn serve_to_auto(s: &ServeProfile) -> AutoServe {
    AutoServe {
        stype: s.stype.clone(),
        remote: s.remote.clone(),
        sub_path: s.sub_path.clone(),
        addr: s.addr.clone(),
        user: s.user.clone(),
        pass: s.pass.clone(),
        enabled: s.enabled,
    }
}

fn auto_to_name(n: &ServeName) -> ServeNameProfile {
    ServeNameProfile {
        stype: n.stype.clone(),
        remote: n.remote.clone(),
        sub_path: n.sub_path.clone(),
        addr: n.addr.clone(),
        name: n.name.clone(),
    }
}

fn name_to_auto(n: &ServeNameProfile) -> ServeName {
    ServeName {
        stype: n.stype.clone(),
        remote: n.remote.clone(),
        sub_path: n.sub_path.clone(),
        addr: n.addr.clone(),
        name: n.name.clone(),
    }
}

fn cfg_to_doc(cfg: &AppConfig, existing: Option<&ProfileDoc>) -> ProfileDoc {
    let base = existing.cloned().unwrap_or_default();
    let mut doc = ProfileDoc {
        mounts: cfg.auto_mounts.iter().map(auto_to_mount).collect(),
        serves: cfg.auto_serves.iter().map(auto_to_serve).collect(),
        serve_names: cfg.serve_names.iter().map(auto_to_name).collect(),
        ..base
    };
    doc.connections = connections_from_cfg(cfg, &doc);
    if doc.active_connection.is_none() {
        doc.active_connection = doc.connections.first().map(|c| c.id.clone());
    }
    doc
}

/// 从 `cfg.rclone` 生成单条连接档案。**密码明文不写入档案**，只记 `has_pass`。
fn connections_from_cfg(cfg: &AppConfig, doc: &ProfileDoc) -> Vec<ConnectionProfile> {
    let c = ConnectionProfile {
        id: doc
            .connections
            .first()
            .map(|x| x.id.clone())
            .unwrap_or_else(|| "default".to_string()),
        name: doc
            .connections
            .first()
            .map(|x| x.name.clone())
            .unwrap_or_else(|| "本机".to_string()),
        // 新模型没有「托管模式」：一律按「连接已有实例」记录
        mode: 1,
        url: cfg.rclone.url.clone(),
        // 端口从 endpoint 推导不再单独存；保留字段供旧档案兼容
        port: 0,
        user: cfg.rclone.user.clone(),
        has_pass: !cfg.rclone.pass.is_empty(),
    };
    vec![c]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_cfg() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.auto_mounts.push(AutoMount {
            remote: "teldrive".into(),
            sub_path: "media".into(),
            point: "T:".into(),
            ro: false,
            cache: "writes".into(),
            enabled: true,
        });
        cfg.auto_serves.push(AutoServe {
            stype: "webdav".into(),
            remote: "teldrive".into(),
            sub_path: String::new(),
            addr: "127.0.0.1:8080".into(),
            user: "u".into(),
            pass: "p".into(),
            enabled: true,
        });
        cfg
    }

    #[test]
    fn roundtrip_cfg_doc_cfg() {
        let cfg = legacy_cfg();
        let doc = cfg_to_doc(&cfg, None);
        assert_eq!(doc.mounts.len(), 1);
        assert_eq!(doc.serves.len(), 1);
        // 连接档案里不得出现密码明文
        assert!(doc.connections.iter().all(|c| !c.has_pass || c.user == ""));
        let mut back = AppConfig::default();
        apply_doc(&doc, &mut back);
        assert_eq!(back.auto_mounts, cfg.auto_mounts);
        assert_eq!(back.auto_serves, cfg.auto_serves);
    }

    #[test]
    fn cfg_to_doc_preserves_meta_and_remotes() {
        let mut doc = ProfileDoc::default();
        doc.meta.name = "工作档案".into();
        doc.remotes.push(RemoteNote {
            needs_auth: false,
            name: "gdrive".into(),
            rtype: "drive".into(),
            usage: "1 TB".into(),
        });
        let cfg = legacy_cfg();
        let out = cfg_to_doc(&cfg, Some(&doc));
        assert_eq!(out.meta.name, "工作档案");
        assert_eq!(out.remotes.len(), 1);
    }

    #[test]
    fn empty_cache_falls_back_to_writes() {
        let m = MountProfile {
            cache: String::new(),
            ..Default::default()
        };
        assert_eq!(mount_to_auto(&m).cache, "writes");
    }
}
