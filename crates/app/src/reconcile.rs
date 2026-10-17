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
use rcgui_profile::proxy::{plan_apply, ApplyPlan, ProxyConfigValues, ProxyProfile};
use rcgui_profile::{MountProfile, ProxyRegistry, RemoteProxyBinding, ServeProfile};
use rcgui_rc::RcClient;
use std::collections::HashSet;

// ---------------- 期望状态 ----------------

/// 一个 remote 的期望代理，以及档案里的绑定记录。
///
/// 绑定记录一并带上：`plan_apply` 需要它才知道「上次写的是哪个键、原值是什么」，
/// 否则没法判断该回滚哪个键、也没法识别「被外部改过」。
#[derive(Debug, Clone, PartialEq)]
pub struct DesiredRemote {
    pub remote: String,
    /// `None` = 明确不代理 / 引用的配置已不存在
    pub proxy: Option<ProxyProfile>,
    pub binding: RemoteProxyBinding,
}

/// 一次「期望状态」快照：从档案现算，不缓存。
#[derive(Debug, Clone, Default)]
pub struct Desired {
    pub remotes: Vec<DesiredRemote>,
    pub mounts: Vec<MountProfile>,
    pub serves: Vec<ServeProfile>,
}

/// 从档案算出期望状态。
///
/// 悬空引用（引用的配置已删除）按「不代理」处理，同时记一条错误；若该绑定
/// 还留着脚印，`plan` 会顺带把它回滚掉，不会把 rclone 里的旧代理留在那儿。
///
/// 值的构造（URL / 原生 socks 格式）不在这里做：它依赖 remote 的真实类型，
/// 只有 `config/get` 才有权威答案，统一在 `plan_apply` 里处理。
pub fn desired_from(cfg: &AppConfig) -> (Desired, Vec<String>) {
    let mut errors = Vec::new();
    let reg = ProxyRegistry::new(&cfg.proxies, &cfg.proxy_bindings);
    let mut remotes = Vec::new();
    for b in &cfg.proxy_bindings {
        let proxy = if b.is_direct() {
            None
        } else {
            match reg.by_id(&b.proxy_id) {
                Some(p) => Some(p.clone()),
                // 悬空引用：期望「不代理」，但要让用户知道这不是他的选择
                None => {
                    errors.push(format!("{} 引用的代理配置已不存在", b.remote));
                    None
                }
            }
        };
        remotes.push(DesiredRemote {
            remote: b.remote.clone(),
            proxy,
            binding: b.clone(),
        });
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
    /// 让某 remote 的代理变成档案里的期望。具体写哪个键、写什么值由
    /// `proxy::apply_to_remote` 按真实 backend 类型决定，这里只表达「要动」。
    SetProxy { remote: String },
    /// 移除/还原某 remote 的代理（用绑定里记录的键与回滚值）
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
            Self::SetProxy { remote } => format!("设置 {remote} 的代理"),
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
/// 代理部分直接复用 `plan_apply`（与真正下发时同一套规则），避免「计划说不用动、
/// 执行时却又写了」或反过来的不一致。只对**档案里出现过绑定**的 remote 比对；
/// 档案没提的 remote 一律不碰（用户自己配的代理不归我们管）。
///
/// 返回 `(动作, 错误)`；错误用于「代理配置本身无效」这类没法变成动作的问题。
pub fn plan(
    desired: &Desired,
    actual: &Actual,
    proxy_actual: &[ProxyActual],
) -> (Vec<Action>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();

    // ---- 代理：只在「期望与现状不一致」时动作 ----
    for d in &desired.remotes {
        let Some(state) = proxy_actual
            .iter()
            .find(|s| s.remote.eq_ignore_ascii_case(&d.remote))
        else {
            // config_get 失败：collect 阶段已记错误。不知道该看哪个键时宁可不动。
            continue;
        };
        let plan = match plan_apply(
            d.proxy.as_ref(),
            &state.backend_type,
            &state.values,
            Some(&d.binding),
        ) {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!("{} 的代理配置无效：{e}", d.remote));
                continue;
            }
        };
        match plan {
            // 已是期望 / 从没管过：无需动作
            ApplyPlan::Nothing { .. } | ApplyPlan::Unmanaged => {}
            // 其余情况（含冲突、尊重用户已有配置）都交给 apply 判定并报告：
            // plan 只回答「要不要动」，规则不重复实现第二遍。
            _ => out.push(if d.proxy.is_some() {
                Action::SetProxy {
                    remote: d.remote.clone(),
                }
            } else {
                Action::UnsetProxy {
                    remote: d.remote.clone(),
                }
            }),
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

    (out, errors)
}

// ---------------- 执行 ----------------

/// 执行结果汇总。
#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    pub done: Vec<String>,
    /// 没写入、但需要告诉用户的原因（如「该远程已有你自己设的原生代理」）
    pub skipped: Vec<String>,
    pub failed: Vec<String>,
    /// 已实际发生的动作数（0 表示「本来就是一致的」）
    pub changed: usize,
}

impl ReconcileReport {
    /// 是否什么都没做（含失败）。测试用。
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.done.is_empty() && self.skipped.is_empty() && self.failed.is_empty()
    }

    /// 给用户看的细节：失败在前，跳过在后。
    pub fn detail(&self) -> String {
        let mut parts = self.failed.clone();
        parts.extend(self.skipped.iter().cloned());
        parts.join("；")
    }

    pub fn summary(&self) -> String {
        if self.changed == 0 && self.failed.is_empty() && self.skipped.is_empty() {
            return "运行状态已与配置一致，无需重载".into();
        }
        let mut s = format!("已重载 {} 项", self.changed);
        if !self.skipped.is_empty() {
            s.push_str(&format!("，{} 项跳过", self.skipped.len()));
        }
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

/// 一个 remote 的代理现状：真实 backend 类型 + 三个可能的代理键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyActual {
    pub remote: String,
    /// `config/get` 里的 `type`（权威值；空串 = 拿不到，按 override 处理）
    pub backend_type: String,
    pub values: ProxyConfigValues,
}

/// 采集有绑定 remote 的代理现状。
///
/// 只查有绑定的 remote，避免为了比对而把全部远程的凭据都读一遍。
///
/// 读取失败**不能吞成空值**：分键之后“不知道该看哪个键”会让判断完全错位
/// （把用户已有的代理当成不存在，或反过来），因此失败记进错误列表并跳过
/// 该 remote —— 宁可不判定，也不猜。
pub async fn collect_proxy_actual(
    client: &RcClient,
    remotes: &[String],
) -> (Vec<ProxyActual>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for r in remotes {
        match client.config_get(r).await {
            Ok(map) => out.push(ProxyActual {
                remote: r.clone(),
                backend_type: ProxyConfigValues::backend_type(&map),
                values: ProxyConfigValues::from_config_map(&map),
            }),
            Err(e) => errors.push(format!("读取 {r} 的代理状态失败：{e}（本次跳过该远程）")),
        }
    }
    (out, errors)
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
            Action::SetProxy { remote } | Action::UnsetProxy { remote } => {
                // 代理的写回需要更新绑定记录（脚印 + 用的哪个键），
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
                    .unwrap_or_else(|| RemoteProxyBinding {
                        remote: remote.clone(),
                        ..Default::default()
                    });
                let result =
                    crate::proxy::apply_to_remote(client, remote, profile.as_ref(), &mut binding)
                        .await;
                // **失败也要写回**：换键时旧键可能已经成功还原，丢掉这次更新会让
                // 下一次比对把「已还原」误判成「被外部改过」。
                if let Some(slot) = cfg
                    .proxy_bindings
                    .iter_mut()
                    .find(|b| b.remote.eq_ignore_ascii_case(remote))
                {
                    *slot = binding;
                } else {
                    cfg.proxy_bindings.push(binding);
                }
                match result {
                    Ok(out) => {
                        if out.is_failure() {
                            rep.failed.push(out.describe(remote));
                        } else if out.is_skipped() {
                            rep.skipped.push(out.describe(remote));
                        } else if out.is_changed() {
                            rep.done.push(out.describe(remote));
                            rep.changed += 1;
                        }
                        // Unchanged / LeftAlone：没有变化，不用报告
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
    let (desired, mut errors) = desired_from(cfg);
    let actual = collect_actual(client).await;
    let remotes: Vec<String> = desired.remotes.iter().map(|r| r.remote.clone()).collect();
    let (proxy_actual, proxy_errors) = collect_proxy_actual(client, &remotes).await;
    errors.extend(proxy_errors);
    let (actions, plan_errors) = plan(&desired, &actual, &proxy_actual);
    errors.extend(plan_errors);
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

    fn proxy_profile(id: &str, protocol: rcgui_profile::ProxyProtocol, host: &str, port: u16) -> ProxyProfile {
        ProxyProfile {
            id: id.into(),
            name: "n".into(),
            protocol,
            host: host.into(),
            port,
            user: String::new(),
            pass: String::new(),
        }
    }

    fn state(remote: &str, rtype: &str, values: ProxyConfigValues) -> ProxyActual {
        ProxyActual {
            remote: remote.into(),
            backend_type: rtype.into(),
            values,
        }
    }

    fn binding_with_footprint(remote: &str, key: &str, value: &str, original: Option<&str>) -> RemoteProxyBinding {
        RemoteProxyBinding {
            remote: remote.into(),
            proxy_id: String::new(),
            applied: true,
            applied_key: key.into(),
            applied_value: value.into(),
            original_override: original.map(str::to_string),
        }
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
        let (acts, errs) = plan(&desired, &actual, &[]);
        assert!(errs.is_empty());
        assert!(
            acts.is_empty(),
            "已经在跑且一致时不应产生任何动作"
        );
    }

    #[test]
    fn missing_mount_is_created() {
        let cfg = cfg_with_mount("r", "T:");
        let (desired, _) = desired_from(&cfg);
        let actual = Actual::default();
        let (acts, _) = plan(&desired, &actual, &[]);
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
        assert!(plan(&desired, &actual, &[]).0.is_empty());
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
        let (acts, _) = plan(&desired, &actual, &[]);
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
            plan(&desired, &actual, &[]).0.is_empty(),
            "已在监听的地址不能再 serve/start（会报 bind: address already in use）"
        );
    }

    #[test]
    fn missing_serve_is_started() {
        let cfg = cfg_with_serve("127.0.0.1:8081");
        let (desired, _) = desired_from(&cfg);
        let actual = Actual::default();
        let (acts, _) = plan(&desired, &actual, &[]);
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
        let (acts, _) = plan(&desired, &actual, &[]);
        assert_eq!(
            acts,
            vec![Action::StopServe { addr: "127.0.0.1:9".into(), id: "id9".into() }]
        );
    }

    #[test]
    fn proxy_change_produces_set() {
        let mut cfg = AppConfig::default();
        cfg.proxies
            .push(proxy_profile("p1", rcgui_profile::ProxyProtocol::Http, "127.0.0.1", 8899));
        cfg.bind_proxy_slot("teldrive", "p1");
        let (desired, errs) = desired_from(&cfg);
        assert!(errs.is_empty());
        // rclone 里还没有这个键
        let proxy_actual = vec![state("teldrive", "s3", ProxyConfigValues::default())];
        let (acts, errs) = plan(&desired, &Actual::default(), &proxy_actual);
        assert!(errs.is_empty());
        assert_eq!(
            acts,
            vec![Action::SetProxy {
                remote: "teldrive".into()
            }]
        );
    }

    /// 代理值一致时不应重复下发（避免每次 reconcile 都打一次 config/update）。
    #[test]
    fn unchanged_proxy_is_noop() {
        let mut cfg = AppConfig::default();
        cfg.proxies
            .push(proxy_profile("p1", rcgui_profile::ProxyProtocol::Http, "h", 1));
        cfg.bind_proxy_slot("r", "p1");
        let (desired, _) = desired_from(&cfg);
        let proxy_actual = vec![state(
            "r",
            "s3",
            ProxyConfigValues {
                override_http: Some("http://h:1".into()),
                ..Default::default()
            },
        )];
        assert!(plan(&desired, &Actual::default(), &proxy_actual).0.is_empty());
    }

    /// ftp 的目标键是原生 http_proxy：override 上有没有值都不该影响判断。
    #[test]
    fn ftp_proxy_compares_native_http_key() {
        let mut cfg = AppConfig::default();
        cfg.proxies
            .push(proxy_profile("p1", rcgui_profile::ProxyProtocol::Http, "rcgui", 1));
        cfg.bind_proxy_slot("r", "p1");
        let (desired, _) = desired_from(&cfg);

        // 原生键为空、override 上有旧值（老版本写的）→ 仍需写入原生键
        let stale_override = vec![state(
            "r",
            "ftp",
            ProxyConfigValues {
                override_http: Some("http://old:1".into()),
                ..Default::default()
            },
        )];
        assert_eq!(
            plan(&desired, &Actual::default(), &stale_override).0,
            vec![Action::SetProxy { remote: "r".into() }]
        );

        // 原生键已是期望值 → 无需动作
        let done = vec![state(
            "r",
            "ftp",
            ProxyConfigValues {
                native_http: Some("http://rcgui:1".into()),
                ..Default::default()
            },
        )];
        assert!(plan(&desired, &Actual::default(), &done).0.is_empty());
    }

    /// 用户自己设了原生代理：plan 仍然给出动作，让 apply 判定并报告「只补空缺」。
    #[test]
    fn ftp_existing_user_proxy_is_reported_by_apply() {
        let mut cfg = AppConfig::default();
        cfg.proxies
            .push(proxy_profile("p1", rcgui_profile::ProxyProtocol::Http, "rcgui", 1));
        cfg.bind_proxy_slot("r", "p1");
        let (desired, _) = desired_from(&cfg);
        let proxy_actual = vec![state(
            "r",
            "ftp",
            ProxyConfigValues {
                native_http: Some("http://user:3128".into()),
                ..Default::default()
            },
        )];
        assert_eq!(
            plan(&desired, &Actual::default(), &proxy_actual).0,
            vec![Action::SetProxy { remote: "r".into() }],
            "由 apply 返回 RespectExisting 并计入 skipped"
        );
    }

    /// 「不代理」+ 有脚印 → 应产生回滚动作（还原哪个键由 apply 用记录决定）。
    #[test]
    fn direct_binding_with_footprint_rolls_back() {
        let mut cfg = AppConfig::default();
        cfg.proxy_bindings
            .push(binding_with_footprint("r", "override.http_proxy", "http://old:1", None));
        let (desired, _) = desired_from(&cfg);
        let proxy_actual = vec![state(
            "r",
            "s3",
            ProxyConfigValues {
                override_http: Some("http://old:1".into()),
                ..Default::default()
            },
        )];
        let (acts, _) = plan(&desired, &Actual::default(), &proxy_actual);
        assert_eq!(acts, vec![Action::UnsetProxy { remote: "r".into() }]);
    }

    /// 「不代理」但从未下发过：rclone 里的值不归我们管，一个动作都不能有。
    #[test]
    fn direct_binding_without_footprint_is_noop() {
        let mut cfg = AppConfig::default();
        cfg.proxy_bindings.push(RemoteProxyBinding {
            remote: "r".into(),
            proxy_id: String::new(),
            ..Default::default()
        });
        let (desired, _) = desired_from(&cfg);
        let proxy_actual = vec![state(
            "r",
            "s3",
            ProxyConfigValues {
                override_http: Some("http://user:3128".into()),
                ..Default::default()
            },
        )];
        assert!(plan(&desired, &Actual::default(), &proxy_actual).0.is_empty());
    }

    /// 采集失败（没有现状）时不能猜，也不能产生动作。
    #[test]
    fn missing_proxy_state_is_skipped() {
        let mut cfg = AppConfig::default();
        cfg.proxies
            .push(proxy_profile("p1", rcgui_profile::ProxyProtocol::Http, "h", 1));
        cfg.bind_proxy_slot("r", "p1");
        let (desired, _) = desired_from(&cfg);
        assert!(plan(&desired, &Actual::default(), &[]).0.is_empty());
    }

    /// 原生 socks 凭据无法表达时，plan 要把原因当成错误报出来，而不是静默。
    #[test]
    fn unrepresentable_native_socks_is_reported() {
        let mut cfg = AppConfig::default();
        let mut p = proxy_profile("p1", rcgui_profile::ProxyProtocol::Socks5, "h", 1080);
        p.user = "a@b".into();
        cfg.proxies.push(p);
        cfg.bind_proxy_slot("r", "p1");
        let (desired, _) = desired_from(&cfg);
        let proxy_actual = vec![state("r", "ftp", ProxyConfigValues::default())];
        let (acts, errs) = plan(&desired, &Actual::default(), &proxy_actual);
        assert!(acts.is_empty());
        assert!(errs.iter().any(|e| e.contains("代理配置无效")));
    }

    #[test]
    fn dangling_binding_reports_error_and_clears() {
        let mut cfg = AppConfig::default();
        cfg.bind_proxy_slot("r", "gone-id");
        let (desired, errs) = desired_from(&cfg);
        assert_eq!(desired.remotes.len(), 1);
        assert!(desired.remotes[0].proxy.is_none(), "悬空引用按不代理处理");
        assert!(errs.iter().any(|e| e.contains("已不存在")));
    }

    /// 悬空引用但还留着脚印：仍要回滚，不能把旧代理留在 rclone 里。
    #[test]
    fn dangling_binding_with_footprint_still_rolls_back() {
        let mut cfg = AppConfig::default();
        cfg.proxy_bindings
            .push(binding_with_footprint("r", "override.http_proxy", "http://old:1", None));
        cfg.proxy_bindings[0].proxy_id = "gone".into();
        let (desired, _) = desired_from(&cfg);
        let proxy_actual = vec![state(
            "r",
            "s3",
            ProxyConfigValues {
                override_http: Some("http://old:1".into()),
                ..Default::default()
            },
        )];
        let (acts, _) = plan(&desired, &Actual::default(), &proxy_actual);
        assert_eq!(acts, vec![Action::UnsetProxy { remote: "r".into() }]);
    }

    #[test]
    fn report_summary_messages() {
        let mut r = ReconcileReport::default();
        assert!(r.is_empty());
        assert!(r.summary().contains("无需重载"));
        r.changed = 2;
        r.done.push("x".into());
        assert!(r.summary().contains("已重载 2 项"));
        r.skipped.push("s".into());
        assert!(r.summary().contains("1 项跳过"));
        r.failed.push("y".into());
        assert!(r.summary().contains("1 项失败"));
        assert!(r.detail().starts_with('y'), "失败排在跳过之前");
    }
}
