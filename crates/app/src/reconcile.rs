//! 统一热重载：把「用户想要的状态」与「rclone 正在运行的状态」逐项比对，
//! 只对**不一致**的项做最小操作（重建挂载 / 重启服务 / 重新下发代理）。
//!
//! ## 为什么单独一个模块
//!
//! 在此之前，「改完某项设置后如何让它生效」这件事被复制到了至少四个地方：
//! 保存代理配置、切换代理绑定、应用导入、连接后重放。每处都要自己判断
//! 「这个 remote 的代理变了吗」「这个挂载还在吗」「这个 serve 地址被占用了吗」——
//! 逻辑一多就必然出现漏写和不一致（比如改了代理配置，绑定的 remote 却仍用旧值）。
//!
//! 本模块把这件事收敛成一条固定流程：
//!
//! ```text
//!   档案（期望）  ──┐
//!                  ├─→ diff ─→ plan ─→ execute ─→ 新的运行态
//!   运行态（现状） ──┘
//! ```
//!
//! 调用方只需要在「某项设置被改动」之后调 `reconcile()`，不必再关心具体怎么做。
//!
//! ## 关于「一份运行时配置文件」
//!
//! 用户建议用一份动态生成的配置文件作为唯一真相源，再拿它和 remote/mount/serve
//! 逐项比对。方向是对的，但 **rclone 的 RC 不提供「从任意配置文件热加载」的接口**：
//! 运行中的实例只认启动时 `--config` 指定的那份文件，改文件不会让已建好的
//! remote 重新初始化。因此这里采用的是同一思想的数据版本：
//! **档案即期望状态文件**，`desired()` 每次从档案现算，与运行态做 diff。
//! 这样「真相源唯一」和「逐项比对热重载」两个目标都达到了，
//! 且不依赖 rclone 并不具备的能力。

use crate::config::AppConfig;
use rcgui_profile::proxy::ProxyProfile;
use rcgui_profile::{MountProfile, ProxyRegistry, ServeProfile};
use rcgui_rc::RcClient;
use std::collections::HashSet;

// ---------------- 期望状态 ----------------

/// 一次「期望状态」快照：从档案现算，不缓存。
#[derive(Debug, Clone, Default)]
pub struct Desired {
    /// remote 名 → 期望的 `override.http_proxy`（`None` = 不代理）
    pub remotes: Vec<(String, Option<String>)>,
    pub mounts: Vec<MountProfile>,
    pub serves: Vec<ServeProfile>,
}

/// 从档案算出期望状态。
///
/// 代理 URL 在这里就构造好（失败则视为「不代理」并记进 `errors`），
/// 避免下游每处都要再处理一次构造失败。
pub fn desired_from(cfg: &AppConfig) -> (Desired, Vec<String>) {
    let mut errors = Vec::new();
    let reg = ProxyRegistry::new(&cfg.proxies, &cfg.proxy_bindings);
    let mut remotes: Vec<(String, Option<String>)> = Vec::new();
    for b in &cfg.proxy_bindings {
        if b.is_direct() {
            // 明确「不代理」也要出现在期望里：它表示「把 override.http_proxy 去掉/还原」
            remotes.push((b.remote.clone(), None));
            continue;
        }
        match reg.by_id(&b.proxy_id) {
            Some(p) => match p.to_proxy_url() {
                Ok(url) => remotes.push((b.remote.clone(), Some(url))),
                Err(e) => errors.push(format!("{} 的代理地址无效：{e}", b.remote)),
            },
            // 悬空引用：期望「不代理」，但要让用户知道这不是他的选择
            None => {
                errors.push(format!("{} 引用的代理配置已不存在", b.remote));
                remotes.push((b.remote.clone(), None));
            }
        }
    }
    (
        Desired {
            remotes,
            mounts: cfg
                .auto_mounts
                .iter()
                .filter(|m| m.enabled)
                .map(|m| MountProfile {
                    remote: m.remote.clone(),
                    sub_path: m.sub_path.clone(),
                    point: m.point.clone(),
                    ro: m.ro,
                    cache: m.cache.clone(),
                    enabled: true,
                })
                .collect(),
            serves: cfg
                .auto_serves
                .iter()
                .filter(|s| s.enabled)
                .map(|s| ServeProfile {
                    stype: s.stype.clone(),
                    remote: s.remote.clone(),
                    sub_path: s.sub_path.clone(),
                    addr: s.addr.clone(),
                    user: s.user.clone(),
                    pass: s.pass.clone(),
                    enabled: true,
                })
                .collect(),
        },
        errors,
    )
}

