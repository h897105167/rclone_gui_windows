//! WASM 插件宿主（wasmtime preview1）。
//!
//! 通信协议（stdin/stdout 各一个 JSON 文本，UTF-8）：
//!
//! 宿主 → 插件 stdin：
//! ```json
//! {"id":1,"method":"manifest"}
//! {"id":2,"method":"request","params":{"http":{...},"provided":{...}|null,"state":{...},"now":{...},"req_id":"..."}}
//! ```
//!
//! 插件 → 宿主 stdout：
//! ```json
//! {"id":1,"manifest":{"id":"imagehost","name":"图床","version":"0.2.0","description":"..."}}
//! {"id":2,"need":[{"cap":"kv.get_all","key":"kv"},{"cap":"rclone.call","key":"dir_list","params":{"method":"operations/list","params":{...}}}],"state":{...}}
//! {"id":2,"ops":[{"op":"rclone.upload_file","fs":"...","remote_path":"...","filename":"...","data_b64":"..."},{"op":"kv.set","key":"index","value":{...}}],"result":{"status":200,"body":{...}}}
//! ```
//!
//! 插件是无状态 WASI 命令：需要宿主能力时先发 `need`，宿主执行后带
//! `provided` 再次运行同一 wasm，插件根据 `state` 续算，直到产出 `ops`+`result`。
//!
//! 能力分两类：
//!
//! - **通用能力**（`fs.*` / `http.*` / `proc.*` / `net.*` / `kv.*` / `ui.*` / `env.info`）：
//!   名字到能力组的映射见 [`config::cap_of_op`]。插件必须在 `config.json` 的
//!   `capabilities` 里声明对应组，否则宿主拒绝执行——拒绝发生在宿主侧，
//!   插件无法绕过。
//! - **旧版专用能力**（`kv.get_all` / `rclone.call`）：上一代插件直接用，
//!   没有声明机制，调用方传空 `allowed` 表示不限制。

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;
use wasmtime::{Config, Engine, Module};

