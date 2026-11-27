//! 插件宿主的**通用能力**实现。
//!
//! 设计原则：这里只提供「操作系统本来就有的原子操作」（读文件、算哈希、跑进程、
//! 下文件、解压、探端口……），**不含任何插件业务策略**。策略住在插件自己的
//! wasm 里，因此新增插件不需要改 GUI 源码。
//!
//! 能力授权不在这里做：宿主在 [`plugin_host::PluginHost::invoke_with`] 里按
//! `config.json` 的 `capabilities` 先拦一遍，能进到 `cap_call` 的调用已经是允许的。

use crate::kv::KvStore;
use crate::{config, MainWindow};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use plugin_host::CapProvider;
use rcgui_rc::RcState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------- 进度中心 ----------------

/// 当前正在执行的长任务（下载 / 解压 / 安装）。
#[derive(Debug, Clone, Default)]
pub struct ProgressState {
    pub busy: bool,
    pub label: String,
    pub done: u64,
    pub total: u64,
}

impl ProgressState {
    fn to_json(&self) -> Value {
        json!({
            "busy": self.busy,
            "label": self.label,
            "done": self.done,
            "total": self.total,
        })
    }

    /// 0.0–1.0；总大小未知时返回 -1（UI 用不确定进度条）。
    fn ratio(&self) -> f32 {
        if self.total == 0 {
            -1.0
        } else {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32
        }
    }
}

/// 长任务进度广播：宿主执行下载时写，UI 定时读。
///
/// 用「共享状态 + UI 主动轮询」而不是回调，是因为进度更新的频率远高于
/// UI 事件队列能承受的节奏；由 UI 每 200ms 拉一次，天然做了节流。
pub struct ProgressHub {
    state: Mutex<ProgressState>,
    /// 每次写入递增，UI 据此判断是否需要重绘
    seq: AtomicU64,
    /// 已经推给 UI 的序号，避免每秒重复写同样的值
    pushed: AtomicU64,
    ui: Mutex<Option<slint::Weak<MainWindow>>>,
}

impl Default for ProgressHub {
    fn default() -> Self {
        Self {
            state: Mutex::new(ProgressState::default()),
            seq: AtomicU64::new(0),
            pushed: AtomicU64::new(0),
            ui: Mutex::new(None),
        }
    }
}

impl ProgressHub {
    pub fn set_ui(&self, ui: slint::Weak<MainWindow>) {
        *self.ui.lock().unwrap() = Some(ui);
    }

    pub fn begin(&self, label: impl Into<String>) {
        let mut g = self.state.lock().unwrap();
        *g = ProgressState {
            busy: true,
            label: label.into(),
            done: 0,
            total: 0,
        };
        self.bump(&mut g);
    }

    pub fn update(&self, done: u64, total: Option<u64>) {
        let mut g = self.state.lock().unwrap();
        g.busy = true;
        g.done = done;
        if let Some(t) = total {
            g.total = t;
        }
        self.bump(&mut g);
    }

    pub fn finish(&self) {
        let mut g = self.state.lock().unwrap();
        *g = ProgressState::default();
        self.bump(&mut g);
    }

    pub fn snapshot(&self) -> ProgressState {
        self.state.lock().unwrap().clone()
    }

    fn bump(&self, _g: &mut ProgressState) {
        self.seq.fetch_add(1, Ordering::Relaxed);
    }
}

/// 把当前进度推给 UI（由轮询循环每秒调用一次）。
///
/// 这里必须走 `upgrade_in_event_loop`：调用方在 tokio 工作线程上，
/// 而 Slint 属性只能在 UI 线程改。`ProgressHub` 内部持有 Weak，
/// 就不必把 UI 句柄在 Ctx 里到处传。
pub fn pump_progress(hub: &ProgressHub) {
    let st = hub.snapshot();
    let seq = hub.seq.load(Ordering::Relaxed);
    // 没变化就不打扰 UI 线程
    if seq == hub.pushed.load(Ordering::Relaxed) {
        return;
    }
    hub.pushed.store(seq, Ordering::Relaxed);
    let weak = hub.ui.lock().unwrap().clone();
    let Some(ui) = weak else { return };
    let _ = ui.upgrade_in_event_loop(move |ui| {
        use slint::ComponentHandle as _;
        let g = ui.global::<crate::PluginsG>();
        g.set_op_busy(st.busy);
        g.set_op_label(st.label.as_str().into());
        g.set_op_progress(st.ratio());
    });
}

// ---------------- 子进程登记 ----------------

struct ProcEntry {
    child: std::process::Child,
    started: Instant,
    /// 宿主退出时是否保留（= 插件设置「退出时停止」取反）
    keep_on_exit: bool,
}

static PROCS: Mutex<Option<HashMap<String, ProcEntry>>> = Mutex::new(None);
static PROC_SEQ: AtomicU64 = AtomicU64::new(1);

fn procs_lock() -> std::sync::MutexGuard<'static, Option<HashMap<String, ProcEntry>>> {
    PROCS.lock().unwrap_or_else(|e| e.into_inner())
}

/// 退出时停掉所有「不保留」的插件子进程。
///
/// **刻意不调用 wasm**：退出路径上任何一次 await 都可能拖住退出（历史上踩过），
/// 所以插件在 `proc.start` 时就把「要不要留」告诉宿主，宿主自己记账。
pub fn stop_plugin_procs_on_exit() {
    let taken: Vec<(String, ProcEntry)> = {
        let mut g = procs_lock();
        let map = g.as_mut();
        match map {
            None => Vec::new(),
            Some(m) => {
                let keep: Vec<String> = m
                    .iter()
                    .filter(|(_, e)| e.keep_on_exit)
                    .map(|(k, _)| k.clone())
                    .collect();
                let mut out = Vec::new();
                for k in m.keys().cloned().collect::<Vec<_>>() {
                    if keep.contains(&k) {
                        continue;
                    }
                    if let Some(e) = m.remove(&k) {
                        out.push((k, e));
                    }
                }
                out
            }
        }
    };
    for (handle, mut e) in taken {
        let pid = e.child.id();
        let _ = e.child.kill();
        let _ = e.child.wait();
        #[cfg(windows)]
        {
            let mut cmd = std::process::Command::new("taskkill.exe");
            cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
            let _ = cmd.output();
        }
        let _ = handle;
    }
}

/// 停掉某个插件登记过的全部子进程（卸载 / 停用插件时调用）。
///
/// 与 [`stop_plugin_procs_on_exit`] 的区别：这里**不看** `keep_on_exit` ——
/// 插件都要被删掉了，它拉起的进程不该继续留着。
pub fn stop_plugin_procs(plugin_id: &str) {
    let taken: Vec<(String, ProcEntry)> = {
        let mut g = procs_lock();
        let Some(m) = g.as_mut() else { return };
        let keys: Vec<String> = m
            .keys()
            .filter(|k| k.starts_with(&format!("{plugin_id}#")))
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|k| m.remove(&k).map(|e| (k, e)))
            .collect()
    };
    for (handle, mut e) in taken {
        let pid = e.child.id();
        let _ = e.child.kill();
        let _ = e.child.wait();
        #[cfg(windows)]
        {
            let mut cmd = std::process::Command::new("taskkill.exe");
            cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
            let _ = cmd.output();
        }
        eprintln!("[plugin] 已停止插件子进程 {handle}（PID {pid}）");
    }
}

fn no_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW = 0x08000000
        cmd.creation_flags(0x0800_0000);
    }
}

// ---------------- 能力实现 ----------------

pub struct HostCaps {
    pub rc: Arc<RcState>,
    pub store: Arc<KvStore>,
    pub progress: Arc<ProgressHub>,
    /// 整个程序的状态，只给「按远程所有者分派」这一类判断用。
    ///
    /// 比如 `rclone.upload_file` 要先知道这条远程是普通 rclone 远程还是插件托管的，
    /// 那个判断（`remote_ops::owner_of`）要看插件清单和远程登记表，光有 `rc` 与
    /// `store` 做不到。`Ctx` 自己不存 `HostCaps`，所以这里放 `Arc<Ctx>` 不会绕成环。
    pub ctx: Arc<crate::Ctx>,
}

impl HostCaps {
    fn p_str(params: &Value, k: &str) -> String {
        params
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn p_u64(params: &Value, k: &str) -> u64 {
        params
            .get(k)
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0)
    }