// ---------------- 运行态 ----------------

/// rclone 当前运行状态（只取比对所需的最小字段）。
#[derive(Debug, Clone, Default)]
pub struct Actual {
    /// 已挂载的（规范化）挂载点
    pub mount_points: Vec<String>,
    /// 已挂载的 fs 串（`remote:sub`）。保留供诊断与将来「fs 变了也要重挂」使用。
    #[allow(dead_code)]
    pub mount_fs: Vec<String>,
    /// 正在监听的 serve 地址 → serve id
    pub serve_addrs: Vec<(String, String)>,
}

pub fn actual_mounts(list: &[rcgui_rc::MountInfo]) -> Actual {
    Actual {
        mount_points: list.iter().map(|m| m.mount_point.clone()).collect(),
        mount_fs: list.iter().map(|m| m.fs.clone()).collect(),
        serve_addrs: Vec::new(),
    }
}

/// 从 `serve/list` 的原始 JSON 解析运行中的服务。
pub fn serve_addrs_from(v: &serde_json::Value) -> Vec<(String, String)> {
    v.get("list")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|it| {
                    let addr = it
                        .get("params")
                        .and_then(|p| p.get("addr"))
                        .or_else(|| it.get("addr"))
                        .and_then(|x| x.as_str())?
                        .to_string();
                    let id = it
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Some((addr, id))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------- 计划 ----------------

/// 一个待执行的动作。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 把某 remote 的代理设为该值
    SetProxy { remote: String, url: String },
    /// 移除某 remote 的代理（恢复原值由 `proxy::apply_to_remote` 负责）
    UnsetProxy { remote: String },
    /// 挂载
    Mount { fs: String, point: String },
    /// 卸载（挂载点已不存在于期望状态，但仍在运行）
    Unmount { point: String },
    /// 启动服务
    StartServe { stype: String, fs: String, addr: String },
    /// 停止服务（id）
    StopServe { addr: String, id: String },
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Self::SetProxy { remote, .. } => format!("设置 {remote} 的代理"),
            Self::UnsetProxy { remote } => format!("移除 {remote} 的代理"),
            Self::Mount { fs, point } => format!("挂载 {fs} → {point}"),
            Self::Unmount { point } => format!("卸载 {point}"),
            Self::StartServe { stype, addr, .. } => format!("启动 {stype} 服务 {addr}"),
            Self::StopServe { addr, .. } => format!("停止服务 {addr}"),
        }
    }
}

