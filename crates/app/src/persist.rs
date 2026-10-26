//! 用户操作的持久化：把「用户想要的挂载 / 服务」记进档案，并在每次连接成功后重放。
//!
//! 背景（产品决策）：
//! - 挂载 / 服务在 rclone 里**没有**持久化概念：`rclone mount` / `rclone serve` 都是前台进程，
//!   rcd 重启后必须重新下发。用户每次都要重新配一遍，体验很差。
//! - 因此 RCGUI 自己记住这些期望状态，连接成功后自动通过 RC 重新创建。
//! - 这不是「预设 UI」回归：界面上没有额外的预设管理，只有用户实际执行过的操作被记住，
//!   并且随时可以在「活动挂载 / 运行中服务」里停掉（停掉即忘记）。
//!
//! 与 rclone 生命周期无关：只发 RC 请求，绝不启动/停止 rclone 进程。

use crate::config::{self, AutoMount, AutoServe, AppConfig, ServeName};
use crate::{mount_key, normalize_mount_point, Ctx};
use anyhow::anyhow;
use std::sync::Arc;

/// 记下一次成功的挂载（按挂载点去重）。
pub fn remember_mount(
    cfg: &mut AppConfig,
    remote: &str,
    sub_path: &str,
    point: &str,
    ro: bool,
    cache: &str,
) -> anyhow::Result<()> {
    let item = AutoMount {
        remote: remote.to_string(),
        sub_path: sub_path.trim().trim_matches('/').to_string(),
        point: normalize_mount_point(point),
        ro,
        cache: cache.to_string(),
        enabled: true,
    };
    let key = mount_key(&item.point);
    match cfg.auto_mounts.iter().position(|m| mount_key(&m.point) == key) {
        Some(i) => cfg.auto_mounts[i] = item,
        None => cfg.auto_mounts.push(item),
    }
    config::save(cfg).map_err(|e| anyhow!("{}", rcgui_i18n::f("记住挂载失败: {}", &[&(e)])))
}

/// 忘掉某个挂载点（用户主动卸载时调用）。
pub fn forget_mount(cfg: &mut AppConfig, point: &str) {
    let key = mount_key(point);
    cfg.auto_mounts.retain(|m| mount_key(&m.point) != key);
}

/// 记下一次启动成功的服务（按 协议+地址 去重）。
pub fn remember_serve(
    cfg: &mut AppConfig,
    stype: &str,
    remote: &str,
    sub_path: &str,
    addr: &str,
    user: &str,
    pass: &str,
) -> anyhow::Result<()> {
    let item = AutoServe {
        stype: stype.to_string(),
        remote: remote.to_string(),
        sub_path: sub_path.trim().trim_matches('/').to_string(),
        addr: addr.to_string(),
        user: user.to_string(),
        pass: pass.to_string(),
        enabled: true,
    };
    cfg.auto_serves
        .retain(|s| !(s.stype == item.stype && s.addr == item.addr));
    cfg.auto_serves.push(item);
    config::save(cfg).map_err(|e| anyhow!("{}", rcgui_i18n::f("记住服务失败: {}", &[&(e)])))
}

/// 忘掉一个服务（用户停止，或该服务已不存在时调用）。
pub fn forget_serve(cfg: &mut AppConfig, addr: &str) {
    cfg.auto_serves.retain(|s| s.addr != addr);
    cfg.serve_names.retain(|n| n.addr != addr);
}

/// 记一次服务显示名（供重放后保持名字稳定）。
pub fn remember_serve_name(
    cfg: &mut AppConfig,
    stype: &str,
    remote: &str,
    sub_path: &str,
    addr: &str,
    name: &str,
) {
    cfg.serve_names
        .retain(|n| !(n.stype == stype && n.addr == addr));
    cfg.serve_names.push(ServeName {
        stype: stype.to_string(),
        remote: remote.to_string(),
        sub_path: sub_path.to_string(),
        addr: addr.to_string(),
        name: name.to_string(),
    });
}

