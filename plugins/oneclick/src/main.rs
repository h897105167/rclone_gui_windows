//! 一键 rclone：检测 / 安装 / 启动本机 rclone（rcd），并把连接参数交给宿主。
//!
//! # 这个插件是什么
//!
//! 「内置插件」的样板。它和用户自己装的插件在磁盘上完全同构
//! （`config.json` + `check.sha256` + `main.wasm`），因此同样可以被卸载；
//! 卸载后还能通过插件页的「恢复内置」重新释放。
//!
//! # 为什么它不直接碰操作系统
//!
//! WASI 里没有进程、没有网络、没有文件对话框。所以选文件、下载、解压、
//! 算哈希、起进程这些动作全部通过 `need` 向宿主申请通用能力
//! （`fs.*` / `http.*` / `proc.*` / `net.*` / `env.info`），能用到哪几组在
//! `config.json` 的 `capabilities` 里声明，越权的调用会被宿主直接拒绝。
//!
//! 插件里剩下的只有**策略**：装哪个版本、装到哪里、怎么校验、什么时候启动。
//!
//! # 一次调用 = 一个全新进程
//!
//! 每轮都是「stdin 一个 JSON、stdout 一个 JSON、退出」，所以跨轮状态只能靠
//! `state`（宿主原样回传）与 kv 持久化。宿主对单次调用最多允许 16 轮协商，
//! 因此每个动作都要把能并行的能力一次问完。
//!
//! # 面板协议
//!
//! 回包 `body` 里用到的字段：`status` / `lamp` / `checks` / `hints` /
//! `enabled` / `values` / `toast` / `rc`。含义见 `crates/app/src/plugins.rs`
//! 的模块文档。

use serde_json::{json, Map, Value};
use std::io::{Read, Write};

// ---------------- 常量（与上一代内置实现保持一致） ----------------

const OFFICIAL_BASE: &str = "https://downloads.rclone.org";

/// 实测：清华镜像没有 rclone 目录（`/rclone/version.txt` 返回 404），
/// 所以按顺序回退到南大镜像。保留它是因为镜像站偶尔会补上。
const MIRRORS: &[(&str, &str)] = &[
    ("清华镜像", "https://mirrors.tuna.tsinghua.edu.cn/rclone"),
    ("南京大学镜像", "https://mirrors.nju.edu.cn/rclone"),
];

const EXTRA_REPO_API: &str = "https://api.github.com/repos/gulp79/rclone-extra/releases/latest";

const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;
const MAX_EXTRACTED: u64 = 600 * 1024 * 1024;

/// 真 rclone.exe 至少 4 MB（单文件静态编译），超过 300 MB 基本是下错了东西。
const MIN_EXE_SIZE: u64 = 4 * 1024 * 1024;
const MAX_EXE_SIZE: u64 = 300 * 1024 * 1024;

const DEFAULT_PORT: u16 = 5572;

// ---------------- 入口 ----------------

fn main() {
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return;
    }
    let resp = handle(&input).unwrap_or_else(|e| json!({ "id": 1, "error": { "message": e } }));
    let mut out = std::io::stdout();
    let _ = out.write_all(&serde_json::to_vec(&resp).unwrap_or_default());
    let _ = out.flush();
}

fn handle(input: &[u8]) -> Result<Value, String> {
    let req: Value = serde_json::from_slice(input).map_err(|e| format!("请求解析失败: {e}"))?;
    let id = req.get("id").and_then(|v| v.as_u64()).unwrap_or(1);
    let method = s(req.get("method"));
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    match method.as_str() {
        "manifest" => Ok(json!({
            "id": id,
            "manifest": {
                "id": "onerclone",
                "name": "一键 rclone",
                "version": env!("CARGO_PKG_VERSION"),
                "description": "检测 / 安装 / 启动本机 rclone（rcd），并自动连接"
            }
        })),
        "request" => Ok(dispatch(id, &params)),
        other => Err(format!("未知 method: {other}")),
    }
}

/// 一次 `request` 的输入。
struct Rq {
    form: Value,
    provided: Value,
    state: Value,
}

fn dispatch(id: u64, p: &Value) -> Value {
    let action = s(p.get("action"));
    let rq = Rq {
        form: p.get("form").cloned().unwrap_or_else(|| json!({})),
        provided: p.get("provided").cloned().unwrap_or(Value::Null),
        state: p.get("state").cloned().unwrap_or(Value::Null),
    };
    match action.as_str() {
        // 面板打开 / 动作完成后刷新：只读，不产生副作用
        "state" => on_state(id, &rq),
        // 选一个 rclone.exe 并校验
        "pick_exe" => on_pick(id, &rq),
        // 自动搜索本机 rclone.exe（快扫；一个都没找到时把决定权交回用户）
        "search_exe" => on_search(id, &rq),
        // 快扫没找到时的「深度扫描」（用户点了才做，避免一次点击就翻全盘）
        "search_deep" => on_search_deep(id),
        // 从历史记录里选一个路径（右侧 ▾ 下拉）
        "pick_history" => on_pick_history(id, &rq),
        // 重新校验当前路径
        "revalidate" => on_revalidate(id, &rq),
        // 下载 / 解压 / 校验 / 安装
        "install" => on_install(id, &rq),
        // 启动 rcd
        "start" => on_start(id, &rq, false),
        // 开机自启：与 start 同一套流程，失败不打扰用户
        "autostart" => on_start(id, &rq, true),
        // 停止
        "stop" | "shutdown" => on_stop(id, &rq),
        other => fail(id, &format!("未知动作: {other}")),
    }
}

// ---------------- 协议小工具 ----------------