/// 把期望与现状比对，算出需要做的动作。
///
/// 纯函数，不碰网络——这让「只重建该重建的」这条要求可以被单元测试固定住。
///
/// `proxy_actual` 是「remote → rclone 里当前的 override.http_proxy」。
/// 只对**档案里出现过绑定**的 remote 做代理比对；档案没提的 remote 一律不碰
/// （用户自己配的代理不归我们管）。
pub fn plan(
    desired: &Desired,
    actual: &Actual,
    proxy_actual: &[(String, Option<String>)],
) -> Vec<Action> {
    let mut out = Vec::new();

    // ---- 代理：只在「期望值与 rclone 当前值不同」时动作 ----
    for (remote, want) in &desired.remotes {
        let cur = proxy_actual
            .iter()
            .find(|(r, _)| r.eq_ignore_ascii_case(remote))
            .and_then(|(_, v)| v.clone());

        match want {
            // 期望设置某个代理
            Some(url) => {
                if cur.as_deref() != Some(url.as_str()) {
                    out.push(Action::SetProxy {
                        remote: remote.clone(),
                        url: url.clone(),
                    });
                }
            }
            // 期望「不代理」：只有当前确实有值时才需要动作。
            // 具体是「恢复原值」还是「删除键」由 `proxy::apply_to_remote` 决定
            // （它手里有 original_override 记录），这里只表达「需要清掉」。
            None => {
                if cur.is_some() {
                    out.push(Action::UnsetProxy {
                        remote: remote.clone(),
                    });
                }
            }
        }
    }

    // ---- 挂载：期望有但没有 → 挂；运行有但期望没有 → 卸 ----
    let want_points: Vec<String> = desired
        .mounts
        .iter()
        .map(|m| crate::normalize_mount_point(&m.point))
        .collect();
    for m in &desired.mounts {
        let point = crate::normalize_mount_point(&m.point);
        if !actual
            .mount_points
            .iter()
            .any(|p| crate::mount_key(p) == crate::mount_key(&point))
        {
            out.push(Action::Mount {
                fs: crate::compose_fs(&m.remote, &m.sub_path),
                point,
            });
        }
    }
    let want_set: HashSet<String> = want_points.iter().map(|p| crate::mount_key(p)).collect();
    for p in &actual.mount_points {
        if !want_set.contains(&crate::mount_key(p)) {
            out.push(Action::Unmount {
                point: p.clone(),
            });
        }
    }

    // ---- 服务：同地址已在监听则不重启；期望没有的停掉 ----
    let want_addrs: Vec<String> = desired.serves.iter().map(|s| s.addr.clone()).collect();
    for s in &desired.serves {
        if !actual.serve_addrs.iter().any(|(a, _)| a == &s.addr) {
            out.push(Action::StartServe {
                stype: s.stype.clone(),
                fs: crate::compose_fs(&s.remote, &s.sub_path),
                addr: s.addr.clone(),
            });
        }
    }
    for (addr, id) in &actual.serve_addrs {
        if !want_addrs.iter().any(|a| a == addr) {
            out.push(Action::StopServe {
                addr: addr.clone(),
                id: id.clone(),
            });
        }
    }

    out
}

// ---------------- 执行 ----------------

/// 执行结果汇总。
#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    pub done: Vec<String>,
    pub failed: Vec<String>,
    /// 已实际发生的动作数（0 表示「本来就是一致的」）
    pub changed: usize,
}

impl ReconcileReport {
    /// 是否什么都没做（含失败）。测试用。
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.done.is_empty() && self.failed.is_empty()
    }

    pub fn summary(&self) -> String {
        if self.changed == 0 && self.failed.is_empty() {
            return "运行状态已与配置一致，无需重载".into();
        }
        let mut s = format!("已重载 {} 项", self.changed);
        if !self.failed.is_empty() {
            s.push_str(&format!("，{} 项失败", self.failed.len()));
        }
        s
    }
}

/// 采集当前运行态（挂载 + 服务）。
pub async fn collect_actual(client: &RcClient) -> Actual {
    let mounts = client.mounts().await.unwrap_or_default();
    let serves = client.serve_list().await.unwrap_or(serde_json::json!({}));
    let mut a = actual_mounts(&mounts);
    a.serve_addrs = serve_addrs_from(&serves);
    a
}

/// 采集「remote → 当前 override.http_proxy」。
///
/// 只查有绑定的 remote，避免为了比对而把全部远程的凭据都读一遍。
pub async fn collect_proxy_actual(
    client: &RcClient,
    remotes: &[String],
) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for r in remotes {
        let v = match client.config_get(r).await {
            Ok(map) => map
                .get("override.http_proxy")
                .map(|x| x.as_str().unwrap_or_default().to_string()),
            Err(_) => None,
        };
        out.push((r.clone(), v));
    }
    out
}