    fn p_bool(params: &Value, k: &str, d: bool) -> bool {
        params.get(k).and_then(|v| v.as_bool()).unwrap_or(d)
    }
}

/// 只读解析 rclone.conf 里的 `[远程名]` 段：不启动进程、不碰 RC。
///
/// 只在 RC 拿不到列表时兜底（rclone 没在跑、或者认证对不上）。读不到就返回空，
/// 由插件自己决定怎么提示——这里不猜、不造名字。
fn remotes_from_conf() -> Vec<String> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("RCLONE_CONFIG") {
        if !p.trim().is_empty() {
            paths.push(PathBuf::from(p.trim()));
        }
    }
    if let Ok(a) = std::env::var("APPDATA") {
        paths.push(PathBuf::from(a).join("rclone").join("rclone.conf"));
    }
    if let Ok(u) = std::env::var("USERPROFILE") {
        paths.push(
            PathBuf::from(u)
                .join(".config")
                .join("rclone")
                .join("rclone.conf"),
        );
    }
    for p in paths {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let mut out: Vec<String> = Vec::new();
        for line in text.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix('[') else {
                continue;
            };
            let Some(name) = rest.strip_suffix(']') else {
                continue;
            };
            let name = name.trim();
            if !name.is_empty() && !out.iter().any(|x| x == name) {
                out.push(name.to_string());
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

#[async_trait::async_trait]
impl CapProvider for HostCaps {
    // 能力层只按名字转给内核。哪一类远程要由插件接管、请求怎么编码、
    // 认领者缺席时该说什么，全在 `remote_ops` 里：这里不该出现任何具体远程类型的业务。
    async fn rclone_call(&self, method: String, params: Value) -> Result<Value> {
        let client = self.rc.client().await;
        client.call(&method, &params).await.map_err(Into::into)
    }

    // `rclone.upload_file` 的目标可能是普通 rclone 远程，也可能是插件托管的那种，
    // 所以先问一句「这条远程归谁管」，再决定字节交给谁发。
    //
    // 为什么不能一律 copyfile：K-Vault 这类远程在内核里就是个 WebDAV 地址，站点只
    // 放行它自己的接口，原生 PUT 会被回一句 `403 Forbidden: this WebDAV endpoint is
    // read-only`；而且站点地址、去重开关都在插件手里，只有它能算出给用户的那个直链。
    //
    // 插件那一路不把字节塞进 wasm：宿主先把内容落到临时文件，用
    // [`plugin_host::upload::grant`] 授权这一次发送，再把「哪个文件、发到哪、叫什么
    // 名字」告诉它。插件读不到别的路径，也读不到第二次。
    async fn upload_file(
        &self,
        fs: String,
        remote_path: String,
        filename: String,
        data_b64: String,
    ) -> Result<Value> {
        let data = base64::engine::general_purpose::STANDARD
            .decode(data_b64.as_bytes())
            .map_err(|e| anyhow!("{}", rcgui_i18n::f("base64 解码失败: {}", &[&(e)])))?;
        let tmp_dir = config::data_dir().join("tmp");
        std::fs::create_dir_all(&tmp_dir)?;
        let name = format!("up-{}-{}", std::process::id(), chrono_ts());
        let tmp = tmp_dir.join(&name);
        std::fs::write(&tmp, &data)?;

        let r = match crate::remote_ops::upload_file_owned(
            &self.ctx,
            &fs,
            &remote_path,
            &filename,
            &tmp,
        )
        .await
        {
            // 插件传完了：它的回复里带着名字、直链和去重标记，原样往上交，
            // 上层（imagehost 之类）靠 `link` 决定给用户站点地址还是本机代理地址。
            Ok(Some(v)) => Ok(v),
            // 这条远程不归插件管：照旧走原生 copyfile。
            Ok(None) => {
                let client = self.rc.client().await;
                client
                    .copyfile(&rclone_client::local_fs(&tmp_dir), &name, &fs, &remote_path)
                    .await
                    .map_err(anyhow::Error::from)
                    .map(|()| json!({}))
            }
            // 认领者不在或不肯接：如实报出来，不退回原生访问。
            Err(e) => Err(anyhow!(e)),
        };
        // 两条路都在这次调用里读完了文件，临时副本留着只是占盘。
        let _ = std::fs::remove_file(&tmp);
        r
    }

    async fn kv_get_all(&self, plugin_id: &str) -> Result<Value> {
        Ok(self.store.get_all(plugin_id))
    }

    async fn kv_set(&self, plugin_id: &str, key: String, value: Value) -> Result<()> {
        self.store.set(plugin_id, &key, value)
    }

    async fn kv_delete(&self, plugin_id: &str, key: String) -> Result<()> {
        self.store.delete(plugin_id, &key)
    }

    async fn log(&self, message: String) {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(config::data_dir().join("logs").join("plugins.log"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{message}\n").as_bytes()));
    }

    fn live(&self) -> Value {
        self.progress.snapshot().to_json()
    }

    async fn toast(&self, level: &str, message: String) {
        if let Some(ui) = self.progress.ui.lock().unwrap().clone() {
            let kind = match level {
                "err" | "error" => 2,
                "warn" | "warning" => 1,
                _ => 0,
            };
            let msg = message.clone();
            let _ = ui.upgrade_in_event_loop(move |w| {
                crate::toast_ui(&w, kind, msg);
            });
        }
    }

    async fn cap_call(&self, op: &str, params: Value) -> Result<Value> {
        match op {
            // ---------------- 文件系统 ----------------
            "fs.meta" => {
                let path = Self::p_str(&params, "path");
                spawn_blocking(move || meta_of(&path)).await
            }
            "fs.sha256" => {
                let path = Self::p_str(&params, "path");
                spawn_blocking(move || {
                    let p = PathBuf::from(&path);
                    if !p.is_file() {
                        bail!("{}", rcgui_i18n::f("文件不存在: {}", &[&(path)]));
                    }
                    let sha = plugin_host::zip::sha256_file(&p)?;
                    let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                    Ok(json!({ "sha256": sha, "size": size }))
                })
                .await
            }
            "fs.read_text" => {
                let path = Self::p_str(&params, "path");
                let max = Self::p_u64(&params, "max_bytes").max(1) as usize;
                spawn_blocking(move || {
                    let raw = std::fs::read(&path).with_context(|| rcgui_i18n::f("读取 {} 失败", &[&(path)]))?;
                    let cut = raw.len().min(max);
                    Ok(json!({ "text": String::from_utf8_lossy(&raw[..cut]).to_string(), "size": raw.len() }))
                })
                .await
            }
            "fs.read_b64" => {
                let path = Self::p_str(&params, "path");
                let max = if Self::p_u64(&params, "max_bytes") == 0 {
                    64 * 1024 * 1024
                } else {
                    Self::p_u64(&params, "max_bytes")
                } as usize;
                spawn_blocking(move || {
                    let md = std::fs::metadata(&path)
                        .with_context(|| rcgui_i18n::f("读取 {} 失败", &[&(path)]))?;
                    if !md.is_file() {
                        bail!("{}", rcgui_i18n::f("不是文件: {}", &[&(path)]));
                    }
                    if md.len() as usize > max {
                        bail!(
                            "{}",
                            rcgui_i18n::f(
                                "文件超过 {} MB 上限: {}",
                                &[&(max / 1024 / 1024), &(path)]
                            )
                        );
                    }
                    let raw = std::fs::read(&path)
                        .with_context(|| rcgui_i18n::f("读取 {} 失败", &[&(path)]))?;
                    Ok(json!({
                        "data_b64": base64::engine::general_purpose::STANDARD.encode(&raw),
                        "size": raw.len(),
                    }))
                })
                .await
            }
            // `fs.write_file` 是早期名字。权限表和 `docs/PLUGIN.md` 都还把它
            // 列为可用的别名，参数与行为完全一样，所以两个名字共用这一份实现。
            "fs.write_text" | "fs.write_file" => {
                let path = Self::p_str(&params, "path");
                let text = Self::p_str(&params, "text");
                let append = Self::p_bool(&params, "append", false);
                spawn_blocking(move || {
                    if let Some(d) = Path::new(&path).parent() {
                        let _ = std::fs::create_dir_all(d);
                    }
                    let mut opt = std::fs::OpenOptions::new();
                    opt.create(true).write(true);
                    if append {
                        opt.append(true);
                    } else {
                        opt.truncate(true);
                    }
                    let mut f = opt
                        .open(&path)
                        .with_context(|| rcgui_i18n::f("写入 {} 失败", &[&(path)]))?;
                    std::io::Write::write_all(&mut f, text.as_bytes())?;
                    Ok(json!({ "ok": true }))
                })
                .await
            }
            "fs.write_b64" => {
                let path = Self::p_str(&params, "path");
                let data_b64 = Self::p_str(&params, "data_b64");
                spawn_blocking(move || {
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(data_b64.as_bytes())
                        .map_err(|e| {
                            anyhow!("{}", rcgui_i18n::f("base64 解码失败: {}", &[&(e)]))
                        })?;
                    if let Some(d) = Path::new(&path).parent() {
                        let _ = std::fs::create_dir_all(d);
                    }
                    std::fs::write(&path, &raw)
                        .with_context(|| rcgui_i18n::f("写入 {} 失败", &[&(path)]))?;
                    Ok(json!({ "bytes": raw.len() }))
                })
                .await
            }
            "fs.copy" => {
                let from = Self::p_str(&params, "from");
                let to = Self::p_str(&params, "to");
                spawn_blocking(move || {
                    if let Some(d) = Path::new(&to).parent() {
                        std::fs::create_dir_all(d).with_context(|| {
                            rcgui_i18n::f("创建目录失败: {}", &[&(d.display())])
                        })?;
                    }
                    std::fs::copy(&from, &to).with_context(|| {
                        rcgui_i18n::f(
                            "复制 {} → {} 失败（可能被占用或权限不足）",
                            &[&(from), &(to)],
                        )
                    })?;
                    Ok(json!({ "ok": true, "to": to }))
                })
                .await
            }
            "fs.mkdir" => {
                let path = Self::p_str(&params, "path");
                spawn_blocking(move || {
                    std::fs::create_dir_all(&path)?;
                    Ok(json!({ "ok": true }))
                })
                .await
            }
            // 同上：`fs.remove_dir` 是早期名字。目录走 `remove_dir_all`、
            // 文件走 `remove_file`，两个名字共用的就是这套分流。
            "fs.remove" | "fs.remove_dir" => {
                let path = Self::p_str(&params, "path");
                spawn_blocking(move || {
                    let p = Path::new(&path);
                    if p.is_dir() {
                        std::fs::remove_dir_all(p)
                            .with_context(|| rcgui_i18n::f("删除目录失败: {}", &[&(path)]))?;
                    } else if p.exists() {
                        std::fs::remove_file(p)
                            .with_context(|| rcgui_i18n::f("删除文件失败: {}", &[&(path)]))?;
                    }
                    Ok(json!({ "ok": true }))
                })
                .await
            }
            "fs.list_dir" => {
                let path = Self::p_str(&params, "path");
                spawn_blocking(move || {
                    let mut out = Vec::new();
                    for e in std::fs::read_dir(&path)
                        .with_context(|| rcgui_i18n::f("读取目录失败: {}", &[&(path)]))?
                        .flatten()
                    {
                        let p = e.path();
                        let is_dir = p.is_dir();
                        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                        out.push(json!({
                            "name": e.file_name().to_string_lossy(),
                            "path": p.display().to_string(),
                            "is_dir": is_dir,
                            "size": size,
                        }));
                    }
                    Ok(json!({ "entries": out }))
                })
                .await
            }
            "fs.find" => {
                let dir = Self::p_str(&params, "dir");
                let name = Self::p_str(&params, "name");
                let depth = {
                    let d = Self::p_u64(&params, "max_depth");
                    if d == 0 {
                        4
                    } else {
                        d.min(16) as usize
                    }
                };
                spawn_blocking(move || {
                    let want = name.to_ascii_lowercase();
                    let found = find_file(Path::new(&dir), &want, depth);
                    Ok(json!({ "path": found.clone().unwrap_or_default(), "found": found.is_some() }))
                })
                .await
            }
            // 搜索本机可执行文件：只扫本地固定盘 + PATH / 常见安装目录。
            //
            // 为什么由宿主做而不是插件做：盘符枚举、跳过系统目录、总时限这些
            // 「本机知识」放在宿主里，插件只声明要找什么名字（如 rclone.exe）。
            // 时限是硬约束：网络盘或超大目录不能让插件调用一直挂着。
            "fs.search_exe" => {
                let name = Self::p_str(&params, "name");
                let name = if name.is_empty() {
                    "rclone.exe".to_string()
                } else {
                    name
                };
                let depth = {
                    let d = Self::p_u64(&params, "max_depth");
                    if d == 0 {
                        3
                    } else {
                        d.min(6) as usize
                    }
                };
                let budget_ms = {
                    let d = Self::p_u64(&params, "timeout_ms");
                    if d == 0 {
                        8000
                    } else {
                        d.min(30000)
                    }
                };
                let hub = self.progress.clone();
                hub.begin(rcgui_i18n::t("正在搜索本机程序…"));
                // 用**专用大栈线程**而不是 tokio 的 blocking 池：
                // 后者默认栈只有 2MB，而搜索要遍历深层目录、每层都留一份
                // read_dir 迭代器与 PathBuf；实测在 `C:\` 上会直接把 worker 栈撑爆
                // （进程直接 abort，日志只有 "has overflowed its stack"）。
                let (tx, rx) = std::sync::mpsc::channel::<Result<Value>>();
                let name2 = name.clone();
                std::thread::Builder::new()
                    .name("rcgui-search-exe".into())
                    .stack_size(16 * 1024 * 1024)
                    .spawn(move || {
                        let found = search_exe(&name2, depth, Duration::from_millis(budget_ms));
                        let _ = tx.send(Ok(json!({ "paths": found, "count": found.len() })));
                    })
                    .map_err(|e| anyhow!("{}", rcgui_i18n::f("无法启动搜索线程: {}", &[&(e)])))?;
                let r = tokio::task::spawn_blocking(move || rx.recv())
                    .await
                    .map_err(|e| anyhow!("{}", rcgui_i18n::f("等待搜索结果失败: {}", &[&(e)])))
                    .and_then(|inner| {
                        inner.map_err(|e| anyhow!("{}", rcgui_i18n::f("搜索结果丢失: {}", &[&(e)])))
                    });
                hub.finish();
                r?
            }
            "fs.unzip" => {
                let zip_path = Self::p_str(&params, "zip");
                let dest = Self::p_str(&params, "dest");
                let max = if Self::p_u64(&params, "max_bytes") == 0 {
                    plugin_host::package::MAX_EXTRACTED_BYTES
                } else {
                    Self::p_u64(&params, "max_bytes")
                };
                let hub = self.progress.clone();
                hub.begin(rcgui_i18n::t("正在解压…"));
                let r = spawn_blocking(move || -> Result<Value> {
                    let data = std::fs::read(&zip_path)
                        .with_context(|| rcgui_i18n::f("读取压缩包失败: {}", &[&(zip_path)]))?;
                    if data.len() < 4 || &data[0..2] != b"PK" {
                        bail!("{}", rcgui_i18n::t("不是 zip 文件（可能下载到了错误页面）"));
                    }
                    std::fs::create_dir_all(&dest)?;
                    let n = plugin_host::zip::extract_all(&data, Path::new(&dest), max)?;
                    Ok(json!({ "files": n, "dest": dest }))
                })
                .await;
                hub.finish();
                r
            }
            "fs.pick_file" => {
                let title = Self::p_str(&params, "title");
                let filters: Vec<String> = params
                    .get("filters")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(|s| s.trim_start_matches('.').to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let start = Self::p_str(&params, "start_dir");
                // 插件只能说「面板里哪一栏」（`form_key`），说不了路径：下面是宿主自己
                // 弹的对话框，用户按了「打开」之后才把那个路径记进上传授权表。
                let form_key = Self::p_str(&params, "form_key");
                let plugin_id = Self::p_str(&params, "_plugin");
                let picked = spawn_blocking(move || {
                    let mut d = rfd::FileDialog::new();
                    if !title.is_empty() {
                        d = d.set_title(&title);
                    }
                    if !filters.is_empty() {
                        let refs: Vec<&str> = filters.iter().map(|s| s.as_str()).collect();
                        d = d.add_filter(rcgui_i18n::t("允许的文件"), &refs);
                    }
                    if !start.is_empty() {
                        d = d.set_directory(&start);
                    }
                    // 取消是正常结果（`None`），不是错误；包成 `Ok` 只是因为
                    // 宿主这个 `spawn_blocking` 只收「返回 `Result` 的闭包」。
                    Ok(d.pick_file())
                })
                .await?;
                let path = picked.map(|p| p.display().to_string()).unwrap_or_default();
                // 取消时什么都不记：空路径进授权表，下一次 http.upload 就会拿它去读盘。
                if !path.is_empty() && !form_key.is_empty() {
                    if let Err(e) =
                        plugin_host::upload::grant(&plugin_id, &form_key, Path::new(&path))
                    {
                        // 授权没记下不算这次挑选失败：用户已经挑到了文件，
                        // 返回值照旧给它；失败原因写到插件日志里，别让对话框白挑一次。
                        self.log(format!("上传授权没记下来（{}）：{:#}", form_key, e))
                            .await;
                    }
                }
                Ok(json!({ "path": path }))
            }
            "fs.pick_dir" => {
                let title = Self::p_str(&params, "title");
                spawn_blocking(move || {
                    let mut d = rfd::FileDialog::new();
                    if !title.is_empty() {
                        d = d.set_title(&title);
                    }
                    Ok(json!({ "path": d.pick_folder().map(|p| p.display().to_string()).unwrap_or_default() }))
                })
                .await
            }
            "fs.open_dir" => {
                let path = Self::p_str(&params, "path");
                spawn_blocking(move || {
                    crate::plugins::open_path(&path)?;
                    Ok(json!({ "ok": true }))
                })
                .await
            }
            // 同上：`fs.inspect_exe` 是早期名字，返回的是同一份 PE 信息。
            "fs.pe_info" | "fs.inspect_exe" => {
                let path = Self::p_str(&params, "path");
                spawn_blocking(move || Ok(pe_info_json(&path))).await
            }
            "fs.signature" => {
                let path = Self::p_str(&params, "path");
                spawn_blocking(move || Ok(signature_json(&path))).await
            }

            // ---------------- HTTP ----------------
            "http.text" | "http.json" => {
                let url = Self::p_str(&params, "url");
                let timeout = Self::p_u64(&params, "timeout_ms");
                let timeout = Duration::from_millis(if timeout == 0 { 30_000 } else { timeout });
                let client = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(15))
                    .timeout(timeout)
                    .user_agent(format!("RCGUI/{}", env!("CARGO_PKG_VERSION")))
                    .build()?;
                let resp = client
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| rcgui_i18n::f("请求 {} 失败", &[&(url)]))?;
                let status = resp.status().as_u16();
                let body = resp
                    .text()
                    .await
                    .with_context(|| rcgui_i18n::f("读取 {} 响应失败", &[&(url)]))?;
                Ok(json!({ "status": status, "body": body }))
            }
            "http.request" => {
                let url = Self::p_str(&params, "url");
                let method = {
                    let m = Self::p_str(&params, "method").to_uppercase();
                    if m.is_empty() {
                        "GET".to_string()
                    } else {
                        m
                    }
                };
                let body = Self::p_str(&params, "body");
                let timeout = Self::p_u64(&params, "timeout_ms");
                let timeout = Duration::from_millis(if timeout == 0 { 30_000 } else { timeout });
                let max = if Self::p_u64(&params, "max_bytes") == 0 {
                    16 * 1024 * 1024
                } else {
                    Self::p_u64(&params, "max_bytes")
                } as usize;
                let client = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(15))
                    .timeout(timeout)
                    .user_agent(format!("RCGUI/{}", env!("CARGO_PKG_VERSION")))
                    .build()?;
                let mut req = match method.as_str() {
                    "GET" => client.get(&url),
                    "POST" => client.post(&url),
                    "PUT" => client.put(&url),
                    "PATCH" => client.patch(&url),
                    "DELETE" => client.delete(&url),
                    "HEAD" => client.head(&url),
                    other => bail!(
                        "{}",
                        rcgui_i18n::f("http.request 不支持的方法: {}", &[&(other)])
                    ),
                };
                if let Some(h) = params.get("headers").and_then(|v| v.as_object()) {
                    for (k, v) in h {
                        let Some(v) = v.as_str() else { continue };
                        req = req.header(k.as_str(), v);
                    }
                }
                if !body.is_empty() {
                    req = req.body(body);
                }
                let resp = req
                    .send()
                    .await
                    .with_context(|| rcgui_i18n::f("请求 {} 失败", &[&(url)]))?;
                let status = resp.status().as_u16();
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let bytes = resp
                    .bytes()
                    .await
                    .with_context(|| rcgui_i18n::f("读取 {} 响应失败", &[&(url)]))?;
                if bytes.len() > max {
                    bail!(
                        "{}",
                        rcgui_i18n::f("响应超过 {} MB 上限: {}", &[&(max / 1024 / 1024), &(url)])
                    );
                }
                Ok(json!({
                    "status": status,
                    "content_type": content_type,
                    "body": String::from_utf8_lossy(&bytes).to_string(),
                    "size": bytes.len(),
                }))
            }
            "http.download" => {
                let url = Self::p_str(&params, "url");
                let dest = Self::p_str(&params, "dest");
                let max = if Self::p_u64(&params, "max_bytes") == 0 {
                    400 * 1024 * 1024
                } else {
                    Self::p_u64(&params, "max_bytes")
                };
                let hub = self.progress.clone();
                hub.begin(rcgui_i18n::f("正在下载 {}", &[&(short_host(&url))]));
                let client = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(20))
                    .timeout(Duration::from_secs(1800))
                    .user_agent(format!("RCGUI/{}", env!("CARGO_PKG_VERSION")))
                    .build()?;
                let mut r = client
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| rcgui_i18n::f("下载 {} 失败", &[&(url)]))?;
                if !r.status().is_success() {
                    let code = r.status().as_u16();
                    hub.finish();
                    bail!(
                        "{}",
                        rcgui_i18n::f("下载失败：{} 返回 HTTP {}", &[&(url), &(code)])
                    );
                }
                let total = r.content_length();
                hub.update(0, total);
                if let Some(d) = Path::new(&dest).parent() {
                    std::fs::create_dir_all(d)?;
                }
                let mut file = std::fs::File::create(&dest)
                    .with_context(|| rcgui_i18n::f("创建 {} 失败", &[&(dest)]))?;
                let mut done = 0u64;
                let mut last = Instant::now();
                loop {
                    match r.chunk().await {
                        Ok(Some(chunk)) => {
                            done += chunk.len() as u64;
                            if done > max {
                                drop(file);
                                let _ = std::fs::remove_file(&dest);
                                hub.finish();
                                bail!(
                                    "{}",
                                    rcgui_i18n::f(
                                        "下载体积异常（超过 {} MB），已中止",
                                        &[&(max / 1024 / 1024)]
                                    )
                                );
                            }
                            std::io::Write::write_all(&mut file, &chunk)?;
                            if last.elapsed() >= Duration::from_millis(120) {
                                last = Instant::now();
                                hub.update(done, total);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            drop(file);
                            let _ = std::fs::remove_file(&dest);
                            hub.finish();
                            return Err(anyhow!("{}", rcgui_i18n::f("下载中断: {}", &[&(e)])));
                        }
                    }
                }
                drop(file);
                hub.update(done, total);
                hub.finish();
                if done == 0 {
                    let _ = std::fs::remove_file(&dest);
                    bail!("{}", rcgui_i18n::f("下载内容为空：{}", &[&(url)]));
                }
                let sha = plugin_host::zip::sha256_file(Path::new(&dest))?;
                Ok(json!({ "path": dest, "bytes": done, "sha256": sha }))
            }

            // 流式上传：把用户在宿主文件对话框里亲手挑过的那个文件发出去。
            // 路径查表、上限、超时、请求头禁令、幂等键都在 `plugin_host::upload`
            // 里，这里只负责把插件 id（宿主注入的 `_plugin`）和参数交过去。
            "http.upload" => {
                let plugin_id = Self::p_str(&params, "_plugin");
                plugin_host::upload::upload(&plugin_id, &params).await
            }

            // ---------------- 进程 ----------------
            "proc.run" => {
                let program = Self::p_str(&params, "program");
                let args: Vec<String> = params
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let timeout = Self::p_u64(&params, "timeout_ms");
                let timeout = Duration::from_millis(if timeout == 0 { 15_000 } else { timeout });
                spawn_blocking(move || {
                    let mut cmd = std::process::Command::new(&program);
                    cmd.args(&args);
                    let out = run_capture(cmd, timeout)?;
                    Ok(out)
                })
                .await
            }
            "proc.start" => {
                let program = Self::p_str(&params, "program");
                let args: Vec<String> = params
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let silent = Self::p_bool(&params, "silent", true);
                let keep = Self::p_bool(&params, "keep_on_exit", false);
                let cwd = Self::p_str(&params, "cwd");
                let log_to = Self::p_str(&params, "log");
                let plugin = Self::p_str(&params, "_plugin");
                spawn_blocking(move || {
                    start_proc(&plugin, &program, &args, silent, keep, &cwd, &log_to)
                })
                .await
            }
            "proc.status" => {
                let handle = Self::p_str(&params, "handle");
                spawn_blocking(move || Ok(proc_status(&handle))).await
            }
            "proc.stop" => {
                let handle = Self::p_str(&params, "handle");
                spawn_blocking(move || Ok(stop_proc(&handle))).await
            }

            // ---------------- 网络 / 环境 ----------------
            "net.port_open" => {
                let port = Self::p_u64(&params, "port") as u16;
                let ms = Self::p_u64(&params, "timeout_ms");
                let ms = if ms == 0 { 600 } else { ms };
                let open = matches!(
                    tokio::time::timeout(
                        Duration::from_millis(ms),
                        tokio::net::TcpStream::connect(("127.0.0.1", port))
                    )
                    .await,
                    Ok(Ok(_))
                );
                Ok(json!({ "open": open }))
            }
            "env.info" => Ok(json!({
                "gui_version": env!("CARGO_PKG_VERSION"),
                "arch": std::env::consts::ARCH,
                "os": "windows",
                "elevated": crate::is_elevated(),
                "data_dir": config::data_dir().display().to_string(),
                "temp_dir": std::env::temp_dir().display().to_string(),
                "localappdata": std::env::var("LOCALAPPDATA").unwrap_or_default(),
                "programfiles": std::env::var("ProgramFiles").unwrap_or_default(),
                "userprofile": std::env::var("USERPROFILE").unwrap_or_default(),
                "process_id": std::process::id(),
            })),

            // ---------------- 交互 ----------------
            "ui.copy" => {
                let text = Self::p_str(&params, "text");
                spawn_blocking(move || {
                    let mut cb = arboard::Clipboard::new()
                        .map_err(|e| anyhow!("{}", rcgui_i18n::f("无法访问剪贴板: {}", &[&(e)])))?;
                    cb.set_text(text)
                        .map_err(|e| anyhow!("{}", rcgui_i18n::f("写入剪贴板失败: {}", &[&(e)])))?;
                    Ok(json!({ "ok": true }))
                })
                .await
            }
            "ui.confirm" => {
                let title = Self::p_str(&params, "title");
                let message = Self::p_str(&params, "message");
                spawn_blocking(move || {
                    let ok = rfd::MessageDialog::new()
                        .set_title(if title.is_empty() {
                            rcgui_i18n::t("请确认")
                        } else {
                            &title
                        })
                        .set_description(&message)
                        .set_buttons(rfd::MessageButtons::YesNo)
                        .set_level(rfd::MessageLevel::Warning)
                        .show()
                        == rfd::MessageDialogResult::Yes;
                    Ok(json!({ "ok": ok }))
                })
                .await
            }

            // 远程名列表：RC 通了就问 RC，否则只读解析 rclone.conf。
            // 面板里只想填一个下拉框，不该因为 RC 没起或认证不对就空白。
            "rclone.remotes" => {
                let mut source = "rc";
                let mut names: Vec<String> = Vec::new();
                let client = self.rc.client().await;
                if let Ok(v) = client.call("config/listremotes", &json!({})).await {
                    for it in v.as_array().cloned().unwrap_or_default() {
                        // rclone 回的是 "名字:类型"，面板里只认裸名字
                        let raw = it.as_str().unwrap_or("").trim().to_string();
                        if let Some(n) = raw.split(':').next() {
                            let n = n.trim();
                            if !n.is_empty() {
                                names.push(n.to_string());
                            }
                        }
                    }
                }
                if names.is_empty() {
                    source = "conf";
                    names = remotes_from_conf();
                }
                names.sort();
                names.dedup();
                Ok(json!({ "remotes": names, "source": source }))
            }

            "rclone.cat" => {
                let remote = Self::p_str(&params, "remote");
                let path = Self::p_str(&params, "path");
                let max = if Self::p_u64(&params, "max_bytes") == 0 {
                    32 * 1024 * 1024
                } else {
                    Self::p_u64(&params, "max_bytes")
                } as usize;
                if remote.trim().is_empty() || path.trim().is_empty() {
                    bail!("{}", rcgui_i18n::t("rclone.cat 需要 remote 与 path"));
                }
                // 该 rclone 版本没有 operations/cat（实测 404），
                // 因此走 copyfile 到唯一临时目录再读回来，和图片直链同一路径。
                // fs 串必须带冒号：不带冒号的串会被当成相对本地目录，
                // copyfile 直接回 `object not found`（实测 404）。
                let fs = if remote.contains(':') {
                    remote.clone()
                } else {
                    format!("{remote}:")
                };
                let dir = config::data_dir()
                    .join("tmp")
                    .join(format!("cat-{}", std::process::id()));
                let client = self.rc.client().await;
                let bytes = rcgui_rc::fetch_to_temp(&client, &fs, &path, &dir)
                    .await
                    .map_err(|e| {
                        anyhow!(
                            "{}",
                            rcgui_i18n::f("读取远程文件失败（{}）: {}", &[&(e.kind.label()), &(e)])
                        )
                    })?;
                let _ = std::fs::remove_dir_all(&dir);
                if bytes.len() > max {
                    bail!(
                        "{}",
                        rcgui_i18n::f(
                            "远程文件超过 {} MB 上限: {}{}",
                            &[&(max / 1024 / 1024), &(fs), &(path)]
                        )
                    );
                }
                Ok(json!({
                    "data_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "size": bytes.len(),
                    "content_type": content_type_of(&path),
                }))
            }
            // 同一个上传，但把结果**带回来**：`ops` 里的 `rclone.upload_file` 是
            // 即发即忘的（`exec_ops` 丢掉返回值），需要直链的插件只能走 need。
            // 行为与 op 完全一致（同一份 `upload_file` 实现），只是返回 `{link,name,deduped}`。
            "rclone.upload_file" => {
                let fs = Self::p_str(&params, "fs");
                let remote_path = Self::p_str(&params, "remote_path");
                if fs.trim().is_empty() || remote_path.trim().is_empty() {
                    bail!(
                        "{}",
                        rcgui_i18n::t("rclone.upload_file 需要 fs 与 remote_path")
                    );
                }
                let filename = Self::p_str(&params, "filename");
                let data_b64 = Self::p_str(&params, "data_b64");
                self.upload_file(fs, remote_path, filename, data_b64).await
            }
            other => bail!("{}", rcgui_i18n::f("宿主不认识的能力: {}", &[&(other)])),
        }
    }
}

// ---------------- 辅助 ----------------

/// 按扩展名猜 Content-Type。
///
/// 插件自己不知道 HTTP 语义，回 `content_type` 时常用得到；
/// 也用在内置 HTTP 入口转发二进制响应时。
pub(crate) fn content_type_of(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn spawn_blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow!("{}", rcgui_i18n::f("后台线程异常: {}", &[&(e)])))?
}