/// 远程被删除时级联清掉相关记忆（否则重放会一直失败并刷错误提示）。
/// 返回 (需要卸载的挂载点, 需要停止的服务地址)。
pub fn forget_remote(cfg: &mut AppConfig, name: &str) -> (Vec<String>, Vec<String>) {
    let points: Vec<String> = cfg
        .auto_mounts
        .iter()
        .filter(|m| m.remote == name)
        .map(|m| m.point.clone())
        .collect();
    let addrs: Vec<String> = cfg
        .auto_serves
        .iter()
        .filter(|s| s.remote == name)
        .map(|s| s.addr.clone())
        .collect();
    cfg.auto_mounts.retain(|m| m.remote != name);
    cfg.auto_serves.retain(|s| s.remote != name);
    cfg.serve_names.retain(|s| s.remote != name);
    (points, addrs)
}

/// 连接成功后重放：把档案里记住的挂载与服务重新创建一遍。
///
/// - 已存在（同盘符 / 同地址）的直接跳过，不做无意义调用；
/// - 单条失败只提示该条，不影响其他条目；
/// - 全程只发 RC 请求，不触碰 rclone 进程。
pub async fn replay(ctx: &Arc<Ctx>, ui: &slint::Weak<crate::MainWindow>) {
    let (mounts, serves) = {
        let cfg = ctx.cfg.lock().unwrap();
        (cfg.auto_mounts.clone(), cfg.auto_serves.clone())
    };
    if mounts.is_empty() && serves.is_empty() {
        return;
    }
    // 注意：`ctx.cfg` 的 MutexGuard 不能跨越 await（未来必须 Send），
    // 因此上面先 clone 出数据、立刻释放锁，再进入异步调用。
    let client = ctx.rc.client().await;

    // 对端没配 RC 鉴权时，除 `core/version` 之类的免鉴权接口外一律 403。继续往下走
    // 只会得到一句「自动恢复有 5 项失败：… authentication must be set up …」的复读提示，
    // 而真正的诊断已经写在连接状态里（「已连接（对端未配置鉴权）」）。
    // 用 `rc/noopauth`（NoAuth:false、无副作用）探一次，命中就直接跳过，不刷假失败。
    if let Err(e) = client.noopauth().await {
        if e.is_auth_not_configured() {
            return;
        }
    }

    // 已挂载的盘符 / 已在监听的服务地址：避免重复创建
    let live_mounts: Vec<(String, String)> = client
        .mounts()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|m| (m.fs, m.mount_point))
        .collect();
    let live_addrs: Vec<String> = match client.serve_list().await {
        Ok(v) => v
            .get("list")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|it| {
                        it.get("params")
                            .and_then(|p| p.get("addr"))
                            .or_else(|| it.get("addr"))
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    let mut ok_m = 0usize;
    let mut ok_s = 0usize;
    let mut errs: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();

    for m in &mounts {
        let point = normalize_mount_point(&m.point);
        if live_mounts
            .iter()
            .any(|(_, p)| mount_key(p) == mount_key(&point))
        {
            continue;
        }
        let fs = crate::compose_fs(&m.remote, &m.sub_path);
        let opts = serde_json::json!({
            "vfs_cache_mode": m.cache,
            "read_only": m.ro,
        });
        match tokio::time::timeout(crate::MOUNT_RC_TIMEOUT, client.mount(&fs, &point, opts)).await {
            Ok(Ok(_)) => ok_m += 1,
            Ok(Err(e)) => {
                errs.push(rcgui_i18n::f("挂载 {} → {}: {}", &[&(fs), &(point), &(e)]));
                stale.push(point);
            }
            Err(_) => {
                errs.push(rcgui_i18n::f("挂载 {} → {}: 超时", &[&(fs), &(point)]));
                stale.push(point);
            }
        }
    }

    for s in &serves {
        if live_addrs.iter().any(|a| a == &s.addr) {
            continue;
        }
        let fs = crate::compose_fs(&s.remote, &s.sub_path);
        match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            client.serve_start(&s.stype, &fs, &s.addr, &s.user, &s.pass),
        )
        .await
        {
            Ok(Ok(_)) => ok_s += 1,
            Ok(Err(e)) => errs.push(rcgui_i18n::f("服务 {} {}: {}", &[&(s.stype), &(s.addr), &(e)])),
            Err(_) => errs.push(rcgui_i18n::f("服务 {} {}: 超时", &[&(s.stype), &(s.addr)])),
        }
    }

    if ok_m > 0 || ok_s > 0 {
        let _ = Box::pin(crate::handle_cmd(ctx, ui, crate::Cmd::MountsRefresh)).await;
        let _ = Box::pin(crate::handle_cmd(ctx, ui, crate::Cmd::ServesRefresh)).await;
        crate::toast(
            ui,
            1,
            rcgui_i18n::f("已恢复上次的挂载 {} 项 / 服务 {} 项", &[&(ok_m), &(ok_s)]),
        );
    }
    if !errs.is_empty() {
        crate::toast(
            ui,
            2,
            rcgui_i18n::f("自动恢复有 {} 项失败：{}", &[&(errs.len()), &(errs.join("；"))]),
        );
    }
    let _ = stale;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AutoMount, AutoServe};

    fn cfg_with_entries() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.auto_mounts.push(AutoMount {
            remote: "teldrive".into(),
            sub_path: String::new(),
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
            user: String::new(),
            pass: String::new(),
            enabled: true,
        });
        cfg.auto_serves.push(AutoServe {
            stype: "http".into(),
            remote: "other".into(),
            sub_path: String::new(),
            addr: "127.0.0.1:9090".into(),
            user: String::new(),
            pass: String::new(),
            enabled: true,
        });
        cfg
    }

    #[test]
    fn forget_remote_drops_related_entries() {
        let mut cfg = cfg_with_entries();
        let (points, addrs) = forget_remote(&mut cfg, "teldrive");
        assert_eq!(points, vec!["T:".to_string()]);
        assert_eq!(addrs, vec!["127.0.0.1:8080".to_string()]);
        assert!(cfg.auto_mounts.is_empty());
        assert_eq!(cfg.auto_serves.len(), 1);
        assert_eq!(cfg.auto_serves[0].remote, "other");
    }

    #[test]
    fn remember_mount_is_idempotent_across_separator_forms() {
        // "T:\" 与 "T:" 必须视为同一个挂载点，否则重放会重复挂载
        let mut cfg = AppConfig::default();
        cfg.auto_mounts.push(AutoMount {
            remote: "a".into(),
            sub_path: String::new(),
            point: "T:\\".into(),
            ro: false,
            cache: "writes".into(),
            enabled: true,
        });
        let key = mount_key("T:");
        let mut found = None;
        for (i, m) in cfg.auto_mounts.iter().enumerate() {
            if mount_key(&m.point) == key {
                found = Some(i);
            }
        }
        assert_eq!(found, Some(0), "\"T:\\\\\" 应与 \"T:\" 命中同一条目");
    }

    #[test]
    fn forget_serve_removes_name_mapping_too() {
        let mut cfg = cfg_with_entries();
        remember_serve_name(
            &mut cfg,
            "webdav",
            "teldrive",
            "",
            "127.0.0.1:8080",
            "Serve_01",
        );
        assert_eq!(cfg.serve_names.len(), 1);
        forget_serve(&mut cfg, "127.0.0.1:8080");
        assert!(cfg.auto_serves.iter().all(|s| s.addr != "127.0.0.1:8080"));
        assert!(cfg.serve_names.is_empty(), "名称映射也要一起清掉");
    }
}