/// 执行动作序列。单条失败不影响其他条目。
pub async fn execute(
    client: &RcClient,
    cfg: &mut AppConfig,
    actions: &[Action],
) -> ReconcileReport {
    let mut rep = ReconcileReport::default();
    for a in actions {
        match a {
            Action::SetProxy { remote, .. } | Action::UnsetProxy { remote } => {
                // 代理的写回需要更新绑定记录（含被覆盖的原值），
                // 因此复用单条下发逻辑，而不是在这里重新实现一遍。
                let profile: Option<ProxyProfile> = {
                    let reg = ProxyRegistry::new(&cfg.proxies, &cfg.proxy_bindings);
                    reg.for_remote(remote).cloned()
                };
                let mut binding = cfg
                    .proxy_bindings
                    .iter()
                    .find(|b| b.remote.eq_ignore_ascii_case(remote))
                    .cloned()
                    .unwrap_or_else(|| rcgui_profile::RemoteProxyBinding {
                        remote: remote.clone(),
                        ..Default::default()
                    });
                match crate::proxy::apply_to_remote(client, remote, profile.as_ref(), &mut binding)
                    .await
                {
                    Ok(out) => {
                        if out.is_failure() {
                            rep.failed.push(out.describe(remote));
                        } else {
                            rep.done.push(out.describe(remote));
                            rep.changed += 1;
                        }
                        if let Some(slot) = cfg
                            .proxy_bindings
                            .iter_mut()
                            .find(|b| b.remote.eq_ignore_ascii_case(remote))
                        {
                            *slot = binding;
                        } else {
                            cfg.proxy_bindings.push(binding);
                        }
                    }
                    Err(e) => rep.failed.push(format!("{remote}: {e}")),
                }
            }
            Action::Mount { fs, point } => {
                let opts = serde_json::json!({ "vfs_cache_mode": "writes", "read_only": false });
                match tokio::time::timeout(crate::MOUNT_RC_TIMEOUT, client.mount(fs, point, opts))
                    .await
                {
                    Ok(Ok(_)) => {
                        rep.done.push(a.describe());
                        rep.changed += 1;
                    }
                    Ok(Err(e)) => rep.failed.push(format!("{}：{e}", a.describe())),
                    Err(_) => rep.failed.push(format!("{}：超时", a.describe())),
                }
            }
            Action::Unmount { point } => match client.unmount(point).await {
                Ok(()) => {
                    rep.done.push(a.describe());
                    rep.changed += 1;
                }
                Err(e) => rep.failed.push(format!("{}：{e}", a.describe())),
            },
            Action::StartServe { stype, fs, addr } => {
                match client.serve_start(stype, fs, addr, "", "").await {
                    Ok(_) => {
                        rep.done.push(a.describe());
                        rep.changed += 1;
                    }
                    Err(e) => rep.failed.push(format!("{}：{e}", a.describe())),
                }
            }
            Action::StopServe { id, .. } => match client.serve_stop(id).await {
                Ok(()) => {
                    rep.done.push(a.describe());
                    rep.changed += 1;
                }
                Err(e) => rep.failed.push(format!("{}：{e}", a.describe())),
            },
        }
    }
    rep
}

