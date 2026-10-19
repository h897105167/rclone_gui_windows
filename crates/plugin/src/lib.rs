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
    async fn upload_file(&self, fs: String, remote_path: String, filename: String, data_b64: String) -> Result<Value>;
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
        Err(anyhow!("宿主未实现能力: {op}（参数 {params}）"))
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
                    return Some(format!("宿主不认识的能力: {op}"));
                };
                if groups.iter().any(|c| c == g) {
                    None
                } else {
                    Some(format!(
                        "插件未在 config.json 的 capabilities 里声明「{g}」，拒绝执行 {op}"
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

/// 单个动作允许的最大「need → 重新执行」轮次。
///
/// 插件是无状态的：每一轮都是全新进程，只能靠 `state` 把中间结果带回来。
/// 下载并安装一个 rclone 要走「解析版本 / 下载 / 解压 / 找 exe / 校验 / 复制」
/// 六步，再留几轮给可选的二次探测，因此给到 16。
const MAX_ROUNDS: usize = 16;
pub struct PluginHost {
    engine: Engine,
    cache: Mutex<HashMap<PathBuf, (SystemTime, Module)>>,
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
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| anyhow!("读取插件失败 {}: {e}", path.display()))?;
        let mtime = meta.modified()?;
        let cached = {
            let guard = self.cache.lock().unwrap();
            guard
                .get(path)
                .filter(|(t, _)| *t == mtime)
                .map(|(_, m)| m.clone())
        };
        if let Some(m) = cached {
            return Ok(m);
        }
        let p = path.to_path_buf();
        let engine = self.engine.clone();
        let module = tokio::task::spawn_blocking(move || Module::from_file(&engine, &p))
            .await
            .map_err(|e| anyhow!("wasm 编译线程异常: {e}"))??;
        self.cache
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), (mtime, module.clone()));
        Ok(module)
    }

    /// 加载插件 manifest
    pub async fn load_manifest(&self, wasm: &Path) -> Result<PluginManifest> {
        let out = self.invoke(wasm, "manifest", Value::Null, None, "").await?;
        let m = out.get("manifest").cloned().unwrap_or(Value::Null);
        let manifest: PluginManifest = serde_json::from_value(m)?;
        if manifest.id.is_empty() {
            bail!("插件 manifest 缺少 id");
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
        self.invoke_with(wasm, method, params, caps, plugin_id, &Allowed::Unrestricted)
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
        for _round in 0..MAX_ROUNDS {
            let mut req = json!({ "id": 1, "method": method });
            let mut p = params.clone();
            if let Some(obj) = p.as_object_mut() {
                obj.insert("provided".into(), provided.clone());
                obj.insert("state".into(), state.clone());
                // 宿主当前进度：插件可以在不回传 need 的情况下回显
                if let Some(c) = caps.as_ref() {
                    obj.insert("live".into(), c.live());
                }
            }
            req["params"] = p;

            let module = self.module_for(wasm).await?;
            let out = run_module(&self.engine, &module, serde_json::to_vec(&req)?).await?;
            let raw: Value = serde_json::from_slice(&out)
                .map_err(|e| anyhow!("插件输出解析失败: {e}；输出片段: {}", String::from_utf8_lossy(&out).chars().take(300).collect::<String>()))?;
            if let Some(err) = raw.get("error") {
                bail!("插件错误: {}", err.get("message").and_then(|m| m.as_str()).unwrap_or("未知"));
            }
            // manifest 响应不走 result 协议，原样返回
            if method == "manifest" {
                return Ok(raw);
            }
            let resp: PluginResponse = serde_json::from_value(raw)?;
            if !resp.need.is_empty() {
                let caps = caps.as_ref().ok_or_else(|| anyhow!("插件请求能力但没有提供者"))?;
                let mut prov = serde_json::Map::new();
                for n in resp.need {
                    let key = if n.key.is_empty() { n.cap.clone() } else { n.key };
                    let v = match n.cap.as_str() {
                        // 旧版专用能力（上一代插件直接调用，未做能力声明）
                        "kv.get_all" => caps.kv_get_all(plugin_id).await?,
                        "rclone.call" => {
                            let m = n.params.get("method").and_then(|x| x.as_str()).unwrap_or("").to_string();
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
                let caps = caps.as_ref().ok_or_else(|| anyhow!("插件返回 ops 但没有能力提供者"))?;
                exec_ops(&resp.ops, caps, plugin_id, allowed).await?;
            }
            if let Some(result) = resp.result {
                return Ok(result);
            }
            bail!("插件未返回 result");
        }
        bail!("插件能力协商轮次超限（{MAX_ROUNDS}）")
    }
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
                caps.upload_file(s("fs"), s("remote_path"), s("filename"), s("data_b64")).await?;
            }
            "kv.set" => {
                allowed.check("kv.set")?;
                caps.kv_set(plugin_id, s("key"), op.get("value").cloned().unwrap_or(Value::Null)).await?
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
        .map_err(|e| anyhow!("无法启动插件执行线程: {e}"))?;
    rx.await.map_err(|e| anyhow!("插件执行线程异常退出: {e}"))?
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
        .map_err(|e| anyhow!("插件缺少 _start 导出: {e}"))?;
    if let Err(e) = start.call(&mut store, ()) {
        let err_bytes = stderr.contents();
        let err_text = String::from_utf8_lossy(&err_bytes);
        // fuel 耗尽时 wasmtime 报 "all fuel consumed"；给出更可读的提示
        if format!("{e}").contains("fuel") {
            bail!("插件执行超出资源上限，已中止（可能存在死循环或过度计算）\n插件 stderr: {err_text}");
        }
        bail!("插件运行失败: {e}\n插件 stderr: {err_text}");
    }
    Ok(stdout.contents().to_vec())
}