fn meta_of(path: &str) -> Result<Value> {
    let p = Path::new(path);
    let exists = p.exists();
    let md = std::fs::metadata(p).ok();
    Ok(json!({
        "path": path,
        "exists": exists,
        "is_file": md.as_ref().map(|m| m.is_file()).unwrap_or(false),
        "is_dir": md.as_ref().map(|m| m.is_dir()).unwrap_or(false),
        "size": md.as_ref().map(|m| m.len()).unwrap_or(0),
        "modified_ms": md.as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    }))
}

fn short_host(url: &str) -> String {
    url.split('/').nth(2).unwrap_or(url).to_string()
}

/// 启动子进程并登记句柄。
#[allow(clippy::too_many_arguments)]
fn start_proc(
    plugin: &str,
    program: &str,
    args: &[String],
    silent: bool,
    keep: bool,
    cwd: &str,
    log_to: &str,
) -> Result<Value> {
    if !Path::new(program).is_file() {
        bail!("{}", rcgui_i18n::f("找不到可执行文件：{}", &[&(program)]));
    }
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    if silent {
        cmd.stdin(std::process::Stdio::null());
        if log_to.is_empty() {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        } else {
            if let Some(d) = Path::new(log_to).parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_to)
                .ok();
            match f.and_then(|f1| f1.try_clone().map(|f2| (f1, f2)).ok()) {
                Some((a, b)) => {
                    cmd.stdout(std::process::Stdio::from(a))
                        .stderr(std::process::Stdio::from(b));
                }
                None => {
                    cmd.stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                }
            }
        }
        no_window(&mut cmd);
    }
    let child = cmd
        .spawn()
        .with_context(|| rcgui_i18n::f("无法启动 {}", &[&(program)]))?;
    let pid = child.id();
    let handle = format!("{}#{}", plugin, PROC_SEQ.fetch_add(1, Ordering::Relaxed));
    {
        let mut g = procs_lock();
        let map = g.get_or_insert_with(HashMap::new);
        map.insert(
            handle.clone(),
            ProcEntry {
                child,
                started: Instant::now(),
                keep_on_exit: keep,
            },
        );
    }
    // 观察 1 秒：端口冲突这类错误会让进程立刻退出，这里就能给出真实原因
    std::thread::sleep(Duration::from_millis(1000));
    let st = proc_status(&handle);
    if st.get("running").and_then(|v| v.as_bool()) != Some(true) {
        let tail = if log_to.is_empty() {
            String::new()
        } else {
            tail_log(log_to, 4)
        };
        let code = st.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if tail.is_empty() {
            bail!(
                "{}",
                rcgui_i18n::f("进程启动后立刻退出（退出码 {}），没有日志", &[&(code)])
            );
        }
        bail!(
            "{}",
            rcgui_i18n::f("进程启动后立刻退出（退出码 {}）：{}", &[&(code), &(tail)])
        );
    }
    Ok(json!({ "handle": handle, "pid": pid }))
}