pub mod bundle;
pub mod config;
pub mod package;
pub mod zip;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginManifest {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NeedReq {
    #[serde(default)]
    pub cap: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PluginResponse {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub need: Vec<NeedReq>,
    #[serde(default)]
    pub state: Value,
    #[serde(default)]
    pub ops: Vec<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

/// 宿主能力提供者（由 app 层实现，桥接 rclone、kv 存储与操作系统原语）
#[async_trait]
pub trait CapProvider: Send + Sync {
    async fn rclone_call(&self, method: String, params: Value) -> Result<Value>;
    /// 写临时文件 → operations/copyfile 上传 → 清理临时文件
    async fn upload_file(
        &self,
        fs: String,
        remote_path: String,
        filename: String,
        data_b64: String,
    ) -> Result<Value>;
    async fn kv_get_all(&self, plugin_id: &str) -> Result<Value>;
    async fn kv_set(&self, plugin_id: &str, key: String, value: Value) -> Result<()>;
    async fn kv_delete(&self, plugin_id: &str, key: String) -> Result<()>;
    async fn log(&self, message: String);

    /// 通用能力调用（`fs.*` / `http.*` / `proc.*` / `net.*` / `kv.*` / `ui.*` / `env.*`）。
    ///
    /// `op` 是点分操作名，`params` 是插件给的参数，返回值直接作为该轮 `provided`
    /// 里对应 `key` 的内容。**能力授权由 [`PluginHost::invoke`] 在本方法之前完成**，
    /// 实现方不需要再检查一遍。
    ///
    /// 默认实现返回错误：让不关心新能力的旧实现（以及测试桩）继续可用。
    async fn cap_call(&self, op: &str, params: Value) -> Result<Value> {
        Err(anyhow!(
            "{}",
            rcgui_i18n::f("宿主未实现能力: {}（参数 {}）", &[&(op), &(params)])
        ))
    }

    /// 让宿主弹一个通知（插件主动推送，不需要返回值）。
    async fn toast(&self, _level: &str, _message: String) {}

    /// 宿主当前正在执行的长任务进度，形如
    /// `{"busy":true,"label":"正在下载…","done":123,"total":456}`。
    ///
    /// 宿主每一轮都会把它填进 `params.live`，插件无需额外 `need`
    /// 就能在 `snapshot` 里回显进度（长任务执行期间不受协商轮次限制）。
    fn live(&self) -> Value {
        Value::Null
    }
}

/// 能力授权表：本次调用允许的能力组。
///
/// - `Unrestricted`：旧格式插件 / 测试，不做限制
/// - `Groups`：只允许列出的组（来自 `config.json` 的 `capabilities`）
#[derive(Debug, Clone, Default)]
pub enum Allowed {
    #[default]
    Unrestricted,
    Groups(Vec<String>),
}

impl Allowed {
    /// 从配置里的能力组构造。
    pub fn from_groups(groups: &[String]) -> Self {
        if groups.is_empty() {
            // 空列表是"什么都没声明"，不是"不限制"：拿不到任何通用能力。
            Allowed::Groups(Vec::new())
        } else {
            Allowed::Groups(groups.to_vec())
        }
    }

    /// 检查某个操作是否被允许；返回缺失的能力组（便于报错时告诉用户改配置）。
    pub fn deny_reason(&self, op: &str) -> Option<String> {
        let group = config::cap_of_op(op);
        match self {
            Allowed::Unrestricted => None,
            Allowed::Groups(groups) => {
                let Some(g) = group else {
                    return Some(rcgui_i18n::f("宿主不认识的能力: {}", &[&(op)]));
                };
                if groups.iter().any(|c| c == g) {
                    None
                } else {
                    Some(rcgui_i18n::f(
                        "插件未在 config.json 的 capabilities 里声明「{}」，拒绝执行 {}",
                        &[&(g), &(op)],
                    ))
                }
            }
        }
    }

    fn check(&self, op: &str) -> Result<()> {
        match self.deny_reason(op) {
            None => Ok(()),
            Some(why) => bail!("{why}"),
        }
    }
}

/// 任何单个动作允许的最大「need → 重新执行」轮次（硬上限）。
///
/// 插件是无状态的：每一轮都是全新进程，只能靠 `state` 把中间结果带回来。
/// 下载并安装一个 rclone 要走「解析版本 / 下载 / 解压 / 找 exe / 校验 / 复制」
/// 六步，再留几轮给可选的二次探测。带重试和回滚的更新流程实测要 13 轮左右，
/// 所以上限给到 32：够用，仍然是有限值（每次调用另有 fuel 和超时兜底）。
pub const MAX_ROUNDS: usize = 32;

/// 没被 [`round_budget`] 特别点名的动作拿到的轮次。
///
/// 旧插件一直按 16 轮跑，默认值保持不变：宿主升级后不该有插件突然报「轮次超限」。
pub const DEFAULT_ROUNDS: usize = 16;

/// 单轮的墙钟上限。插件可能在 wasm 里做网络/文件操作，卡住时界面不能跟着卡。
pub const ROUND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// 整个动作（所有轮次加起来）的墙钟上限。
pub const ACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// 这个动作能用几轮。
///
/// 更新类动作要走「查最新版 → 下载 zip → 校验 zip → 解压 → 校验 exe →
/// 备份旧版 → 替换 → 重启 → 复查」并允许失败重试，给足 [`MAX_ROUNDS`]；
/// 其余动作维持 [`DEFAULT_ROUNDS`]，与升级前一致。`manifest` 一轮就返回。
///
/// 只看动作名里的关键词（`update` / `install` / `upgrade` / `repair`），
/// 不认具体插件：插件可以自由命名方法，宿主不能替它们枚举。
pub fn round_budget(method: &str) -> usize {
    let m = method.to_ascii_lowercase();
    if m == "manifest" {
        1
    } else if ["update", "install", "upgrade", "repair"]
        .iter()
        .any(|k| m.contains(k))
    {
        MAX_ROUNDS
    } else {
        DEFAULT_ROUNDS
    }
}

/// 编译好的模块最多缓存几个。
///
/// 一个 wasm 模块编译后在内存里是机器码，不是几百 KB 的字节码：装十个插件
/// 就全留在内存里，用户看到的任务管理器数字会莫名其妙地高。插件调用是低频
/// 动作（点一次按钮才会跑一次），重编译的代价可以接受，所以缓存只当热路径。
pub const MAX_CACHED_MODULES: usize = 8;

/// 模块多久没用就从缓存里丢掉。
///
/// 只按个数限制不够：用户装 3 个插件、只用一个，另外两个的代码会一直留着。
pub const CACHE_IDLE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

struct Cached {
    /// 编译时的文件 mtime。文件一变就作废（热插拔/升级后必须重新编译）。
    mtime: SystemTime,
    module: Module,
    /// 上次用到的时刻，用于空闲回收。
    used: std::time::Instant,
    /// wasm 文件字节数。只用于汇报缓存规模，不参与淘汰决策。
    bytes: u64,
}

/// 进程级的缓存规模计数。
///
/// 为什么要有全局副本：`--diagnostics` 是无界面进程，它手里没有 `PluginHost`
/// （建一个要初始化 wasmtime 引擎），但它仍然要能报出"现在缓存里压了多少"。
/// 界面进程在缓存变化时顺手更新这两个数，无界面进程直接读。
static CACHE_MODULES: AtomicUsize = AtomicUsize::new(0);
static CACHE_BYTES: AtomicU64 = AtomicU64::new(0);

/// 重新发布缓存规模。调用方必须已经拿到锁。
fn publish_cache(guard: &HashMap<PathBuf, Cached>) {
    CACHE_MODULES.store(guard.len(), Ordering::Relaxed);
    CACHE_BYTES.store(guard.values().map(|c| c.bytes).sum(), Ordering::Relaxed);
}

/// 任意时刻都能读的缓存规模（模块个数、wasm 文件字节数）。
///
/// 无界面进程读到的是 0——它确实没编译过任何模块，这不是错误。
pub fn cache_stats_global() -> (usize, u64) {
    (
        CACHE_MODULES.load(Ordering::Relaxed),
        CACHE_BYTES.load(Ordering::Relaxed),
    )
}

pub struct PluginHost {
    engine: Engine,
    cache: Mutex<HashMap<PathBuf, Cached>>,
}

impl PluginHost {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        // 启用 fuel 计量：插件是外部代码，必须有执行上限。
        // 关闭时一个死循环插件会永久占住一个 blocking 线程（无法取消），
        // 拖垮整个插件子系统。这里按"每次调用消耗多少 fuel"计费。
        config.consume_fuel(true);
        Ok(Self {
            engine: Engine::new(&config)?,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// 按 mtime 缓存编译产物（热插拔：文件变了自动重编译）
    async fn module_for(&self, path: &Path) -> Result<Module> {
        let meta = tokio::fs::metadata(path).await.map_err(|e| {
            anyhow!(
                "{}",
                rcgui_i18n::f("读取插件失败 {}: {}", &[&(path.display()), &(e)])
            )
        })?;
        let mtime = meta.modified()?;
        let bytes = meta.len();
        let cached = {
            let mut guard = self.cache.lock().unwrap();
            match guard.get_mut(path) {
                Some(c) if c.mtime == mtime => {
                    c.used = std::time::Instant::now();
                    Some(c.module.clone())
                }
                _ => None,
            }
        };
        if let Some(m) = cached {
            return Ok(m);
        }
        let p = path.to_path_buf();
        let engine = self.engine.clone();
        let module = tokio::task::spawn_blocking(move || Module::from_file(&engine, &p))
            .await
            .map_err(|e| anyhow!("{}", rcgui_i18n::f("wasm 编译线程异常: {}", &[&(e)])))??;
        {
            let mut guard = self.cache.lock().unwrap();
            guard.insert(
                path.to_path_buf(),
                Cached {
                    mtime,
                    module: module.clone(),
                    used: std::time::Instant::now(),
                    bytes,
                },
            );
            evict_locked(&mut guard, path);
            publish_cache(&guard);
        }
        Ok(module)
    }

    /// 丢掉空闲超过 [`CACHE_IDLE`] 的模块，返回丢掉的个数。由轻量模式与定时回收调用。
    pub fn trim_idle(&self) -> usize {
        let mut guard = self.cache.lock().unwrap();
        let before = guard.len();
        guard.retain(|_, c| c.used.elapsed() < CACHE_IDLE);
        publish_cache(&guard);
        before - guard.len()
    }

    /// 清空全部缓存，返回清掉的个数。轻量模式进/出、以及插件安装后都会用。
    pub fn clear_cache(&self) -> usize {
        let mut guard = self.cache.lock().unwrap();
        let n = guard.len();
        guard.clear();
        publish_cache(&guard);
        n
    }

    /// 当前缓存规模（模块个数、原文件字节数）。
    ///
    /// 注意 `bytes` 是 wasm 文件大小，**不是**编译后的内存占用：机器码通常比
    /// 文件大若干倍。这里不给"省了多少 MB"的估算，因为估算出来的数字没人能验。
    pub fn cache_stats(&self) -> (usize, u64) {
        let guard = self.cache.lock().unwrap();
        (guard.len(), guard.values().map(|c| c.bytes).sum())
    }

    /// 加载插件 manifest
    pub async fn load_manifest(&self, wasm: &Path) -> Result<PluginManifest> {
        let out = self.invoke(wasm, "manifest", Value::Null, None, "").await?;
        let m = out.get("manifest").cloned().unwrap_or(Value::Null);
        let manifest: PluginManifest = serde_json::from_value(m)?;
        if manifest.id.is_empty() {
            bail!("{}", rcgui_i18n::t("插件 manifest 缺少 id"));
        }
        Ok(manifest)
    }

    /// 调用插件：多轮协商直到返回 result
    pub async fn invoke(
        &self,
        wasm: &Path,
        method: &str,
        params: Value,
        caps: Option<std::sync::Arc<dyn CapProvider>>,
        plugin_id: &str,
    ) -> Result<Value> {
        self.invoke_with(
            wasm,
            method,
            params,
            caps,
            plugin_id,
            &Allowed::Unrestricted,
        )
        .await
    }

    /// 带能力授权的调用。`allowed` 一般来自 `config.json` 的 `capabilities`。
    pub async fn invoke_with(
        &self,
        wasm: &Path,
        method: &str,
        params: Value,
        caps: Option<std::sync::Arc<dyn CapProvider>>,
        plugin_id: &str,
        allowed: &Allowed,
    ) -> Result<Value> {
        let mut provided = Value::Null;
        let mut state = Value::Null;
        let rounds = round_budget(method);
        let deadline = std::time::Instant::now() + ACTION_TIMEOUT;
        for _round in 0..rounds {
            if std::time::Instant::now() >= deadline {
                bail!(
                    "{}",
                    rcgui_i18n::f(
                        "插件动作 {} 超过 {} 秒仍未结束，已放弃等待（已完成 {} 轮）",
                        &[&(method), &(ACTION_TIMEOUT.as_secs()), &(_round)]
                    )
                );
            }
            let mut req = json!({ "id": 1, "method": method });
            let mut p = params.clone();
            if let Some(obj) = p.as_object_mut() {
                obj.insert("provided".into(), provided.clone());
                obj.insert("state".into(), state.clone());
                // 宿主当前进度：插件可以在不回传 need 的情况下回显
                if let Some(c) = caps.as_ref() {
                    obj.insert("live".into(), c.live());
                }
                // 宿主界面语言（`zh` / `en`）：插件自带译文，只按这个标签挑语言，
                // 缺对应语言时回落英文。放在 params 根上而不是 `now` 里，
                // 这样 manifest 调用（不经过 app 的 now_json）也能拿到。
                obj.insert(
                    "lang".into(),
                    Value::String(rcgui_i18n::lang().tag().to_string()),
                );
            }
            req["params"] = p;

            let module = self.module_for(wasm).await?;
            let payload = serde_json::to_vec(&req)?;
            // 超时只放弃等待，不强杀：wasm 跑在独立线程里，宿主没法安全打断它，
            // 那个线程会在插件自己返回或 fuel 耗尽后结束。
            let out = match tokio::time::timeout(ROUND_TIMEOUT, run_module(&self.engine, &module, payload)).await {
                Ok(r) => r?,
                Err(_) => bail!("{}", rcgui_i18n::f("插件单轮执行超过 {} 秒，已放弃等待（插件线程会继续跑到自行返回或 fuel 耗尽）", &[&(ROUND_TIMEOUT.as_secs())])),
            };
            let raw: Value = serde_json::from_slice(&out).map_err(|e| {
                anyhow!(
                    "{}",
                    rcgui_i18n::f(
                        "插件输出解析失败: {}；输出片段: {}",
                        &[
                            &(e),
                            &(String::from_utf8_lossy(&out)
                                .chars()
                                .take(300)
                                .collect::<String>())
                        ]
                    )
                )
            })?;
            if let Some(err) = raw.get("error") {
                bail!(
                    "{}",
                    rcgui_i18n::f(
                        "插件错误: {}",
                        &[&(err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or(rcgui_i18n::t("未知")))]
                    )
                );
            }
            // manifest 响应不走 result 协议，原样返回
            if method == "manifest" {
                return Ok(raw);
            }
            let resp: PluginResponse = serde_json::from_value(raw)?;
            if !resp.need.is_empty() {
                let caps = caps
                    .as_ref()
                    .ok_or_else(|| anyhow!("{}", rcgui_i18n::t("插件请求能力但没有提供者")))?;
                let mut prov = serde_json::Map::new();
                for n in resp.need {
                    let key = if n.key.is_empty() {
                        n.cap.clone()
                    } else {
                        n.key
                    };
                    let v = match n.cap.as_str() {
                        // 旧版专用能力（上一代插件直接调用，未做能力声明）
                        "kv.get_all" => caps.kv_get_all(plugin_id).await?,
                        "rclone.call" => {
                            let m = n
                                .params
                                .get("method")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            let pr = n.params.get("params").cloned().unwrap_or(Value::Null);
                            allowed.check("rclone.call")?;
                            caps.rclone_call(m, pr).await?
                        }
                        other => {
                            allowed.check(other)?;
                            // 让能力实现知道是哪个插件在调用（proc.* 按插件分组登记子进程）
                            let mut p = n.params.clone();
                            if let Some(obj) = p.as_object_mut() {
                                obj.insert("_plugin".into(), Value::String(plugin_id.to_string()));
                            }
                            // 单次能力失败**不中断整轮**：插件经常需要在「预期会失败」的
                            // 调用之间做回退（镜像 404、路径还不存在、进程已退出……）。
                            // 失败以 `{"_error": "..."}` 的形式交给插件自己判断。
                            match caps.cap_call(other, p).await {
                                Ok(v) => v,
                                Err(e) => json!({ "_error": format!("{e:#}") }),
                            }
                        }
                    };
                    prov.insert(key, v);
                }
                provided = Value::Object(prov);
                state = resp.state;
                continue;
            }
            if !resp.ops.is_empty() {
                let caps = caps
                    .as_ref()
                    .ok_or_else(|| anyhow!("{}", rcgui_i18n::t("插件返回 ops 但没有能力提供者")))?;
                exec_ops(&resp.ops, caps, plugin_id, allowed).await?;
            }
            if let Some(result) = resp.result {
                return Ok(result);
            }
            bail!("{}", rcgui_i18n::t("插件未返回 result"));
        }
        bail!(
            "{}",
            rcgui_i18n::f(
                "插件动作 {} 用完了 {} 轮仍未返回结果（硬上限 {} 轮）",
                &[&(method), &(rounds), &(MAX_ROUNDS)]
            )
        )
    }
}

/// 插入后维持缓存上限：先扔空闲太久的，仍超限就扔最久没用的。
///
/// `keep` 是刚插入的那个路径，任何情况下都不扔（否则刚编译完就丢，白编译）。
fn evict_locked(cache: &mut HashMap<PathBuf, Cached>, keep: &Path) {
    let entries: Vec<(PathBuf, std::time::Instant)> =
        cache.iter().map(|(p, c)| (p.clone(), c.used)).collect();
    for p in victims(
        &entries,
        keep,
        MAX_CACHED_MODULES,
        CACHE_IDLE,
        std::time::Instant::now(),
    ) {
        cache.remove(&p);
    }
}

/// 淘汰决策（纯函数，便于用构造的时间点测试）。
///
/// 两步：先按空闲时长淘汰，再按"最久没用"压到 `cap` 以内。`keep` 永不入选。
fn victims(
    entries: &[(PathBuf, std::time::Instant)],
    keep: &Path,
    cap: usize,
    idle: std::time::Duration,
    now: std::time::Instant,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = entries
        .iter()
        .filter(|(p, _)| p.as_path() != keep)
        .filter(|(_, used)| now.duration_since(*used) >= idle)
        .map(|(p, _)| p.clone())
        .collect();
    let alive: Vec<&(PathBuf, std::time::Instant)> = entries
        .iter()
        .filter(|(p, used)| {
            p.as_path() == keep || (now.duration_since(*used) < idle && !out.contains(p))
        })
        .collect();
    if alive.len() <= cap {
        return out;
    }
    let mut live_sorted: Vec<&(PathBuf, std::time::Instant)> = alive
        .into_iter()
        .filter(|(p, _)| p.as_path() != keep)
        .collect();
    live_sorted.sort_by_key(|(_, used)| *used);
    let over = entries.len() - out.len() - cap;
    for (p, _) in live_sorted.into_iter().take(over) {
        out.push(p.clone());
    }
    out
}

async fn exec_ops(
    ops: &[Value],
    caps: &std::sync::Arc<dyn CapProvider>,
    plugin_id: &str,
    allowed: &Allowed,
) -> Result<()> {
    for op in ops {
        let kind = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let s = |k: &str| op.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        match kind {
            "rclone.call" => {
                let m = s("method");
                let p = op.get("params").cloned().unwrap_or(Value::Null);
                allowed.check("rclone.call")?;
                caps.rclone_call(m, p).await?;
            }
            "rclone.upload_file" => {
                allowed.check("rclone.call")?;
                caps.upload_file(s("fs"), s("remote_path"), s("filename"), s("data_b64"))
                    .await?;
            }
            "kv.set" => {
                allowed.check("kv.set")?;
                caps.kv_set(
                    plugin_id,
                    s("key"),
                    op.get("value").cloned().unwrap_or(Value::Null),
                )
                .await?
            }
            "kv.delete" => {
                allowed.check("kv.delete")?;
                caps.kv_delete(plugin_id, s("key")).await?
            }
            "log.write" => caps.log(s("message")).await,
            "ui.toast" => {
                allowed.check("ui.toast")?;
                caps.toast(&s("level"), s("message")).await
            }
            other => {
                allowed.check(other)?;
                // 其余 op 一律转发给通用能力实现（参数为去掉 `op` 字段的整个对象）
                let mut params = op.clone();
                if let Some(obj) = params.as_object_mut() {
                    obj.remove("op");
                    obj.insert("_plugin".into(), Value::String(plugin_id.to_string()));
                }
                caps.cap_call(other, params).await?;
            }
        }
    }
    Ok(())
}

/// 单次插件调用的线程栈大小。
///
/// tokio 的 blocking 池默认只有 **2MB** 栈，而 wasmtime 实例化 + WASI preview1
/// 的调用链本身就要吃掉相当一部分；插件里再嵌一层「搜索 / 解压 / 下载」这类
/// 递归或深调用时，就会把 worker 栈撑爆 —— 现象是进程直接 abort，日志只有
/// `thread 'tokio-rt-worker' has overflowed its stack`，没有任何 Rust 回溯。
/// 给 16MB 是一次性买断这类问题，代价只是每个并发插件调用多占一点虚拟内存。
const PLUGIN_THREAD_STACK: usize = 16 * 1024 * 1024;

async fn run_module(engine: &Engine, module: &Module, stdin: Vec<u8>) -> Result<Vec<u8>> {
    let engine = engine.clone();
    let module = module.clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<u8>>>();
    std::thread::Builder::new()
        .name("rcgui-plugin".into())
        .stack_size(PLUGIN_THREAD_STACK)
        .spawn(move || {
            let _ = tx.send(run_module_sync(&engine, &module, stdin));
        })
        .map_err(|e| anyhow!("{}", rcgui_i18n::f("无法启动插件执行线程: {}", &[&(e)])))?;
    rx.await
        .map_err(|e| anyhow!("{}", rcgui_i18n::f("插件执行线程异常退出: {}", &[&(e)])))?
}

fn run_module_sync(engine: &Engine, module: &Module, stdin: Vec<u8>) -> Result<Vec<u8>> {
    use wasmtime::Linker;
    use wasmtime_wasi::pipe::{MemoryInputPipe, MemoryOutputPipe};
    use wasmtime_wasi::preview1::{self, WasiP1Ctx};
    use wasmtime_wasi::WasiCtxBuilder;

    /// 单次插件调用的 fuel 上限。图床上传/扫描这类操作足够用；
    /// 耗尽即中断执行，避免死循环插件永久占用线程。
    const FUEL_PER_CALL: u64 = 50_000_000_000;

    let mut linker: Linker<WasiP1Ctx> = Linker::new(engine);
    preview1::add_to_linker_sync(&mut linker, |t| t)?;
    let stdout = MemoryOutputPipe::new(128 * 1024 * 1024);
    let stderr = MemoryOutputPipe::new(4 * 1024 * 1024);
    let wasi = WasiCtxBuilder::new()
        .stdin(MemoryInputPipe::new(stdin))
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .args(&["plugin.wasm"])
        .build_p1();
    let mut store = wasmtime::Store::new(engine, wasi);
    store.set_fuel(FUEL_PER_CALL)?;
    let instance = linker.instantiate(&mut store, module)?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| anyhow!("{}", rcgui_i18n::f("插件缺少 _start 导出: {}", &[&(e)])))?;
    if let Err(e) = start.call(&mut store, ()) {
        let err_bytes = stderr.contents();
        let err_text = String::from_utf8_lossy(&err_bytes);
        // fuel 耗尽时 wasmtime 报 "all fuel consumed"；给出更可读的提示
        if format!("{e}").contains("fuel") {
            bail!(
                "{}",
                rcgui_i18n::f(
                    "插件执行超出资源上限，已中止（可能存在死循环或过度计算）\n插件 stderr: {}",
                    &[&(err_text)]
                )
            );
        }
        bail!(
            "{}",
            rcgui_i18n::f("插件运行失败: {}\n插件 stderr: {}", &[&(e), &(err_text)])
        );
    }
    Ok(stdout.contents().to_vec())
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn entry(name: &str, age_secs: u64, now: Instant) -> (PathBuf, Instant) {
        (PathBuf::from(name), now - Duration::from_secs(age_secs))
    }

    #[test]
    fn idle_entries_are_dropped_even_under_cap() {
        let now = Instant::now();
        let rows = vec![entry("a.wasm", 10, now), entry("b.wasm", 3600, now)];
        let v = victims(&rows, Path::new("a.wasm"), 8, CACHE_IDLE, now);
        assert_eq!(v, vec![PathBuf::from("b.wasm")]);
    }

    #[test]
    fn fresh_entries_survive_under_cap() {
        let now = Instant::now();
        let rows = vec![entry("a.wasm", 10, now), entry("b.wasm", 20, now)];
        assert!(victims(&rows, Path::new("a.wasm"), 8, CACHE_IDLE, now).is_empty());
    }

    #[test]
    fn over_cap_drops_least_recently_used_first() {
        let now = Instant::now();
        let rows = vec![
            entry("a.wasm", 1, now),
            entry("b.wasm", 50, now),
            entry("c.wasm", 5, now),
        ];
        let v = victims(&rows, Path::new("a.wasm"), 2, CACHE_IDLE, now);
        assert_eq!(v, vec![PathBuf::from("b.wasm")]);
    }

    #[test]
    fn the_module_being_inserted_is_never_dropped() {
        let now = Instant::now();
        let rows = vec![entry("a.wasm", 0, now), entry("b.wasm", 1, now)];
        let v = victims(&rows, Path::new("a.wasm"), 1, CACHE_IDLE, now);
        assert!(!v.contains(&PathBuf::from("a.wasm")));
        assert_eq!(v, vec![PathBuf::from("b.wasm")]);
    }
}

#[cfg(test)]
mod round_tests {
    use super::*;

    #[test]
    fn update_actions_get_the_hard_ceiling() {
        // 带重试和回滚的更新流程实测要 13 轮，16 轮会卡在中间状态回不来。
        for m in [
            "update.check",
            "update.install",
            "install",
            "upgrade",
            "repair",
        ] {
            assert_eq!(round_budget(m), MAX_ROUNDS, "{m} 应该拿满上限");
        }
        assert!(MAX_ROUNDS >= 13);
    }

    #[test]
    fn anything_else_keeps_the_old_budget() {
        // 旧插件按 16 轮写的：宿主升级不能把它们挤到超限。
        for m in ["create", "test", "state", "copy_dav", "push"] {
            assert_eq!(round_budget(m), DEFAULT_ROUNDS, "{m} 应该保持旧预算");
        }
    }

    #[test]
    fn manifest_finishes_in_one_round() {
        assert_eq!(round_budget("manifest"), 1);
        // 大小写不是约定的一部分：插件把方法名写成 Update.Check 也要按更新类处理。
        assert_eq!(round_budget("Update.Check"), MAX_ROUNDS);
    }
}