fn s(v: Option<&Value>) -> String {
    v.and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn step_of(state: &Value) -> String {
    state.get("step").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// 取本轮 `provided` 里的某个键；没有就是 Null。
fn got(provided: &Value, key: &str) -> Value {
    provided.get(key).cloned().unwrap_or(Value::Null)
}

/// 能力调用失败时宿主回的是 `{"_error": "..."}`，不是抛异常。
fn got_err(provided: &Value, key: &str) -> Option<String> {
    provided
        .get(key)
        .and_then(|v| v.get("_error"))
        .and_then(|v| v.as_str())
        .map(|m| m.to_string())
}

fn need(id: u64, state: Value, list: Vec<Value>) -> Value {
    json!({ "id": id, "need": list, "state": state })
}

fn done(id: u64, body: Value) -> Value {
    done_ops(id, Vec::new(), body)
}

fn done_ops(id: u64, ops: Vec<Value>, body: Value) -> Value {
    json!({ "id": id, "ops": ops, "result": { "status": 200, "body": body } })
}

/// 面向用户的失败：不抛错，而是回一个带 toast 的面板状态。
/// 这样插件页会刷新并立刻弹提示，比 `error` 字段只留下一条日志更有用。
fn fail(id: u64, msg: &str) -> Value {
    done(
        id,
        json!({
            "status": msg,
            "lamp": "err",
            "toast": { "level": "err", "message": msg },
            "enabled": { "start": false, "stop": false },
        }),
    )
}

/// 把一项数据塞进 `state` 带到下一轮。
fn keep(mut state: Value, key: &str, value: Value) -> Value {
    if let Some(o) = state.as_object_mut() {
        o.insert(key.to_string(), value);
    }
    state
}

fn state_s(state: &Value, key: &str) -> String {
    s(state.get(key))
}

// ---------------- 表单读取 ----------------

fn fstr(form: &Value, key: &str) -> String {
    match form.get(key) {
        Some(Value::String(v)) => v.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// 复选框在 kv 里存的是 `"true"` / `"false"` 字符串。
fn fbool(form: &Value, key: &str, dflt: bool) -> bool {
    match form.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(v)) => v == "true",
        _ => dflt,
    }
}

fn fport(form: &Value) -> u16 {
    fstr(form, "port")
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .unwrap_or(DEFAULT_PORT)
}

// ---------------- kv 里的插件私有状态 ----------------

fn kv_state(kv: &Value) -> Map<String, Value> {
    kv.get("_state")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

fn proc_handle(kv: &Value) -> String {
    s(kv_state(kv).get("proc").and_then(|p| p.get("handle")))
}

fn verified_version(kv: &Value) -> String {
    s(kv_state(kv).get("exe_version"))
}

/// 生成一条 `kv.set` op：合并写回 `_state`（保留宿主写入的 status / lamp / checks）。
fn state_op(kv: &Value, patch: impl FnOnce(&mut Map<String, Value>)) -> Value {
    let mut st = kv_state(kv);
    patch(&mut st);
    json!({ "op": "kv.set", "key": "_state", "value": Value::Object(st) })
}

// ---------------- state：面板刷新 ----------------

fn on_state(id: u64, rq: &Rq) -> Value {
    let st = step_of(&rq.state);
    let exe = fstr(&rq.form, "exe");
    if st.is_empty() {
        return need(
            id,
            json!({ "step": "read" }),
            vec![
                json!({ "cap": "kv.get_all", "key": "kv" }),
                json!({ "cap": "fs.meta", "key": "meta", "params": { "path": exe } }),
            ],
        );
    }
    let kv = got(&rq.provided, "kv");
    let meta = got(&rq.provided, "meta");
    if st == "read" {
        let handle = proc_handle(&kv);
        if !handle.is_empty() {
            // 要进第三轮就必须把 kv 一起带过去：第三轮的 provided 里只有 `proc`，
            // 这里只 keep meta 的话，`exe_history(&Null)` 会返回空数组，宿主就
            // 不再渲染路径下拉 —— 表现正是「rclone 在跑时历史不见了」。
            return need(
                id,
                keep(keep(json!({ "step": "proc" }), "meta", meta.clone()), "kv", kv),
                vec![json!({
                    "cap": "proc.status",
                    "key": "proc",
                    "params": { "handle": handle }
                })],
            );
        }
        return done(id, state_body(&rq.form, &kv, &meta, &Value::Null));
    }
    let meta = rq.state.get("meta").cloned().unwrap_or(meta);
    let kv = rq.state.get("kv").cloned().unwrap_or(kv);
    let proc = got(&rq.provided, "proc");
    done(id, state_body(&rq.form, &kv, &meta, &proc))
}

/// 合成面板状态：一行文字 + 一盏灯 + 校验清单 + 按钮可用性。
fn state_body(form: &Value, kv: &Value, meta: &Value, proc: &Value) -> Value {
    let exe = fstr(form, "exe");
    let running = proc.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
    let pid = proc.get("pid").and_then(|v| v.as_i64()).unwrap_or(0);
    let uptime = proc.get("uptime_ms").and_then(|v| v.as_i64()).unwrap_or(0);
    let checks = kv_state(kv).get("checks").cloned().unwrap_or(Value::Null);
    let exe_ok = meta.get("exists").and_then(|v| v.as_bool()).unwrap_or(false)
        && meta.get("is_file").and_then(|v| v.as_bool()).unwrap_or(false);

    let (status, lamp) = if running {
        (
            format!("正在运行（PID {pid} · 已运行 {}）", fmt_uptime(uptime)),
            "ok",
        )
    } else if exe.is_empty() {
        ("还没有指定 rclone.exe".to_string(), "err")
    } else if !exe_ok {
        ("指定的文件不存在".to_string(), "err")
    } else {
        ("未启动".to_string(), "warn")
    };

    let mut hints = Map::new();
    // 只有**用户名**为空才致命：`--rc-user gui`（不带 `--rc-pass`）时 rclone 照样
    // 开启鉴权，客户端发 `Basic gui:`（空密码）就能通过（已实测）。
    // 原来的条件把「密码留空」也算进去、文案也说成会拒绝连接，是错的，
    // 会把用户误导到勾「本地不校验」上，反而把实例变成谁都能操作。
    if !fbool(form, "no_auth", false) && fstr(form, "rc_user").is_empty() {
        hints.insert(
            "auth_warning".into(),
            json!(
                "⚠ 未填写用户名：这样启动的 rclone 会拒绝挂载 / 服务 / 参数读取等所有接口（403）。\
                 请填用户名（密码可留空），或勾选「本地不校验」"
            ),
        );
    }
    if verified_version(kv).is_empty() {
        hints.insert(
            "auto_start_hint".into(),
            json!("⚠ 自动启动需要先通过一次校验（点「重新校验」）"),
        );
    }

    let mut body = json!({
        "status": status,
        "lamp": lamp,
        // 路径历史：宿主的 path 字段会把它渲染成输入框右侧的 ▾ 下拉
        "choices": { "exe": exe_history(kv) },
        "enabled": {
            "start": !running && !exe.is_empty(),
            "stop": running,
            "revalidate": !exe.is_empty(),
            // 快扫没找到时，面板会显示「深度扫描」按钮
            "search_deep": kv_state(kv)
                .get("search_deep_offered")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        },
    });
    if !checks.is_null() {
        body["checks"] = checks;
    }
    if !hints.is_empty() {
        body["hints"] = Value::Object(hints);
    }
    body
}

fn fmt_uptime(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3600 {
        format!("{} 分钟", secs / 60)
    } else {
        format!("{} 小时 {} 分钟", secs / 3600, (secs % 3600) / 60)
    }
}

// ---------------- 校验 ----------------

/// 一次问齐五项证据：存在性 / PE 头 / 数字签名 / `rclone version` / 现有 kv。
///
/// kv 一并取回是为了**合并**写 `_state`：早期版本校验完成后直接
/// `kv.set _state {checks, exe_version}`，会把同一份 `_state` 里的 `proc`
/// （进程句柄）和 `exe_history`（路径历史）一起抹掉 —— 表现为「校验完就
/// 不知道 rclone 还在不在跑」。
fn verify_needs(exe: &str) -> Vec<Value> {
    vec![
        json!({ "cap": "kv.get_all", "key": "kv" }),
        json!({ "cap": "fs.meta", "key": "meta", "params": { "path": exe } }),
        json!({ "cap": "fs.pe_info", "key": "pe", "params": { "path": exe } }),
        json!({ "cap": "fs.signature", "key": "sig", "params": { "path": exe } }),
        json!({
            "cap": "proc.run",
            "key": "run",
            "params": { "program": exe, "args": ["version"], "timeout_ms": 20000 }
        }),
    ]
}

fn on_pick(id: u64, rq: &Rq) -> Value {
    let st = step_of(&rq.state);
    if st.is_empty() {
        return need(
            id,
            json!({ "step": "pick" }),
            vec![json!({
                "cap": "fs.pick_file",
                "key": "pick",
                "params": {
                    "title": "选择 rclone.exe",
                    "filters": ["exe"],
                    "start_dir": fstr(&rq.form, "exe")
                }
            })],
        );
    }
    if st == "pick" {
        let picked = s(got(&rq.provided, "pick").get("path"));
        if picked.is_empty() {
            return done(
                id,
                json!({
                    "status": "未选择文件",
                    "lamp": "warn",
                    "enabled": { "start": !fstr(&rq.form, "exe").is_empty(), "stop": false },
                }),
            );
        }
        return need(
            id,
            json!({ "step": "judge", "exe": picked }),
            verify_needs(&picked),
        );
    }
    // judge
    let exe = state_s(&rq.state, "exe");
    finish_verify(id, &rq.form, &exe, &rq.provided)
}

fn on_revalidate(id: u64, rq: &Rq) -> Value {
    let st = step_of(&rq.state);
    let exe = if st.is_empty() {
        fstr(&rq.form, "exe")
    } else {
        state_s(&rq.state, "exe")
    };
    if exe.is_empty() {
        return fail(id, "还没有设置 rclone.exe 路径");
    }
    if st.is_empty() {
        return need(
            id,
            json!({ "step": "judge", "exe": exe }),
            verify_needs(&exe),
        );
    }
    finish_verify(id, &rq.form, &exe, &rq.provided)
}

/// 校验结论 → 面板状态（并把结论持久化，供后续「自动启动」判断）。
fn finish_verify(id: u64, form: &Value, exe: &str, provided: &Value) -> Value {
    let v = judge(
        exe,
        &got(provided, "meta"),
        &got(provided, "pe"),
        &got(provided, "sig"),
        &got(provided, "run"),
    );
    let kv = got(provided, "kv");
    if !v.ok {
        let checks = json!(v.checks);
        let ops = vec![state_op(&kv, |s| {
            s.insert("checks".into(), checks);
        })];
        return done_ops(
            id,
            ops,
            json!({
                "status": format!("校验未通过：{}", v.detail),
                "lamp": "err",
                "checks": v.checks,
                "values": { "exe": exe },
                "choices": { "exe": exe_history(&kv) },
                "toast": { "level": "err", "message": format!("rclone 校验未通过：{}", v.detail) },
                "enabled": { "start": false, "stop": false, "revalidate": true },
            }),
        );
    }
    // 通过校验的路径进历史（用户要求：只有校验通过的才记，最多 10 条）
    let mut hist = exe_history(&kv);
    history_push(&mut hist, exe);
    let checks = json!(v.checks);
    let version = v.version.clone();
    let ops = vec![state_op(&kv, |s| {
        s.insert("checks".into(), checks);
        s.insert("exe_version".into(), json!(version));
        s.insert("exe_history".into(), json!(hist.clone()));
    })];
    done_ops(
        id,
        ops,
        json!({
            "status": format!("校验通过（rclone {}）", v.version),
            "lamp": "warn",
            "checks": v.checks,
            "values": { "exe": exe },
            "choices": { "exe": hist },
            "toast": { "level": "info", "message": format!("已通过校验：rclone {}", v.version) },
            "enabled": {
                "start": !exe.is_empty() && !form.is_null(),
                "stop": false,
                "revalidate": true,
            },
        }),
    )
}

// ---------------- search：自动搜索本机 rclone.exe ----------------

/// 历史路径上限（用户要求最多 10 条）。
const HISTORY_CAP: usize = 10;

/// 读取已记录的历史路径（最新的在前）。
fn exe_history(kv: &Value) -> Vec<String> {
    kv_state(kv)
        .get("exe_history")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 把一条路径放进历史：去重（大小写不敏感）、最新的在前、最多 10 条。
fn history_push(list: &mut Vec<String>, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    list.retain(|p| !p.eq_ignore_ascii_case(path));
    list.insert(0, path.to_string());
    list.truncate(HISTORY_CAP);
}

/// 搜索本机 rclone.exe。
///
/// 两阶段（用户选的方案 C）：
/// 1. 快扫（PATH + 常见安装目录 + 固定盘 3 层）；
/// 2. 快扫一个都没找到 → 返回 `need_deep`，让面板显示「深度扫描」按钮，
///    用户点了才做全盘（避免一次点击就把磁盘翻个底朝天）。
fn on_search(id: u64, rq: &Rq) -> Value {
    let st = step_of(&rq.state);

    if st.is_empty() {
        // 快扫：depth=3，预算 8 秒
        return need(
            id,
            json!({ "step": "scan" }),
            vec![
                json!({ "cap": "kv.get_all", "key": "kv" }),
                json!({
                    "cap": "fs.search_exe",
                    "key": "scan",
                    "params": { "name": "rclone.exe", "max_depth": 3, "timeout_ms": 8000 }
                }),
            ],
        );
    }
    if st == "scan" {
        let kv = got(&rq.provided, "kv");
        let paths = paths_of(got(&rq.provided, "scan"));
        if !paths.is_empty() {
            return finish_search(id, &rq.form, &kv, &paths);
        }
        // 快扫没找到：交给用户决定要不要深度扫描（方案 C）
        return done_ops(
            id,
            vec![state_op(&kv, |s| {
                s.insert("search_deep_offered".into(), json!(true));
            })],
            json!({
                "status": "快扫没有找到 rclone.exe",
                "lamp": "warn",
                "toast": {
                    "level": "warn",
                    "message": "快扫没有找到 rclone.exe，可以点「深度扫描」再找一次（较慢）"
                },
            }),
        );
    }
    // st == "deep"：全盘结果收尾（depth=6，预算 25 秒）
    let kv = got(&rq.provided, "kv");
    let paths = paths_of(got(&rq.provided, "scan"));
    if paths.is_empty() {
        return done(
            id,
            json!({
                "status": "没有找到 rclone.exe（已深度扫描）",
                "lamp": "err",
                "toast": { "level": "err", "message": "没有找到 rclone.exe，请手动选择路径" },
            }),
        );
    }
    finish_search(id, &rq.form, &kv, &paths)
}

/// 深度扫描的第一步（用户点了「深度扫描」按钮）：发一次更深的搜索。
fn on_search_deep(id: u64) -> Value {
    need(
        id,
        json!({ "step": "deep" }),
        vec![
            json!({ "cap": "kv.get_all", "key": "kv" }),
            json!({
                "cap": "fs.search_exe",
                "key": "scan",
                "params": { "name": "rclone.exe", "max_depth": 6, "timeout_ms": 25000 }
            }),
        ],
    )
}

fn paths_of(v: Value) -> Vec<String> {
    v.get("paths")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// 搜索结果收尾：第一条放进输入框（等待校验），其余先记进历史。
fn finish_search(id: u64, form: &Value, kv: &Value, paths: &[String]) -> Value {
    let first = paths.first().cloned().unwrap_or_default();
    let mut hist = exe_history(kv);
    for p in paths {
        history_push(&mut hist, p);
    }
    let _ = form;
    let mut body = json!({
        "values": { "exe": first },
        "status": format!("找到 {} 个候选，正在校验第一个…", paths.len()),
        "lamp": "warn",
        "choices": { "exe": hist.clone() },
    });
    if paths.len() > 1 {
        body["toast"] = json!({
            "level": "info",
            "message": format!("找到 {} 个 rclone.exe，已把其余 {} 个加入路径历史", paths.len(), paths.len() - 1)
        });
    }
    done_ops(
        id,
        vec![state_op(kv, |s| {
            s.insert("exe_history".into(), json!(hist));
        })],
        body,
    )
}

/// 从历史里选了一条：写回输入框并立即校验。
fn on_pick_history(id: u64, rq: &Rq) -> Value {
    let picked = fstr(&rq.form, "exe");
    if picked.trim().is_empty() {
        return fail(id, "历史里没有可用的路径");
    }
    need(
        id,
        json!({ "step": "judge", "exe": picked }),
        verify_needs(&picked),
    )
}

// ---------------- install：下载 / 解压 / 校验 / 安装 ----------------

fn on_install(id: u64, rq: &Rq) -> Value {
    let st = step_of(&rq.state);
    let form = &rq.form;
    let enhanced = fstr(form, "edition").starts_with("增强");
    let mirror_first = fstr(form, "source").starts_with("镜像");

    match st.as_str() {
        // 0) 一次问齐：环境信息 + 所有候选版本源（谁先成功用谁）
        "" => {
            let mut list = vec![json!({ "cap": "env.info", "key": "env" })];
            if enhanced {
                list.push(json!({
                    "cap": "http.json",
                    "key": "src0",
                    "params": { "url": EXTRA_REPO_API, "timeout_ms": 30000 }
                }));
            } else {
                for (i, (_name, base)) in official_bases(mirror_first).iter().enumerate() {
                    list.push(json!({
                        "cap": "http.text",
                        "key": format!("src{i}"),
                        "params": {
                            "url": format!("{}/version.txt", base.trim_end_matches('/')),
                            "timeout_ms": 20000
                        }
                    }));
                }
            }
            need(id, json!({ "step": "resolve" }), list)
        }
        // 1) 选定下载地址 → 建临时目录 + 下载
        "resolve" => {
            let env = got(&rq.provided, "env");
            let Some(env) = env.as_object() else {
                return fail(id, "宿主没有返回环境信息");
            };
            let arch = match rclone_arch(s(env.get("arch"))) {
                Ok(a) => a,
                Err(e) => return fail(id, &e),
            };
            let resolved = if enhanced {
                resolve_enhanced(&rq.provided, &arch)
            } else {
                resolve_official(&rq.provided, &arch, mirror_first)
            };
            let (url, version, label, notes) = match resolved {
                Ok(v) => v,
                Err(e) => return fail(id, &e),
            };
            let data_dir = s(env.get("data_dir"));
            let tmp = format!("{data_dir}/tmp/oneclick");
            let zip = format!("{tmp}/rclone.zip");
            let mut notes = notes;
            notes.extend(mirror_notes(&url));
            let state = json!({
                "step": "download",
                "url": url,
                "version": version,
                "label": label,
                "notes": notes,
                "tmp": tmp,
                "zip": zip,
                "arch": arch,
                "data_dir": data_dir,
                "localappdata": s(env.get("localappdata")),
                "programfiles": s(env.get("programfiles")),
                "elevated": env.get("elevated").and_then(|v| v.as_bool()).unwrap_or(false),
            });
            need(
                id,
                state,
                vec![
                    // need 按顺序执行：先清旧临时目录，再下载
                    json!({ "cap": "fs.remove", "key": "clean0", "params": { "path": tmp } }),
                    json!({
                        "cap": "fs.mkdir",
                        "key": "mkdir",
                        "params": { "path": tmp }
                    }),
                    json!({
                        "cap": "http.download",
                        "key": "dl",
                        "params": { "url": url, "dest": zip, "max_bytes": MAX_DOWNLOAD }
                    }),
                ],
            )
        }
        // 2) 解压
        "download" => {
            if let Some(e) = got_err(&rq.provided, "dl") {
                return fail(id, &format!("下载失败：{e}"));
            }
            let tmp = state_s(&rq.state, "tmp");
            let zip = state_s(&rq.state, "zip");
            let unpack = format!("{tmp}/unpack");
            let state = keep(keep(rq.state.clone(), "step", json!("unzip")), "unpack", json!(unpack));
            need(
                id,
                state,
                vec![json!({
                    "cap": "fs.unzip",
                    "key": "unz",
                    "params": { "zip": zip, "dest": unpack, "max_bytes": MAX_EXTRACTED }
                })],
            )
        }
        // 3) 在解压结果里找 rclone.exe
        "unzip" => {
            if let Some(e) = got_err(&rq.provided, "unz") {
                return fail(id, &format!("解压失败：{e}"));
            }
            let unpack = state_s(&rq.state, "unpack");
            let state = keep(rq.state.clone(), "step", json!("find"));
            need(
                id,
                state,
                vec![json!({
                    "cap": "fs.find",
                    "key": "find",
                    "params": { "dir": unpack, "name": "rclone.exe", "max_depth": 4 }
                })],
            )
        }
        // 4) 校验解压出来的文件
        "find" => {
            let found = s(got(&rq.provided, "find").get("path"));
            if found.is_empty() {
                return fail(id, "压缩包里没有 rclone.exe（可能下载到了错误页面）");
            }
            let state = keep(
                keep(rq.state.clone(), "step", json!("judge")),
                "unpacked",
                json!(found),
            );
            need(id, state, verify_needs(&found))
        }
        // 5) 校验结论 → 复制到安装目录
        "judge" => {
            let exe = state_s(&rq.state, "unpacked");
            let v = judge(
                &exe,
                &got(&rq.provided, "meta"),
                &got(&rq.provided, "pe"),
                &got(&rq.provided, "sig"),
                &got(&rq.provided, "run"),
            );
            if !v.ok {
                return fail(id, &format!("下载到的文件未通过校验：{}", v.detail));
            }
            let version = {
                let v2 = state_s(&rq.state, "version");
                if v.version.is_empty() {
                    v2
                } else {
                    v.version
                }
            };
            let (root, fallback) = install_roots(&rq.state);
            let target = format!("{root}/{}", safe_dir_name(&version));
            let state = json!({
                "step": "copy",
                "exe": exe,
                "version": version,
                "target": target,
                "fallback": fallback,
                "tmp": state_s(&rq.state, "tmp"),
                "label": state_s(&rq.state, "label"),
                "label_notes": rq.state.get("notes").cloned().unwrap_or(json!([])),
                "root": root,
            });
            need(
                id,
                state,
                vec![json!({
                    "cap": "fs.copy",
                    "key": "cp",
                    "params": { "from": exe, "to": format!("{target}/rclone.exe") }
                })],
            )
        }
        // 6) 复制结果（系统目录写不进时退回用户目录）
        "copy" => {
            if let Some(e) = got_err(&rq.provided, "cp") {
                let fallback = state_s(&rq.state, "fallback");
                if fallback.is_empty() {
                    return fail(id, &format!("安装失败：{e}"));
                }
                let target = format!("{fallback}/{}", safe_dir_name(&state_s(&rq.state, "version")));
                let exe = state_s(&rq.state, "exe");
                let state = keep(
                    keep(rq.state.clone(), "step", json!("copy2")),
                    "target",
                    json!(target),
                );
                let state = keep(state, "copy_note", json!(format!("系统目录不可写（{e}），已改为用户目录")));
                return need(
                    id,
                    state,
                    vec![json!({
                        "cap": "fs.copy",
                        "key": "cp2",
                        "params": { "from": exe, "to": format!("{target}/rclone.exe") }
                    })],
                );
            }
            clean_and_finish(id, rq, 1)
        }
        "copy2" => {
            if let Some(e) = got_err(&rq.provided, "cp2") {
                return fail(id, &format!("安装到用户目录也失败：{e}"));
            }
            clean_and_finish(id, rq, 2)
        }
        // 7) 收尾
        "clean" => {
            let target = state_s(&rq.state, "target");
            let exe = format!("{target}/rclone.exe");
            let version = state_s(&rq.state, "version");
            let label = state_s(&rq.state, "label");
            let mut notes: Vec<String> = rq
                .state
                .get("label_notes")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
                .unwrap_or_default();
            if let Some(n) = rq.state.get("copy_note").and_then(|v| v.as_str()) {
                notes.push(n.to_string());
            }
            let note_text = notes.join("；");
            let mut msg = format!("已安装 rclone {version}（{label}）");
            if !note_text.is_empty() {
                msg.push_str("。");
                msg.push_str(&note_text);
            }
            let ops = vec![json!({
                "op": "kv.set",
                "key": "_state",
                "value": { "exe_version": version, "proc": Value::Null },
            })];
            done_ops(
                id,
                ops,
                json!({
                    "values": { "exe": exe },
                    "status": format!("已安装 rclone {version}"),
                    "lamp": "warn",
                    "hints": { "install_hint": format!("安装位置：{exe}") },
                    "toast": { "level": "info", "message": msg },
                    "enabled": { "start": true, "stop": false },
                }),
            )
        }
        other => fail(id, &format!("内部状态异常：{other}")),
    }
}

/// 复制成功之后的收尾：清临时目录 → 完成。
fn clean_and_finish(id: u64, rq: &Rq, which: u8) -> Value {
    let tmp = state_s(&rq.state, "tmp");
    let state = keep(rq.state.clone(), "step", json!("clean"));
    let mut list = Vec::new();
    if !tmp.is_empty() {
        list.push(json!({ "cap": "fs.remove", "key": "clean", "params": { "path": tmp } }));
    }
    let _ = which;
    need(id, state, list)
}

/// 安装根目录：提权装进 Program Files，否则装到用户目录。
fn install_roots(state: &Value) -> (String, String) {
    let elevated = state.get("elevated").and_then(|v| v.as_bool()).unwrap_or(false);
    let local = state_s(state, "localappdata");
    let pf = state_s(state, "programfiles");
    let user = if local.is_empty() {
        format!("{}/RCGUI/rclone", state_s(state, "data_dir"))
    } else {
        format!("{local}/RCGUI/rclone")
    };
    if elevated && !pf.is_empty() {
        (format!("{pf}/RCGUI/rclone"), user)
    } else {
        (user.clone(), String::new())
    }
}

// ---------------- start / stop ----------------

fn on_start(id: u64, rq: &Rq, auto: bool) -> Value {
    let st = step_of(&rq.state);
    let form = &rq.form;

    if auto && !fbool(form, "auto_start", false) {
        // 用户没开自动启动：静默返回，不打扰
        return done(id, json!({}));
    }

    if st.is_empty() {
        let exe = fstr(form, "exe");
        return need(
            id,
            json!({ "step": "read", "auto": auto, "exe": exe }),
            vec![
                json!({ "cap": "kv.get_all", "key": "kv" }),
                json!({ "cap": "fs.meta", "key": "meta", "params": { "path": exe } }),
                json!({
                    "cap": "net.port_open",
                    "key": "port",
                    "params": { "port": fport(form), "timeout_ms": 800 }
                }),
            ],
        );
    }

    match st.as_str() {
        "read" => {
            let kv = got(&rq.provided, "kv");
            let mut state = rq.state.clone();
            state = keep(state, "meta", got(&rq.provided, "meta"));
            state = keep(state, "port_open", got(&rq.provided, "port"));
            // 这一轮之后不再要 kv，把它带上，后面几轮要用
            state = keep(state, "kv", kv.clone());
            let handle = proc_handle(&kv);
            if handle.is_empty() {
                return launch_decision(id, &state, form, &kv, &Value::Null, auto);
            }
            need(
                id,
                keep(state, "step", json!("check")),
                vec![json!({
                    "cap": "proc.status",
                    "key": "proc",
                    "params": { "handle": handle }
                })],
            )
        }
        "check" => {
            let proc = got(&rq.provided, "proc");
            launch_decision(id, &rq.state, form, &read_kv_cache(rq), &proc, auto)
        }
        // 已经试过启动：收尾
        "launch" => {
            if let Some(e) = got_err(&rq.provided, "run") {
                let msg = format!("rclone 启动失败：{e}");
                if auto {
                    return done(id, json!({ "lamp": "err", "status": msg }));
                }
                return done(
                    id,
                    json!({
                        "status": msg,
                        "lamp": "err",
                        "toast": { "level": "err", "message": msg },
                    }),
                );
            }
            let started = got(&rq.provided, "run");
            let handle = s(started.get("handle"));
            let pid = started.get("pid").and_then(|v| v.as_i64()).unwrap_or(0);
            let port = fport(form);
            let no_auth = fbool(form, "no_auth", false);
            let (user, pass) = if no_auth {
                (String::new(), String::new())
            } else {
                (fstr(form, "rc_user"), fstr(form, "rc_pass"))
            };
            let kv = read_kv_cache(rq);
            let ops = vec![state_op(&kv, |st| {
                st.insert("proc".into(), json!({ "handle": handle, "pid": pid }));
            })];
            done_ops(
                id,
                ops,
                json!({
                    "status": format!("正在运行（PID {pid}）"),
                    "lamp": "ok",
                    // 「自动连接」勾选时，把本次启动用的地址/用户名/密码交给宿主写进设置并连接；
                    // 不勾选就完全不动设置页（用户可能连的是别的实例）。
                    "rc": if fbool(form, "auto_connect", true) {
                        json!({
                            "url": format!("http://127.0.0.1:{port}"),
                            "user": user,
                            "pass": pass,
                            "connect": true,
                        })
                    } else {
                        Value::Null
                    },
                    "toast": {
                        "level": "info",
                        "message": if fbool(form, "auto_connect", true) {
                            format!("已启动 rclone（端口 {port}），正在连接…")
                        } else {
                            format!("已启动 rclone（端口 {port}）")
                        }
                    },
                    "enabled": { "start": false, "stop": true },
                }),
            )
        }
        other => fail(id, &format!("内部状态异常：{other}")),
    }
}

/// 「check」轮拿不到 kv（这一轮只要了 proc.status），从 state 里取回上一轮存下的那份。
fn read_kv_cache(rq: &Rq) -> Value {
    rq.state.get("kv").cloned().unwrap_or(Value::Null)
}

/// 已经在跑就直接连；端口已被占用就当它已经在跑；否则拉起新进程。
fn launch_decision(
    id: u64,
    state: &Value,
    form: &Value,
    kv: &Value,
    proc: &Value,
    auto: bool,
) -> Value {
    let exe = fstr(form, "exe");
    let port = fport(form);
    let running = proc.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
    let port_open = state
        .get("port_open")
        .and_then(|v| v.get("open"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let meta = state.get("meta").cloned().unwrap_or(Value::Null);
    let exe_ok = meta.get("exists").and_then(|v| v.as_bool()).unwrap_or(false);

    if running {
        let pid = proc.get("pid").and_then(|v| v.as_i64()).unwrap_or(0);
        return done(
            id,
            json!({
                "status": format!("正在运行（PID {pid}）"),
                "lamp": "ok",
                "enabled": { "start": false, "stop": true },
            }),
        );
    }
    // 端口有人听着：大概率是上次留下的 rclone，直接连比再拉一个更安全
    if port_open {
        let no_auth = fbool(form, "no_auth", false);
        let (user, pass) = if no_auth {
            (String::new(), String::new())
        } else {
            (fstr(form, "rc_user"), fstr(form, "rc_pass"))
        };
        return done(
            id,
            json!({
                "status": format!("端口 {port} 上已有 rclone 在响应，已直接连接"),
                "lamp": "ok",
                // 「自动连接」勾选时把地址与凭据交给宿主写进设置并连接
                "rc": if fbool(form, "auto_connect", true) {
                    json!({
                        "url": format!("http://127.0.0.1:{port}"),
                        "user": user,
                        "pass": pass,
                        "connect": true,
                    })
                } else {
                    Value::Null
                },
                "enabled": { "start": false, "stop": false },
            }),
        );
    }
    if exe.is_empty() {
        return if auto {
            done(id, json!({ "lamp": "warn" }))
        } else {
            fail(id, "还没有设置 rclone.exe 路径")
        };
    }
    if !exe_ok {
        return if auto {
            done(id, json!({ "lamp": "warn" }))
        } else {
            fail(id, &format!("rclone.exe 不存在：{exe}"))
        };
    }
    if auto && verified_version(kv).is_empty() {
        // 没校验过就不自动启动：静默留一条状态，等用户自己点
        return done(
            id,
            json!({ "status": "自动启动已跳过：rclone 尚未通过校验", "lamp": "warn" }),
        );
    }

    // 拒绝拉起一个「谁都指挥不动」的实例。
    //
    // rclone rc 只放行注册为 NoAuth 的接口（core/version、rc/list…），其余接口一律
    // 要求服务端配置了鉴权，否则 403。实测无鉴权实例的 options/get、config/paths、
    // mount/mount、serve/start 全部 403，在宿主侧表现为「握手成功，但 5 项自动恢复
    // 全失败」。这种进程对宿主毫无用处，早失败 + 说清原因，比留下一个僵尸实例好。
    if !fbool(form, "no_auth", false) && fstr(form, "rc_user").is_empty() {
        let msg = "未填写 RC 用户名：这样启动的 rclone 会拒绝挂载 / 服务 / 参数读取等所有接口（403）。\
                   请填用户名（密码可留空），或勾选「本地不校验（--rc-no-auth）」。";
        return if auto {
            done(id, json!({ "status": msg, "lamp": "warn" }))
        } else {
            fail(id, msg)
        };
    }

    let mut args = vec![
        "rcd".to_string(),
        "--rc-addr".to_string(),
        format!("127.0.0.1:{port}"),
    ];
    if fbool(form, "no_auth", false) {
        args.push("--rc-no-auth".into());
    } else {
        let user = fstr(form, "rc_user");
        let pass = fstr(form, "rc_pass");
        if !user.is_empty() {
            args.push("--rc-user".into());
            args.push(user);
        }
        if !pass.is_empty() {
            args.push("--rc-pass".into());
            args.push(pass);
        }
    }
    let data_dir = state_s(state, "data_dir");
    let log = if data_dir.is_empty() {
        String::new()
    } else {
        format!("{data_dir}/logs/oneclick-rclone.log")
    };
    let silent = fbool(form, "silent", true);
    let keep_on_exit = !fbool(form, "stop_on_exit", true);
    let start_state = keep(state.clone(), "step", json!("launch"));
    need(
        id,
        start_state,
        vec![json!({
            "cap": "proc.start",
            "key": "run",
            "params": {
                "program": exe,
                "args": args,
                "silent": silent,
                "keep_on_exit": keep_on_exit,
                "log": log,
            }
        })],
    )
}

fn on_stop(id: u64, rq: &Rq) -> Value {
    let st = step_of(&rq.state);
    if st.is_empty() {
        return need(
            id,
            json!({ "step": "read" }),
            vec![json!({ "cap": "kv.get_all", "key": "kv" })],
        );
    }
    let kv = got(&rq.provided, "kv");
    if st == "read" {
        let handle = proc_handle(&kv);
        let exe = fstr(&rq.form, "exe");
        if handle.is_empty() {
            return done(
                id,
                json!({
                    "status": "当前没有由本插件启动的 rclone",
                    "lamp": "warn",
                    "enabled": { "start": !exe.is_empty(), "stop": false },
                }),
            );
        }
        return need(
            id,
            keep(keep(json!({ "step": "stop" }), "kv", kv.clone()), "auto", json!(false)),
            vec![json!({
                "cap": "proc.stop",
                "key": "stop",
                "params": { "handle": handle }
            })],
        );
    }
    let kv = rq.state.get("kv").cloned().unwrap_or(kv);
    let stopped = got(&rq.provided, "stop");
    let pid = stopped.get("pid").and_then(|v| v.as_i64()).unwrap_or(0);
    let ops = vec![state_op(&kv, |st| {
        st.remove("proc");
    })];
    done_ops(
        id,
        ops,
        json!({
            "status": if pid > 0 {
                format!("已停止 rclone（PID {pid}）")
            } else {
                "已停止".to_string()
            },
            "lamp": "warn",
            "toast": { "level": "info", "message": "已停止 rclone" },
            "enabled": { "start": !fstr(&rq.form, "exe").is_empty(), "stop": false },
        }),
    )
}

// ---------------- 解析下载地址 ----------------

fn rclone_arch(arch: String) -> Result<String, String> {
    match arch.as_str() {
        "x86_64" => Ok("amd64".into()),
        "x86" => Ok("386".into()),
        "aarch64" => Ok("arm64".into()),
        other => Err(format!("不支持的 CPU 架构：{other}")),
    }
}

fn official_zip_name(version: &str, arch: &str) -> String {
    format!("rclone-{version}-windows-{arch}.zip")
}

fn official_url(base: &str, version: &str, arch: &str) -> String {
    format!(
        "{}/{}/{}",
        base.trim_end_matches('/'),
        version,
        official_zip_name(version, arch)
    )
}

fn extra_zip_name(arch: &str) -> String {
    format!("rclone-windows-{arch}.zip")
}

/// 官方版的候选源（顺序即优先级）。
fn official_bases(mirror_first: bool) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if mirror_first {
        for (name, base) in MIRRORS {
            out.push(((*name).to_string(), (*base).to_string()));
        }
    }
    out.push((
        if mirror_first {
            "官方源（镜像不可用，已回退）".to_string()
        } else {
            "官方源".to_string()
        },
        OFFICIAL_BASE.to_string(),
    ));
    out
}

/// 从 `rclone v1.75.2` 这类文本里抽出 `v1.75.2`。
fn parse_version_txt(text: &str) -> Option<String> {
    let first = text.trim().lines().next()?.trim();
    let first = first.strip_prefix("rclone").unwrap_or(first).trim();
    let token = first.split_whitespace().next()?;
    let mut cs = token.chars();
    if cs.next()? != 'v' {
        return None;
    }
    if !cs.next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        return None;
    }
    Some(token.to_string())
}

/// 官方版：按顺序挑第一个取到 `version.txt` 的源。
fn resolve_official(
    provided: &Value,
    arch: &str,
    mirror_first: bool,
) -> Result<(String, String, String, Vec<String>), String> {
    let bases = official_bases(mirror_first);
    let mut last = String::from("没有可用的下载源");
    for (i, (name, base)) in bases.iter().enumerate() {
        let key = format!("src{i}");
        if let Some(e) = got_err(provided, &key) {
            last = e;
            continue;
        }
        let body = s(got(provided, &key).get("body"));
        let Some(version) = parse_version_txt(&body) else {
            last = format!("{base}/version.txt 内容无法解析");
            continue;
        };
        let mut notes = Vec::new();
        if mirror_first {
            if i + 1 < bases.len() {
                notes.push(format!("使用{name}"));
            } else {
                notes.push(format!("镜像源不可用，已回退到官方源（{last}）"));
            }
        }
        return Ok((
            official_url(base, &version, arch),
            version,
            name.clone(),
            notes,
        ));
    }
    Err(format!("无法解析 rclone 版本：{last}"))
}

/// 增强版：只发布在 GitHub Releases。
fn resolve_enhanced(
    provided: &Value,
    arch: &str,
) -> Result<(String, String, String, Vec<String>), String> {
    if let Some(e) = got_err(provided, "src0") {
        return Err(format!("无法查询 GitHub Releases：{e}"));
    }
    let body = got(provided, "src0").get("body").cloned().unwrap_or(Value::Null);
    let (tag, asset) = parse_github_release(&body, arch)?;
    let url = match asset {
        Some(u) => u,
        None => format!(
            "https://github.com/gulp79/rclone-extra/releases/download/{tag}/{}",
            extra_zip_name(arch)
        ),
    };
    Ok((
        url,
        tag.clone(),
        "GitHub Releases（rclone-extra）".to_string(),
        vec!["增强版只发布在 GitHub Releases，镜像源对它不可用，已直连 GitHub".to_string()],
    ))
}

fn parse_github_release(json: &Value, arch: &str) -> Result<(String, Option<String>), String> {
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("GitHub 返回里没有 tag_name")?
        .to_string();
    if tag.is_empty() {
        return Err("GitHub 返回的 tag_name 为空".into());
    }
    let want = extra_zip_name(arch).to_ascii_lowercase();
    let assets = json
        .get("assets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for a in &assets {
        let name = s(a.get("name"));
        if !name.is_empty() {
            names.push(name.clone());
        }
        if name.to_ascii_lowercase() == want {
            let url = s(a.get("browser_download_url"));
            return Ok((tag.clone(), (!url.is_empty()).then_some(url)));
        }
    }
    if names.is_empty() {
        // 没有 assets 字段时交给固定 URL 兜底
        return Ok((tag, None));
    }
    names.truncate(8);
    Err(format!(
        "该版本没有 {want}（现有资产：{}）",
        names.join("、")
    ))
}

/// 下载源不是官方站点时提醒一句（镜像没有义务与官方逐字节一致）。
fn mirror_notes(url: &str) -> Vec<String> {
    let host = url.split('/').nth(2).unwrap_or("");
    if host.is_empty() || host.contains("rclone.org") || host.contains("github.com") {
        return Vec::new();
    }
    vec![format!(
        "下载源不是官方站点（{host}），如需严格校验请核对官方 SHA256SUMS"
    )]
}

fn safe_dir_name(version: &str) -> String {
    let cleaned: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "rclone".into()
    } else {
        cleaned
    }
}

// ---------------- 校验结论 ----------------

struct Verdict {
    ok: bool,
    version: String,
    detail: String,
    checks: Vec<Value>,
}

fn human_size(n: u64) -> String {
    let mb = n as f64 / 1024.0 / 1024.0;
    if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{} KB", n / 1024)
    }
}

/// 判断一个文件「是不是可信的 rclone.exe」。
///
/// 判定链与上一代内置实现一致：存在 → 文件名（软信号）→ 体积 → PE 头 →
/// 数字签名 → `<exe> version` 输出的硬证据。**最后一步才是硬证据**，
/// 前面的检查只是为了让界面能尽早给出可读的失败原因。
fn judge(exe: &str, meta: &Value, pe: &Value, sig: &Value, run: &Value) -> Verdict {
    let exists = meta.get("exists").and_then(|v| v.as_bool()).unwrap_or(false)
        && meta.get("is_file").and_then(|v| v.as_bool()).unwrap_or(false);
    let size = meta.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
    let name_ok = exe
        .rsplit(['/', '\\'])
        .next()
        .map(|n| n.to_ascii_lowercase().contains("rclone"))
        .unwrap_or(false);
    let size_ok = (MIN_EXE_SIZE..=MAX_EXE_SIZE).contains(&size);
    let pe_ok = pe.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let pe_desc = {
        let d = s(pe.get("desc"));
        if d.is_empty() {
            "未知格式".to_string()
        } else {
            d
        }
    };

    let sig_status = s(sig.get("status"));
    let sig_signer = s(sig.get("signer"));
    let sig_ok = if sig_status.eq_ignore_ascii_case("valid") {
        // 签名有效但不是 rclone 签的：多半是系统里同名的别的程序
        sig_signer.to_ascii_lowercase().contains("rclone")
    } else {
        // NotSigned 不拦：官方 rclone 的 Windows 二进制本来就没有正式签名
        sig_status.eq_ignore_ascii_case("notsigned") || sig_status.is_empty()
    };
    let sig_text = if sig_status.is_empty() {
        "未检测".to_string()
    } else if sig_status.eq_ignore_ascii_case("valid") {
        format!("有效（{sig_signer}）")
    } else {
        sig_status.clone()
    };

    let run_code = run.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    let stdout = s(run.get("stdout"));
    let stderr = s(run.get("stderr"));
    let version = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("rclone ").map(|v| v.trim().to_string()))
        .filter(|v| v.starts_with('v'))
        .unwrap_or_default();
    let run_ok = run_code == 0 && !version.is_empty();
    let run_text = if run_ok {
        version.clone()
    } else if let Some(e) = run.get("_error").and_then(|v| v.as_str()) {
        e.to_string()
    } else if !stderr.is_empty() {
        stderr.lines().next().unwrap_or("执行失败").to_string()
    } else {
        format!("退出码 {run_code}")
    };

    let checks = vec![
        json!({
            "label": "文件存在",
            "value": if exists { exe.to_string() } else { "不存在".to_string() },
            "ok": exists,
        }),
        json!({
            "label": "文件名",
            "value": if name_ok { "含 rclone".to_string() } else { "不含 rclone".to_string() },
            "ok": name_ok,
        }),
        json!({ "label": "体积", "value": human_size(size), "ok": size_ok }),
        json!({
            "label": "程序格式",
            "value": if pe_ok { pe_desc } else { "不是 Windows 可执行文件".to_string() },
            "ok": pe_ok,
        }),
        json!({ "label": "数字签名", "value": sig_text, "ok": sig_ok }),
        json!({ "label": "可执行", "value": run_text, "ok": run_ok }),
    ];

    let mut detail = String::new();
    if !exists {
        detail = "文件不存在".into();
    } else if !size_ok {
        detail = format!("文件大小异常（{}）", human_size(size));
    } else if !pe_ok {
        detail = "不是有效的 Windows 可执行文件".into();
    } else if !sig_ok {
        detail = "数字签名异常".into();
    } else if !run_ok {
        detail = format!("执行 `rclone version` 失败：{run_text}");
    } else if !name_ok {
        detail = "文件名不含 rclone".into();
    }

    Verdict {
        ok: exists && size_ok && pe_ok && sig_ok && run_ok,
        version,
        detail,
        checks,
    }
}