/// 一站式：采集现状 → 比对 → 执行。
///
/// 这是所有「某项设置改完之后让它生效」的唯一入口。
///
/// **签名注意**：这里接收 `&mut AppConfig` 而不是 `MutexGuard`，调用方必须
/// 先 clone 出配置、放锁、再调用（把结果写回）。原因是本函数内部有 `await`，
/// 而 `std::sync::MutexGuard` 不是 `Send`，跨越 `await` 会让整个 future 变成
/// 非 `Send`，无法交给 `tokio::spawn`。
pub async fn reconcile(client: &RcClient, cfg: &mut AppConfig) -> ReconcileReport {
    let (desired, errors) = desired_from(cfg);
    let actual = collect_actual(client).await;
    let remotes: Vec<String> = desired.remotes.iter().map(|(r, _)| r.clone()).collect();
    let proxy_actual = collect_proxy_actual(client, &remotes).await;
    let actions = plan(&desired, &actual, &proxy_actual);
    let mut rep = execute(client, cfg, &actions).await;
    rep.failed.extend(errors);
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AutoMount, AutoServe};

    fn cfg_with_mount(remote: &str, point: &str) -> AppConfig {
        let mut c = AppConfig::default();
        c.auto_mounts.push(AutoMount {
            remote: remote.into(),
            sub_path: String::new(),
            point: point.into(),
            ro: false,
            cache: "writes".into(),
            enabled: true,
        });
        c
    }

    fn cfg_with_serve(addr: &str) -> AppConfig {
        let mut c = AppConfig::default();
        c.auto_serves.push(AutoServe {
            stype: "webdav".into(),
            remote: "r".into(),
            sub_path: String::new(),
            addr: addr.into(),
            user: String::new(),
            pass: String::new(),
            enabled: true,
        });
        c
    }

    #[test]
    fn no_drift_means_no_actions() {
        let cfg = cfg_with_mount("r", "T:");
        let (desired, errs) = desired_from(&cfg);
        assert!(errs.is_empty());
        let actual = Actual {
            mount_points: vec!["T:".into()],
            mount_fs: vec!["r:".into()],
            serve_addrs: Vec::new(),
        };
        assert!(
            plan(&desired, &actual, &[]).is_empty(),
            "已经在跑且一致时不应产生任何动作"
        );
    }

    #[test]
    fn missing_mount_is_created() {
        let cfg = cfg_with_mount("r", "T:");
        let (desired, _) = desired_from(&cfg);
        let actual = Actual::default();
        let acts = plan(&desired, &actual, &[]);
        // 注意：app 侧的 normalize_mount_point 会把 "T:" 规范成 "T:\\"
        // （与 profile crate 的 "T:" 形式不同；挂载这一步用的是 app 侧实现）
        assert_eq!(
            acts,
            vec![Action::Mount { fs: "r:".into(), point: "T:\\".into() }]
        );
    }

    /// 挂载点用不同写法（T: / T:\）时必须视为同一个，否则会重复挂载。
    #[test]
    fn mount_point_forms_are_normalised() {
        let cfg = cfg_with_mount("r", "T:\\");
        let (desired, _) = desired_from(&cfg);
        let actual = Actual {
            mount_points: vec!["T:".into()],
            mount_fs: vec!["r:".into()],
            serve_addrs: Vec::new(),
        };
        assert!(plan(&desired, &actual, &[]).is_empty());
    }

    #[test]
    fn extra_mount_is_unmounted() {
        let cfg = AppConfig::default(); // 期望无挂载
        let (desired, _) = desired_from(&cfg);
        let actual = Actual {
            mount_points: vec!["U:".into()],
            mount_fs: vec!["x:".into()],
            serve_addrs: Vec::new(),
        };
        let acts = plan(&desired, &actual, &[]);
        assert_eq!(acts, vec![Action::Unmount { point: "U:".into() }]);
    }

    #[test]
    fn disabled_mount_is_not_desired() {
        let mut cfg = cfg_with_mount("r", "T:");
        cfg.auto_mounts[0].enabled = false;
        let (desired, _) = desired_from(&cfg);
        assert!(desired.mounts.is_empty(), "停用的挂载不应进入期望状态");
    }

    #[test]
    fn serve_already_listening_is_not_restarted() {
        let cfg = cfg_with_serve("127.0.0.1:8080");
        let (desired, _) = desired_from(&cfg);
        let actual = Actual {
            mount_points: vec![],
            mount_fs: vec![],
            serve_addrs: vec![("127.0.0.1:8080".into(), "id1".into())],
        };
        assert!(
            plan(&desired, &actual, &[]).is_empty(),
            "已在监听的地址不能再 serve/start（会报 bind: address already in use）"
        );
    }

    #[test]
    fn missing_serve_is_started() {
        let cfg = cfg_with_serve("127.0.0.1:8081");
        let (desired, _) = desired_from(&cfg);
        let actual = Actual::default();
        let acts = plan(&desired, &actual, &[]);
        assert_eq!(
            acts,
            vec![Action::StartServe {
                stype: "webdav".into(),
                fs: "r:".into(),
                addr: "127.0.0.1:8081".into()
            }]
        );
    }

    #[test]
    fn extra_serve_is_stopped() {
        let cfg = AppConfig::default();
        let (desired, _) = desired_from(&cfg);
        let actual = Actual {
            mount_points: vec![],
            mount_fs: vec![],
            serve_addrs: vec![("127.0.0.1:9".into(), "id9".into())],
        };
        let acts = plan(&desired, &actual, &[]);
        assert_eq!(
            acts,
            vec![Action::StopServe { addr: "127.0.0.1:9".into(), id: "id9".into() }]
        );
    }

    #[test]
    fn proxy_change_produces_set() {
        let mut cfg = AppConfig::default();
        let p = ProxyProfile {
            id: "p1".into(),
            name: "n".into(),
            protocol: rcgui_profile::ProxyProtocol::Http,
            host: "127.0.0.1".into(),
            port: 8899,
            user: String::new(),
            pass: String::new(),
        };
        cfg.proxies.push(p);
        cfg.bind_proxy_slot("teldrive", "p1");
        let (desired, errs) = desired_from(&cfg);
        assert!(errs.is_empty());
        // rclone 里还没有这个键
        let proxy_actual = vec![("teldrive".to_string(), None)];
        let acts = plan(&desired, &Actual::default(), &proxy_actual);
        assert_eq!(
            acts,
            vec![Action::SetProxy {
                remote: "teldrive".into(),
                url: "http://127.0.0.1:8899".into()
            }]
        );
    }

    /// 代理值一致时不应重复下发（避免每次 reconcile 都打一次 config/update）。
    #[test]
    fn unchanged_proxy_is_noop() {
        let mut cfg = AppConfig::default();
        cfg.proxies.push(ProxyProfile {
            id: "p1".into(),
            name: "n".into(),
            protocol: rcgui_profile::ProxyProtocol::Http,
            host: "h".into(),
            port: 1,
            user: String::new(),
            pass: String::new(),
        });
        cfg.bind_proxy_slot("r", "p1");
        let (desired, _) = desired_from(&cfg);
        let proxy_actual = vec![("r".to_string(), Some("http://h:1".to_string()))];
        assert!(plan(&desired, &Actual::default(), &proxy_actual).is_empty());
    }

    /// 「不代理」+ rclone 里有值 → 应产生移除动作（实际恢复原值由 apply 负责）。
    #[test]
    fn direct_binding_clears_existing_proxy() {
        let mut cfg = AppConfig::default();
        cfg.bind_proxy_slot("r", "");
        let (desired, _) = desired_from(&cfg);
        let proxy_actual = vec![("r".to_string(), Some("http://old:1".to_string()))];
        let acts = plan(&desired, &Actual::default(), &proxy_actual);
        assert_eq!(acts, vec![Action::UnsetProxy { remote: "r".into() }]);
    }

    #[test]
    fn dangling_binding_reports_error_and_clears() {
        let mut cfg = AppConfig::default();
        cfg.bind_proxy_slot("r", "gone-id");
        let (desired, errs) = desired_from(&cfg);
        assert_eq!(desired.remotes.len(), 1);
        assert!(desired.remotes[0].1.is_none(), "悬空引用按不代理处理");
        assert!(errs.iter().any(|e| e.contains("已不存在")));
    }

    #[test]
    fn report_summary_messages() {
        let mut r = ReconcileReport::default();
        assert!(r.is_empty());
        assert!(r.summary().contains("无需重载"));
        r.changed = 2;
        r.done.push("x".into());
        assert!(r.summary().contains("已重载 2 项"));
        r.failed.push("y".into());
        assert!(r.summary().contains("1 项失败"));
    }
}