fn proc_status(handle: &str) -> Value {
    let mut g = procs_lock();
    let Some(map) = g.as_mut() else {
        return json!({ "running": false, "exit_code": -1 });
    };
    let Some(e) = map.get_mut(handle) else {
        return json!({ "running": false, "exit_code": -1, "known": false });
    };
    let pid = e.child.id();
    match e.child.try_wait() {
        Ok(None) => json!({
            "running": true,
            "pid": pid,
            "uptime_ms": e.started.elapsed().as_millis() as u64,
        }),
        Ok(Some(st)) => json!({
            "running": false,
            "pid": pid,
            "exit_code": st.code().unwrap_or(-1),
        }),
        Err(_) => json!({ "running": false, "pid": pid, "exit_code": -1 }),
    }
}

fn stop_proc(handle: &str) -> Value {
    let entry = {
        let mut g = procs_lock();
        g.as_mut().and_then(|m| m.remove(handle))
    };
    let Some(mut e) = entry else {
        return json!({ "stopped": false });
    };
    let pid = e.child.id();
    let _ = e.child.kill();
    let _ = e.child.wait();
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("taskkill.exe");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        no_window(&mut cmd);
        let _ = cmd.output();
    }
    json!({ "stopped": true, "pid": pid })
}

/// 在目录里按文件名递归找文件（大小写不敏感，广度优先，返回第一个命中）。
///
/// 只做「找文件」这一件通用的事：不判断找的是什么程序、也不执行它。
/// 解压出来的压缩包常见多层嵌套，插件不该自己拼路径猜测。
fn find_file(root: &Path, want_lower: &str, max_depth: usize) -> Option<String> {
    let mut level: Vec<PathBuf> = vec![root.to_path_buf()];
    for _ in 0..max_depth {
        let mut next: Vec<PathBuf> = Vec::new();
        for dir in &level {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    next.push(p);
                    continue;
                }
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                if name == want_lower {
                    return Some(p.display().to_string());
                }
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    None
}

/// 搜索本机可执行文件（如 `rclone.exe`）。
///
/// 扫描策略（快扫，不碰网络盘）：
/// 1. `PATH` 里的每个目录；
/// 2. 常见安装位置（Program Files / Program Files (x86) / LOCALAPPDATA / 用户目录）；
/// 3. 每个**固定盘**根目录下按 `depth` 层广度优先（默认 3 层）。
///
/// 跳过：系统目录（Windows / System32 / WinSxS 等，命中只可能是伪装或误判）、
/// 隐藏/系统属性目录、以及 junction 造成的重复（用规范化路径去重）。
///
/// 硬时限：网络盘或超大目录不能让一次插件调用挂住；超时后返回已找到的结果。
/// 一次搜索最多看多少个目录条目。
///
/// 时限之外再加一道硬上限：某些目录（巨型构建产物、Windows 组件存储）
/// 单次 `read_dir` 就能返回几十万条，光枚举就够把一次点击拖成「卡死」。
/// 超限后**停止向下扩展**，返回已找到的结果。
const SEARCH_MAX_ENTRIES: usize = 200_000;

fn search_exe(want: &str, depth: usize, budget: Duration) -> Vec<String> {
    use std::collections::HashSet;

    let start = Instant::now();
    let want_lower = want.to_ascii_lowercase();
    let mut hits: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut scanned: usize = 0;

    // ---- 1) PATH ----
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';').filter(|s| !s.trim().is_empty()) {
            let p = Path::new(dir).join(want);
            if p.is_file() {
                record_hit(&p, &mut hits, &mut seen);
            }
        }
    }

    // ---- 2) 常见安装位置（只扫这几层，命中率最高） ----
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "LOCALAPPDATA",
        "USERPROFILE",
    ] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                roots.push(PathBuf::from(v));
            }
        }
    }
    // ---- 3) 本地固定盘根目录 ----
    #[cfg(windows)]
    for d in crate::state::local_fixed_drives() {
        roots.push(PathBuf::from(format!("{d}\\")));
    }

    let mut level: Vec<PathBuf> = roots;
    for _ in 0..depth {
        if start.elapsed() >= budget || scanned >= SEARCH_MAX_ENTRIES {
            break;
        }
        let mut next: Vec<PathBuf> = Vec::new();
        for dir in &level {
            if start.elapsed() >= budget || scanned >= SEARCH_MAX_ENTRIES {
                break;
            }
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in rd.flatten() {
                scanned += 1;
                if scanned >= SEARCH_MAX_ENTRIES {
                    break;
                }
                let p = e.path();
                if p.is_dir() {
                    if skip_dir(&p) {
                        continue;
                    }
                    next.push(p);
                    continue;
                }
                if e.file_name().to_string_lossy().to_ascii_lowercase() == want_lower {
                    record_hit(&p, &mut hits, &mut seen);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    hits
}

/// 记下一个命中（规范化路径去重：junction / 8.3 短名会让同一文件出现两次）。
fn record_hit(p: &Path, hits: &mut Vec<String>, seen: &mut std::collections::HashSet<String>) {
    let key = std::fs::canonicalize(p)
        .map(|c| c.display().to_string().to_ascii_lowercase())
        .unwrap_or_else(|_| p.display().to_string().to_ascii_lowercase());
    if seen.insert(key) {
        hits.push(p.display().to_string());
    }
}

/// 搜索时要跳过的目录：系统目录 + 明显不可能放 rclone 的地方。
///
/// 跳过 `Windows` 是为了避免把 `C:\Windows\System32\...` 里的同名伪装文件
/// 当成候选（用户会以为找到了真的 rclone）；跳过 `$Recycle.Bin` 等纯属省时间。
fn skip_dir(p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(
        name.as_str(),
        "windows"
            | "winsxs"
            | "system32"
            | "syswow64"
            | "$recycle.bin"
            | "system volume information"
            | "node_modules"
            | ".git"
            | "temp"
            | "tmp"
            | "cache"
    ) {
        return true;
    }
    // 重解析点（junction / 符号链接）：Windows 上 `Documents and Settings` 指向
    // `Users`、`Application Data` 指回自己。跟随它们会重复遍历同一棵子树，
    // 甚至绕成环 —— 这正是「搜索时栈溢出」的来源。
    if is_reparse_point(p) {
        return true;
    }
    false
}

/// 是否重解析点（符号链接 / junction / 挂载点）。
///
/// 只看 `FILE_ATTRIBUTE_REPARSE_POINT`，不解析目标：目的只是「不跟进去」。
#[cfg(windows)]
fn is_reparse_point(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(p)
        .map(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_reparse_point(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn tail_log(path: &str, max_lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join(" ⏎ ")
}

/// 启动子进程并收集输出（带超时）。
fn run_capture(mut cmd: std::process::Command, timeout: Duration) -> Result<Value> {
    use std::process::Stdio;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| rcgui_i18n::t("无法启动子进程"))?;
    let start = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => bail!("{}", rcgui_i18n::f("等待子进程失败: {}", &[&(e)])),
        }
    }
    if timed_out {
        bail!(
            "{}",
            rcgui_i18n::f("执行超时（{}s）", &[&(timeout.as_secs())])
        );
    }
    let out = child
        .wait_with_output()
        .with_context(|| rcgui_i18n::t("读取子进程输出失败"))?;
    Ok(json!({
        "code": out.status.code().unwrap_or(-1),
        "stdout": String::from_utf8_lossy(&out.stdout).to_string(),
        "stderr": String::from_utf8_lossy(&out.stderr).to_string(),
    }))
}

/// 读 PE 头，返回结构化的架构/子系统信息（不含任何业务判断）。
fn pe_info_json(path: &str) -> Value {
    match pe_info(Path::new(path)) {
        Ok((desc, machine, subsystem, bits)) => json!({
            "ok": true,
            "desc": desc,
            "machine": machine,
            "subsystem": subsystem,
            "bits": bits,
        }),
        Err(e) => json!({ "ok": false, "error": format!("{e}") }),
    }
}

fn pe_info(p: &Path) -> Result<(String, String, String, String)> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).with_context(|| rcgui_i18n::t("打开文件失败"))?;
    let mut head = [0u8; 64];
    let mut read = 0usize;
    while read < head.len() {
        let n = f
            .read(&mut head[read..])
            .with_context(|| rcgui_i18n::t("读取文件头失败"))?;
        if n == 0 {
            break;
        }
        read += n;
    }
    if read < 64 || &head[0..2] != b"MZ" {
        bail!("{}", rcgui_i18n::t("缺少 MZ 头"));
    }
    let e_lfanew = u32::from_le_bytes([head[0x3C], head[0x3D], head[0x3E], head[0x3F]]) as u64;
    if e_lfanew < 64 || e_lfanew > 64 * 1024 * 1024 {
        bail!("{}", rcgui_i18n::t("PE 头偏移异常"));
    }
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(e_lfanew))?;
    let mut sig = [0u8; 4];
    f.read_exact(&mut sig)
        .with_context(|| rcgui_i18n::t("读取 PE 签名失败"))?;
    if &sig != b"PE\0\0" {
        bail!("{}", rcgui_i18n::t("缺少 PE 签名"));
    }
    let mut coff = [0u8; 20];
    f.read_exact(&mut coff)?;
    let machine = u16::from_le_bytes([coff[0], coff[1]]);
    let mut opt_head = [0u8; 2];
    f.read_exact(&mut opt_head)?;
    let magic = u16::from_le_bytes(opt_head);
    f.seek(std::io::SeekFrom::Start(e_lfanew + 24 + 68))?;
    let mut sub = [0u8; 2];
    f.read_exact(&mut sub)?;
    let subsystem = u16::from_le_bytes(sub);
    let arch = match machine {
        0x8664 => "amd64",
        0x014c => "x86",
        0xAA64 => "arm64",
        0x01c0 => "arm",
        _ => "unknown",
    };
    let bits = match magic {
        0x20b => "PE32+",
        0x10b => "PE32",
        _ => "PE?",
    };
    let kind = match subsystem {
        2 => "gui",
        3 => "console",
        _ => "other",
    };
    Ok((
        format!("{bits} / {arch} / {kind}"),
        arch.to_string(),
        kind.to_string(),
        bits.to_string(),
    ))
}

