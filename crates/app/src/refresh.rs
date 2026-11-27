//! 取一次 rclone 状态并投影到界面 —— **唯一的更新入口**。
//!
//! 设计见 `state.rs` 的模块头。这里只做三件事：
//!
//! 1. 一次取齐 remote / mount / serve / stats / version（并发，各自超时）；
//! 2. 用 `Hub` 的世代号判断这份数据还能不能用（乱序、或取数期间用户动了手 → 丢弃）；
//! 3. 内容没变就不写 UI；变了就**从同一份快照**同时更新计数与三个列表。
//!
//! 第 3 点是这次修 bug 的关键：旧实现里「挂载 N」来自 `poll_stats` 的
//! `mount/listmounts`，而挂载页列表来自另一条 `MountsRefresh`，两者节奏不同、
//! 也会互相覆盖，于是出现「盘已挂上但仪表盘显示 0」。

use crate::state::{Fetched, MountState, RemoteState, ServeState, Snapshot, StatsState, XferState};
use crate::{fmt_bytes, mount_key, Ctx, MainWindow, MountRow, MountsG, ServeRow, ServesG};
use slint::{ComponentHandle as _, Model as _};
use std::sync::Arc;
use std::time::Duration;

/// 单次取数的超时。rclone 卡住时快速放弃，而不是把轮询永久占住。
const FETCH_TIMEOUT: Duration = Duration::from_secs(12);

/// 用量（`operations/about`）的刷新间隔：它要走网络，不能跟着 1s 轮询跑。
const USAGE_TTL: Duration = Duration::from_secs(30);