/// 数字签名的 PowerShell 命令。路径只经由环境变量传入，不进脚本源码。
///
/// 路径来自用户选择的文件，里面可以有单引号、分号、换行；这些字符只要出现在脚本文本里
/// 就得靠转义保证不被当代码。环境变量的值 PowerShell 只当数据读，路径怎么长都无所谓。
fn signature_command(path: &str) -> std::process::Command {
    // 固定字符串：脚本内容与路径完全无关，路径再看也不会改变要执行的语句。
    const SCRIPT: &str = "$s=Get-AuthenticodeSignature -LiteralPath $env:RCGUI_SIG_PATH; \
         if($s.Status -eq 'Valid'){'VALID'}else{$s.Status.ToString()}; \
         if($s.SignerCertificate){$s.SignerCertificate.Subject}";
    let mut cmd = std::process::Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("RCGUI_SIG_PATH", path);
    cmd
}

/// 数字签名（PowerShell `Get-AuthenticodeSignature`，拿不到就返回 unknown）。
fn signature_json(path: &str) -> Value {
    let Ok(v) = run_capture(signature_command(path), Duration::from_secs(20)) else {
        return json!({ "status": "unknown", "signer": "" });
    };
    let text = v.get("stdout").and_then(|x| x.as_str()).unwrap_or("");
    let mut lines = text.lines().map(|l| l.trim()).filter(|l| !l.is_empty());
    let status = lines.next().unwrap_or("").to_string();
    let signer = lines.next().unwrap_or("").to_string();
    json!({
        "status": if status.is_empty() { "unknown".to_string() } else { status },
        "signer": signer,
    })
}