/// 取一次完整状态。任何一个子项失败都只影响它自己（其余照常更新）。
async fn fetch(ctx: &Arc<Ctx>) -> Fetched {
    let client = ctx.rc.client().await;
    let mut out = Fetched::default();

    // 并发取：串行的话一次慢请求会拖住后面全部子项。
    let (mounts, serves, remotes, stats, ver) = tokio::join!(
        async {
            tokio::time::timeout(Duration::from_secs(10), client.mounts())
                .await
                .ok()
                .and_then(|r| r.ok())
        },
        async {
            tokio::time::timeout(Duration::from_secs(10), client.serve_list())
                .await
                .ok()
                .and_then(|r| r.ok())
        },
        async {
            let names = tokio::time::timeout(Duration::from_secs(10), client.list_remotes())
                .await
                .ok()
                .and_then(|r| r.ok())?;
            let dump = tokio::time::timeout(Duration::from_secs(10), client.config_dump())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_else(|| serde_json::json!({}));
            Some((names, dump))
        },
        async {
            tokio::time::timeout(Duration::from_secs(10), client.core_stats())
                .await
                .ok()
                .and_then(|r| r.ok())
        },
        async {
            tokio::time::timeout(Duration::from_secs(10), client.version())
                .await
                .ok()
                .and_then(|r| r.ok())
        },
    );

    if let Some(list) = mounts {
        out.mounts = Some(
            list.into_iter()
                .map(|m| MountState {
                    fs: m.fs,
                    mount_point: m.mount_point,
                })
                .collect(),
        );
    }
    if let Some(v) = serves {
        out.serves = Some(parse_serves(&v));
    }
    if let Some((names, dump)) = remotes {
        // 列表里显示的类型要和编辑器用同一个判据：都看参数里的标记键，而标记键
        // 有没有实现取决于**当前有没有插件声明那个类型**。插件没装 / 没启用时
        // 这里查不到人，带标记的远程就老实显示成它真实的后端名，和编辑器里看到的
        // 一致 —— 两处各说一套，用户会以为自己的远程被改坏了。
        let derived = crate::remote_types::collect(ctx);
        out.remotes = Some(
            names
                .into_iter()
                .map(|n| {
                    let entry = dump.get(&n);
                    let stored = entry
                        .and_then(|e| e.get("type"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("?")
                        .to_string();
                    // 这里要的是显示名（编辑器那边要的是 id）。
                    let rtype = match entry.and_then(|e| e.as_object()) {
                        Some(params) => {
                            match crate::remote_types::owner_of_params(&derived, params) {
                                Some(d) => d.name.clone(),
                                None => stored,
                            }
                        }
                        None => stored,
                    };
                    RemoteState {
                        name: n,
                        rtype,
                        usage: "—".into(),
                    }
                })
                .collect(),
        );
    }
    if let Some(s) = stats {
        out.stats = Some(project_stats(ctx, &s));
    }
    if let Some(v) = ver {
        out.version = Some(v.version);
        out.os_info = Some(format!("{} / {}", v.os, v.arch));
    }
    out
}

/// `core/stats` → 界面用的投影（含瞬时速度换算）。
///
/// rclone 的 `speed` 是「本次会话平均值」，空闲时几乎不动，因此用累计字节差 / 时间差
/// 算瞬时速度；真正空闲时归零。
fn project_stats(ctx: &Arc<Ctx>, s: &rcgui_rc::CoreStats) -> StatsState {
    let idle = s.transferring.is_empty();
    let now = std::time::Instant::now();
    let inst = {
        let mut g = ctx.speed_meter.lock().unwrap();
        let inst = match *g {
            Some((last_bytes, last_at)) => {
                let dt = now.duration_since(last_at).as_secs_f64();
                let db = (s.bytes - last_bytes).max(0.0);
                if dt >= 0.2 {
                    db / dt
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        *g = Some((s.bytes, now));
        inst
    };
    let speed = if idle && inst < 1.0 {
        0.0
    } else if inst > 0.0 {
        inst
    } else {
        s.speed
    };
    StatsState {
        speed,
        bytes: s.bytes,
        errors: s.errors,
        transfers: s.transferring.len(),
        xfers: s
            .transferring
            .iter()
            .map(|t| XferState {
                name: t.name.clone(),
                size: t.bytes,
                speed: t.speed,
                percent: t.percentage.clamp(0.0, 100.0),
            })
            .collect(),
    }
}

/// `serve/list` → 运行中的服务。结构在不同版本间有差异，沿用既有的宽容取值方式。
fn parse_serves(v: &serde_json::Value) -> Vec<ServeState> {
    let items = v
        .get("list")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter(|it| {
            // 只保留运行中的服务（已停止的不留在列表里）
            it.get("running").and_then(|x| x.as_bool()).unwrap_or(true)
        })
        .map(|it| {
            let id = it
                .get("id")
                .and_then(|x| {
                    x.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| x.as_i64().map(|n| n.to_string()))
                })
                .unwrap_or_default();
            let params = it.get("params").cloned().unwrap_or(serde_json::json!({}));
            let getp = |keys: &[&str]| -> String {
                for k in keys {
                    if let Some(v) = params.get(*k).and_then(|x| x.as_str()) {
                        return v.to_string();
                    }
                    if let Some(v) = it.get(*k).and_then(|x| x.as_str()) {
                        return v.to_string();
                    }
                }
                String::new()
            };
            ServeState {
                id,
                // 协议位于 params.type（顶层没有 type 字段）
                stype: getp(&["type", "Type"]),
                fs: getp(&["fs", "Fs"]),
                addr: getp(&["addr", "Addr", "listen", "Listen"]),
            }
        })
        .collect()
}

/// 把「正在挂载中」的条目合进真实列表；已被真实列表收录的自动摘掉。
///
/// 必须用 `mount_key` 归一化比较：pending 里存的是 `T:\`，rclone 回报的是 `T:`，
/// 直接字符串比较永远不相等，界面上就会出现两条同盘符的行。
fn merge_pending(ctx: &Arc<Ctx>, mounts: &[MountState]) -> Vec<MountRow> {
    let mut rows: Vec<MountRow> = mounts
        .iter()
        .map(|m| MountRow {
            fs: m.fs.clone().into(),
            mount_point: m.mount_point.clone().into(),
            pending: false,
        })
        .collect();
    let mut pending = ctx.pending_mounts.lock().unwrap();
    pending.retain(|(_, pt)| {
        !rows
            .iter()
            .any(|r| mount_key(&r.mount_point) == mount_key(pt))
    });
    for (fs, pt) in pending.iter() {
        rows.push(MountRow {
            fs: fs.clone().into(),
            mount_point: pt.clone().into(),
            pending: true,
        });
    }
    rows
}

/// 界面要显示的「当前连接」。
///
/// 地址取自**实际握手的端点**（`RcState::endpoint()`），不是设置里存的那条字符串：
/// 受管内核每次启动都用随机端口和临时口令，保存的地址在托管模式下从来不是正在用的
/// 那个。`managed` 同理取自「内核是不是本进程拉起来的」，不是常量。
///
/// 之所以单独成一个值：这两项以前各自就近取，一处读了保存的配置、一处写死
/// `false`，于是仪表盘长期显示「外部 · http://127.0.0.1:5572」——哪怕真实连接是
/// 本机受管内核的 54283，并且页面上还显示着刚跑成功的同步。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionView {
    /// 实际使用的 RC 地址，形如 `http://127.0.0.1:54283`
    pub url: String,
    /// 这个地址对应的 rclone 是否由本进程启动并持有
    pub managed: bool,
}

impl ConnectionView {
    /// `core_running` 来自 `Maintenance::core.is_running()`，即「我在管着它」，
    /// 而不是「我连上了它」——外部模式下这句永远是 `false`。
    pub fn from_live(endpoint: &rcgui_rc::Endpoint, core_running: bool) -> Self {
        Self {
            url: endpoint.url.clone(),
            managed: core_running,
        }
    }
}

/// 读一次实际连接状态。`RcState::endpoint()` 记录的是上次握手成功的地址，
/// 与设置里保存的「外部地址」是两回事，后者只在外部模式下才等于实际值。
async fn live_connection(ctx: &Arc<Ctx>) -> ConnectionView {
    let endpoint = ctx.rc.endpoint().await;
    ConnectionView::from_live(&endpoint, ctx.maint.core.is_running())
}

/// 取一次并更新界面。
///
/// - `force = false`：内容没变就跳过重绘（轮询走这条）。
/// - `force = true`：必定重绘，并顺带刷新远程用量（用户操作 / 切页走这条）。
///
/// 返回是否真的写入了 UI。
pub async fn refresh_state(ctx: &Arc<Ctx>, ui: &slint::Weak<MainWindow>, force: bool) -> bool {
    if !ctx.rc.is_live() {
        // 离线：保留上次快照（指南 §3），不发请求、不清空列表
        return false;
    }
    let guard = ctx.hub.begin_fetch(ctx.rc.epoch());
    // 只看当前快照里是否已有数据，用于给用量字段做「沿用上次」的默认值
    let prev = ctx.hub.applied();

    let mut fetched = tokio::time::timeout(FETCH_TIMEOUT, fetch(ctx))
        .await
        .unwrap_or_default();

    // 用量是异步补的：没取到新值时沿用上次同名远程的文本，避免表格闪成「—」
    if let Some(remotes) = fetched.remotes.as_mut() {
        if let Some(prev) = prev.as_ref() {
            for r in remotes.iter_mut() {
                if let Some(old) = prev.remotes.iter().find(|p| p.name == r.name) {
                    if !old.usage.is_empty() {
                        r.usage = old.usage.clone();
                    }
                }
            }
        }
    }

    let snapshot: Snapshot = fetched.merge_into(prev.as_ref());

    // 世代号：取数期间用户动了手、已有更新的结果提交过，或内核被换掉（端点世代变了）
    // → 这份数据不能用来画界面
    if !guard.commit(ctx.rc.epoch()) {
        return false;
    }
    let changed = ctx.hub.note_applied(&snapshot, force);
    if !changed {
        return false;
    }

    // ---- 计数与列表同源：全部从这一份 snapshot 投影 ----
    let mount_rows = merge_pending(ctx, &snapshot.mounts);
    // 地址与「谁持有这个 rclone」都取自实际状态，不读设置里保存的那条：
    // 托管内核每次启动都换随机端口与临时口令，保存值在托管模式下永远不是正在用的那个。
    let conn = live_connection(ctx).await;
    let url = conn.url.clone();
    let managed = conn.managed;
    let running = snapshot.version.starts_with('v');

    // 远程行：沿用既有的筛选函数（它会读搜索框），因此先给缓存赋值再调用
    let remotes_full: Vec<(String, String, String)> = snapshot
        .remotes
        .iter()
        .map(|r| (r.name.clone(), r.rtype.clone(), r.usage.clone()))
        .collect();
    *ctx.remotes_cache.lock().unwrap() = remotes_full.clone();
    *ctx.mounts_cache.lock().unwrap() = mount_rows.clone();

    // 服务行（显示名要按档案映射，所以在这里解析）
    let serve_rows: Vec<ServeRow> = snapshot
        .serves
        .iter()
        .map(|s| ServeRow {
            id: s.id.clone().into(),
            name: crate::ensure_serve_name(ctx, &s.stype, &s.fs, &s.addr).into(),
            stype: s.stype.clone().into(),
            fs: s.fs.clone().into(),
            addr: s.addr.clone().into(),
        })
        .collect();
    *ctx.serves_cache.lock().unwrap() = serve_rows.clone();

    // 挂载页的「实例不匹配」提示：列表为空但本机确有 rclone 盘符时才有内容
    let hint = ctx.drives.mismatch_hint(&url, &snapshot.mounts);

    let stats = snapshot.stats.clone();
    let version = snapshot.version.clone();
    let os_info = snapshot.os_info.clone();
    let remotes_n = remotes_full.len();
    let pending_n = mount_rows.iter().filter(|r| r.pending).count();
    // 计数与列表同源：列表里有几行（含正在挂载的 pending），这里就是几
    let mounts_n = snapshot.mount_count(pending_n);
    let serves_n = snapshot.serve_count();
    let version_ui = version.clone();
    let os_info_ui = os_info.clone();
    let url_ui = url.clone();
    let hint_ui = hint.clone();

    let _ = ui.upgrade_in_event_loop(move |ui| {
        // 仪表盘 / 状态栏：数值全部来自同一份快照
        let d = ui.global::<crate::DashboardG>();
        d.set_version(version_ui.clone().into());
        d.set_os_info(os_info_ui.clone().into());
        d.set_remotes(remotes_n as i32);
        d.set_mounts(mounts_n as i32);
        d.set_serves(serves_n as i32);
        d.set_errors(stats.errors as i32);
        d.set_transfers(stats.transfers as i32);
        d.set_speed(format!("{}/s", fmt_bytes(stats.speed)).into());
        d.set_total_bytes(fmt_bytes(stats.bytes).into());
        d.set_running(running);
        d.set_managed(managed);
        d.set_url(url_ui.clone().into());
        let rows: Vec<crate::XferRow> = stats
            .xfers
            .iter()
            .map(|x| crate::XferRow {
                name: x.name.clone().into(),
                size: fmt_bytes(x.size).into(),
                speed: format!("{}/s", fmt_bytes(x.speed)).into(),
                percent: x.percent as i32,
            })
            .collect();
        d.set_xfers(slint::ModelRc::from(std::rc::Rc::new(
            slint::VecModel::from(rows),
        )));

        // 「实例不匹配」提示：与列表同一份快照算出来
        ui.global::<MountsG>()
            .set_instance_hint(hint_ui.clone().into());
    });

    // 三个列表都走既有的「筛选」实现：搜索框里的词必须继续生效，
    // 否则 1s 轮询会把用户过滤后的列表又刷回全量。
    crate::apply_remotes_filter(ui, &remotes_full).await;
    crate::apply_mounts_filter(ui, &mount_rows).await;
    crate::apply_serves_filter(ui, &serve_rows).await;

    // 档案里的远程列表同步一份（离线也能看到「有哪些远程」）
    crate::profiles::set_remotes(&remotes_full);

    // 仓库下拉框（挂载页 / 服务页共用）
    let names_ui: Vec<slint::SharedString> = remotes_full
        .iter()
        .map(|(n, _, _)| n.as_str().into())
        .collect();
    let names_ui2 = names_ui.clone();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.global::<MountsG>()
            .set_remotes(slint::ModelRc::from(std::rc::Rc::new(
                slint::VecModel::from(names_ui.clone()),
            )));
        ui.global::<ServesG>()
            .set_remotes(slint::ModelRc::from(std::rc::Rc::new(
                slint::VecModel::from(names_ui2.clone()),
            )));
    });

    // 用量：只在这几种情况下才去打网络（about 会真的连后端，不能跟 1s 轮询同频）
    if usage_due(ctx) {
        spawn_usage(ctx, ui, &remotes_full).await;
    }
    true
}

/// 用量刷新节流：距上次抓取超过 `USAGE_TTL` 才允许再抓。
fn usage_due(ctx: &Arc<Ctx>) -> bool {
    let mut g = ctx.usage_at.lock().unwrap();
    match *g {
        Some(at) if at.elapsed() < USAGE_TTL => false,
        _ => {
            *g = Some(std::time::Instant::now());
            true
        }
    }
}

/// 异步补每个远程的用量（不阻塞快照：它只是表格里一列的文本）。
async fn spawn_usage(
    ctx: &Arc<Ctx>,
    ui: &slint::Weak<MainWindow>,
    remotes: &[(String, String, String)],
) {
    let client = match tokio::time::timeout(Duration::from_secs(3), ctx.rc.client()).await {
        Ok(c) => c,
        Err(_) => return,
    };
    for (name, _, _) in remotes {
        let client = client.clone();
        let ctx2 = ctx.clone();
        let ui2 = ui.clone();
        let n = name.clone();
        tokio::spawn(async move {
            // 插件不在的远程连用量都不去问：问成功了会给表格填上一个数字，
            // 看起来就像这远程还能用，点进去却全被拒。
            if crate::remote_blocked(&ctx2, &n).await.is_some() {
                return;
            }
            let usage =
                match tokio::time::timeout(Duration::from_secs(15), client.about(&format!("{n}:")))
                    .await
                {
                    Ok(Ok(v)) => {
                        let total = v.get("total").and_then(|x| x.as_f64());
                        let used = v.get("used").and_then(|x| x.as_f64());
                        match (total, used) {
                            (Some(t), Some(u)) => format!("{} / {}", fmt_bytes(u), fmt_bytes(t)),
                            (None, Some(u)) => rcgui_i18n::f("已用 {}", &[&(fmt_bytes(u))]),
                            _ => "—".into(),
                        }
                    }
                    // 后端不支持 about（部分网盘）或慢：保持「—」，不要报错打扰用户
                    _ => return,
                };
            {
                let mut cache = ctx2.remotes_cache.lock().unwrap();
                if let Some(e) = cache.iter_mut().find(|(cn, _, _)| cn == &n) {
                    e.2 = usage.clone();
                }
            }
            // 用量只改了表格里的一列：就地更新，不重画整份快照
            let _ = ui2.upgrade_in_event_loop(move |ui| {
                let g = ui.global::<crate::RemotesG>();
                let rows = g.get_rows();
                for i in 0..rows.row_count() {
                    if let Some(mut r) = rows.row_data(i) {
                        if r.name.as_str() == n.as_str() {
                            r.usage = usage.clone().into();
                            rows.set_row_data(i, r);
                        }
                    }
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(url: &str) -> rcgui_rc::Endpoint {
        rcgui_rc::Endpoint {
            url: url.to_string(),
            user: Some("gui".to_string()),
            has_pass: true,
        }
    }

    /// 受管内核：显示的必须是它自己的端口，而不是设置里保存的那条。
    ///
    /// 这条守的是实测到的缺陷——受管内核在 54283，仪表盘却一直写
    /// 「外部 · http://127.0.0.1:5572」，因为地址读了保存的配置、`managed` 写死 false。
    #[test]
    fn managed_core_shows_its_own_endpoint() {
        let v = ConnectionView::from_live(&endpoint("http://127.0.0.1:54283"), true);
        assert_eq!(v.url, "http://127.0.0.1:54283");
        assert!(v.managed, "本进程拉起来的内核必须显示成托管");
    }

    /// 外部模式：地址不归我们管，`managed` 必须是 false。
    #[test]
    fn external_endpoint_is_not_managed() {
        let v = ConnectionView::from_live(&endpoint("http://127.0.0.1:5572"), false);
        assert_eq!(v.url, "http://127.0.0.1:5572");
        assert!(!v.managed);
    }

    /// 保存的地址不能参与显示：设置里留着的 5572 不能顶替真实端点。
    #[test]
    fn saved_url_never_replaces_the_live_endpoint() {
        let saved = "http://127.0.0.1:5572";
        let v = ConnectionView::from_live(&endpoint("http://127.0.0.1:54283"), true);
        assert_ne!(v.url, saved);
        assert_eq!(v.url, "http://127.0.0.1:54283");
    }
}