fn chrono_ts() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}{}", now.as_millis(), now.subsec_nanos())
}

#[cfg(test)]
mod search_tests {
    use super::*;

    /// 搜索本身不能崩：这是回归测试 —— 之前点「搜索本机」会让进程栈溢出。
    #[test]
    fn search_exe_terminates_and_finds_nothing_for_bogus_name() {
        let got = search_exe(
            "definitely-not-a-real-exe-xyz.exe",
            2,
            Duration::from_millis(1500),
        );
        assert!(got.is_empty(), "不存在的名字不该有命中");
    }

    #[test]
    fn search_exe_respects_budget() {
        let t = Instant::now();
        let _ = search_exe("rclone.exe", 3, Duration::from_millis(800));
        assert!(
            t.elapsed() < Duration::from_secs(6),
            "搜索必须有硬时限，实际 {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn skip_dir_rejects_system_and_temp() {
        assert!(skip_dir(Path::new("C:\\Windows")));
        assert!(skip_dir(Path::new("C:\\Windows\\System32")));
        assert!(skip_dir(Path::new("C:\\Users\\x\\AppData\\Local\\Temp")));
        assert!(!skip_dir(Path::new("C:\\Program Files\\Rclone")));
    }

    /// 真实场景复现：插件那次调用是 depth=3 / 8000ms。
    /// 关键差异是**在 tokio worker 线程（2MB 默认栈）上跑**，测试主线程栈更大。
    #[test]
    fn search_exe_on_tokio_blocking_pool_does_not_overflow() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let out = tokio::task::spawn_blocking(|| {
                search_exe("rclone.exe", 3, Duration::from_millis(8000))
            })
            .await
            .unwrap();
            // 本机装了 rclone，应该至少命中一个；这里只断言「没崩、能返回」
            let _ = out;
        });
    }

    /// 同样跑在 worker 线程上，但故意用更深/更长时间的快照参数。
    #[test]
    fn search_exe_deep_on_tokio_worker_returns() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _ = tokio::task::spawn_blocking(|| {
                search_exe("rclone.exe", 6, Duration::from_millis(25000))
            })
            .await
            .unwrap();
        });
    }

    /// 能力层不许认识任何具体远程类型：这类业务现在全在各自插件里。
    ///
    /// 守的是分工。这里只要出现一个远程类型名或它的写桥，就意味着宿主又在替那个
    /// 插件实现协议——而插件停用后这段代码依然在，于是「专用远程被当普通 WebDAV
    /// 访问」的老问题就回来了。所以断言的是「一整片都不许出现」，不是「只许出现在某处」。
    #[test]
    fn the_capability_layer_knows_no_remote_type() {
        let src = include_str!("caps.rs").replace("\r\n", "\n");
        let prod = src
            .split("\n#[cfg(test)]\nmod search_tests {")
            .next()
            .unwrap_or_default();
        assert!(
            prod.len() > 1000,
            "扫描基准不对（切分标记变了），先修这个用例"
        );
        for bad in [
            "kvault",
            "k-vault",
            "kv_bridge",
            "kv_cfg_lookup",
            "kv_write_op",
            "kv_plugin_gone",
            "站点",
        ] {
            let hit: Vec<&str> = prod.lines().filter(|l| l.contains(bad)).collect();
            assert!(hit.is_empty(), "能力层里不该出现 {bad}：{hit:?}");
        }
    }

    /// 路径只许出现在环境变量里，不许进 PowerShell 的脚本文本。
    ///
    /// 以前路径是拼进脚本字符串的，靠把单引号翻倍兜底；这条守的是更简单的事实：
    /// 脚本内容与路径无关，所以引号、分号、换行、`$()` 都不可能是代码。
    #[test]
    fn signature_command_never_inlines_the_path() {
        let hostile = "C:\\tmp\\a'; Remove-Item C:\\ -Recurse; '\n$(calc.exe)\"& whoami";
        let cmd = signature_command(hostile);

        let env = cmd
            .get_envs()
            .find(|(k, _)| k.to_string_lossy() == "RCGUI_SIG_PATH")
            .expect("必须通过 RCGUI_SIG_PATH 传路径");
        assert_eq!(env.1.unwrap().to_string_lossy().as_ref(), hostile);

        for a in cmd.get_args() {
            let s = a.to_string_lossy();
            assert!(!s.contains(hostile), "参数里不该有路径: {s}");
            assert!(!s.contains("Remove-Item"), "参数里不该有路径片段: {s}");
            assert!(!s.contains("calc.exe"), "参数里不该有路径片段: {s}");
        }

        // 换成另一个路径，命令行完全不变——说明路径没参与拼接
        let other = signature_command("D:\\x\\b.exe");
        assert_eq!(
            cmd.get_args().map(|a| a.to_owned()).collect::<Vec<_>>(),
            other.get_args().map(|a| a.to_owned()).collect::<Vec<_>>()
        );
    }

    /// 上传授权只认真实存在的普通文件，以及插件能叫得上名字的字段。
    ///
    /// 授权表是插件唯一能拿到的路径来源，所以入口不能宽松：空插件 id、带空格或冒号的
    /// 字段名、不存在的路径、目录，全都要当场拒掉；拒得晚了，插件就多了一条读盘的路。
    #[test]
    fn grant_accepts_a_real_file_and_refuses_everything_else() {
        let dir = std::env::temp_dir().join(format!("rcgui-grant-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("picked.txt");
        std::fs::write(&file, b"x").unwrap();

        assert!(
            plugin_host::upload::grant("caps-grant-test", "avatar", &file).is_ok(),
            "普通文件 + 合法字段名应当被接受"
        );
        // 授权表在进程里是全局的：用完就撤，免得影响同一进程里别的用例。
        plugin_host::upload::revoke_plugin("caps-grant-test");

        assert!(
            plugin_host::upload::grant("", "avatar", &file).is_err(),
            "空插件 id 不能被接受"
        );
        for bad in ["a b", "a:b", ""] {
            let r = plugin_host::upload::grant("caps-grant-test", bad, &file);
            assert!(r.is_err(), "字段名 {bad:?} 不合法，不该进授权表");
            let msg = format!("{:#}", r.err().unwrap());
            assert!(
                msg.contains("UPLOAD_BAD_PARAM"),
                "报错要带错误码，宿主排查时才认得出是哪一类：{msg}"
            );
        }
        assert!(
            plugin_host::upload::grant("caps-grant-test", "avatar", &dir.join("not-there.txt"))
                .is_err(),
            "不存在的路径不能被接受"
        );
        assert!(
            plugin_host::upload::grant("caps-grant-test", "avatar", &dir).is_err(),
            "目录不是可上传的文件"
        );
        plugin_host::upload::revoke_plugin("caps-grant-test");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 面板那一栏的授权只能在「用户真的挑到了文件」之后写。
    ///
    /// 守两件事：`fs.pick_file` 必须从参数里取 `form_key` 和宿主注入的 `_plugin`；
    /// 授权调用必须落在那句空值判断里面——否则用户点「取消」同样会往表里塞一个空路径，
    /// 下一次 `http.upload` 就会拿它去读盘。
    #[test]
    fn pick_file_grants_only_after_a_real_pick() {
        let src = include_str!("caps.rs").replace("\r\n", "\n");
        let prod = src
            .split("\n#[cfg(test)]\nmod search_tests {")
            .next()
            .expect("文件里应当有测试模块");

        let arm = prod
            .split("\"fs.pick_file\" => {")
            .nth(1)
            .expect("fs.pick_file 分支应当存在")
            .split("\"fs.pick_dir\" => {")
            .next()
            .expect("fs.pick_file 分支应当以 fs.pick_dir 开头收尾");

        assert!(
            arm.contains("Self::p_str(&params, \"form_key\")"),
            "宿主得知道插件问的是面板里哪一栏"
        );
        assert!(
            arm.contains("Self::p_str(&params, \"_plugin\")"),
            "插件 id 由宿主注入，这里要读出来"
        );

        let guard = arm
            .find("if !path.is_empty() && !form_key.is_empty() {")
            .expect("授权必须写在『挑到了文件』之后");
        let grant = arm
            .find("plugin_host::upload::grant(&plugin_id, &form_key, Path::new(&path))")
            .expect("挑到文件后要记进上传授权表");
        assert!(
            guard < grant,
            "grant 必须落在那句空值判断里面，取消时不能写表"
        );
    }

    /// 能力表（`plugin_host::config::cap_of_op`）里列出的每个 op，宿主都得真能处理。
    ///
    /// 表是插件作者唯一的说明书：表里写着、宿主却没人认，插件要跑到那一行才报
    /// 「宿主不认识的能力」，看着像插件自己写错了。`cap_of_op` 是个 match、没有
    /// 可枚举的公开清单，所以这里按源码文本核对，两边任何一侧挪了地方都会红。
    #[test]
    fn every_op_in_the_capability_table_is_handled_somewhere() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let read = |rel: &str| {
            std::fs::read_to_string(crates.join(rel)).unwrap_or_else(|e| {
                panic!("读不到 {}（{}）：文件挪了位置就要改这个测试的路径", rel, e)
            })
        };

        // 表里的 op。`fs.write_file` 之外还有 check.sha256 这类文件名与路径字面量，
        // 交给 `cap_of_op` 自己筛——它回 `None` 的就不是 op。
        let table = read("plugin/src/config.rs");
        let body = table
            .split("pub fn cap_of_op(op: &str) -> Option<&'static str> {")
            .nth(1)
            .expect("能力表应当还写在 config.rs 的 cap_of_op 里")
            .split("\n}")
            .next()
            .expect("能力表函数体应当以单独的 } 收尾");

        let mut ops: Vec<&str> = Vec::new();
        let mut rest = body;
        while let Some(start) = rest.find('"') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('"') else { break };
            let op = &after[..end];
            if plugin_host::config::cap_of_op(op).is_some() {
                ops.push(op);
            }
            rest = &after[end + 1..];
        }
        ops.sort_unstable();
        ops.dedup();
        assert!(
            ops.len() > 30,
            "只解析出 {} 个 op，表的结构可能变了：{:?}",
            ops.len(),
            ops
        );

        // 宿主的处理点有两处：`cap_call` 的 match 臂，以及 `plugin_host` 自己接、
        // 根本不经过 `cap_call` 的那几个（`kv.get_all`/`kv.set`/`kv.delete`/
        // `ui.toast`/`rclone.call`）。
        let host = include_str!("caps.rs").replace("\r\n", "\n");
        let host = host
            .split("\n#[cfg(test)]\nmod search_tests {")
            .next()
            .expect("文件里应当有测试模块");
        let lib = read("plugin/src/lib.rs").replace("\r\n", "\n");
        let lib = lib.split("\n#[cfg(test)]").next().unwrap_or(&lib);

        // 已知且有意保留的例外：`kv.get` 在权限表（`config.rs`）和
        // `docs/PLUGIN.md` 的权限表里都出现过，但从来没有实现过
        // （`git log -S'"kv.get" =>'` 零命中），正文 API 清单里也没有它。凭空给它
        // 定一个返回形状比诚实报错更糟，留在这里等表的所有者决定：删掉，还是
        // 定义成单键读。
        const KNOWN_GAPS: &[&str] = &["kv.get"];

        let missing: Vec<&&str> = ops
            .iter()
            .filter(|op| !KNOWN_GAPS.contains(*op))
            .filter(|op| {
                let quoted = format!("\"{op}\"");
                !host.contains(&quoted) && !lib.contains(&quoted)
            })
            .collect();
        assert!(
            missing.is_empty(),
            "这些 op 在能力表里，但宿主和插件运行时都没有处理：{:?}",
            missing
        );
    }
}
