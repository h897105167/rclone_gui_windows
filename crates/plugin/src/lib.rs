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

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;
use wasmtime::{Config, Engine, Module};

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

/// 宿主能力提供者（由 app 层实现，桥接 rclone 与 kv 存储）
#[async_trait]
pub trait CapProvider: Send + Sync {
    async fn rclone_call(&self, method: String, params: Value) -> Result<Value>;
    /// 写临时文件 → operations/copyfile 上传 → 清理临时文件
    async fn upload_file(&self, fs: String, remote_path: String, filename: String, data_b64: String) -> Result<Value>;
    async fn kv_get_all(&self, plugin_id: &str) -> Result<Value>;
    async fn kv_set(&self, plugin_id: &str, key: String, value: Value) -> Result<()>;
    async fn kv_delete(&self, plugin_id: &str, key: String) -> Result<()>;
    async fn log(&self, message: String);
}

pub struct PluginHost {
    engine: Engine,
    cache: Mutex<HashMap<PathBuf, (SystemTime, Module)>>,
}

impl PluginHost {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(false);
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
        let mut provided = Value::Null;
        let mut state = Value::Null;
        for _round in 0..8 {
            let mut req = json!({ "id": 1, "method": method });
            let mut p = params.clone();
            if let Some(obj) = p.as_object_mut() {
                obj.insert("provided".into(), provided.clone());
                obj.insert("state".into(), state.clone());
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
                        "kv.get_all" => caps.kv_get_all(plugin_id).await?,
                        "rclone.call" => {
                            let m = n.params.get("method").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            let pr = n.params.get("params").cloned().unwrap_or(Value::Null);
                            caps.rclone_call(m, pr).await?
                        }
                        other => bail!("插件请求了未知能力: {other}"),
                    };
                    prov.insert(key, v);
                }
                provided = Value::Object(prov);
                state = resp.state;
                continue;
            }
            if !resp.ops.is_empty() {
                let caps = caps.as_ref().ok_or_else(|| anyhow!("插件返回 ops 但没有能力提供者"))?;
                exec_ops(&resp.ops, caps, plugin_id).await?;
            }
            if let Some(result) = resp.result {
                return Ok(result);
            }
            bail!("插件未返回 result");
        }
        bail!("插件能力协商轮次超限（8）")
    }
}

async fn exec_ops(ops: &[Value], caps: &std::sync::Arc<dyn CapProvider>, plugin_id: &str) -> Result<()> {
    for op in ops {
        let kind = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let s = |k: &str| op.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        match kind {
            "rclone.call" => {
                let m = s("method");
                let p = op.get("params").cloned().unwrap_or(Value::Null);
                caps.rclone_call(m, p).await?;
            }
            "rclone.upload_file" => {
                caps.upload_file(s("fs"), s("remote_path"), s("filename"), s("data_b64")).await?;
            }
            "kv.set" => caps.kv_set(plugin_id, s("key"), op.get("value").cloned().unwrap_or(Value::Null)).await?,
            "kv.delete" => caps.kv_delete(plugin_id, s("key")).await?,
            "log.write" => caps.log(s("message")).await,
            other => bail!("未知插件 op: {other}"),
        }
    }
    Ok(())
}

async fn run_module(engine: &Engine, module: &Module, stdin: Vec<u8>) -> Result<Vec<u8>> {
    let engine = engine.clone();
    let module = module.clone();
    tokio::task::spawn_blocking(move || run_module_sync(&engine, &module, stdin))
        .await
        .map_err(|e| anyhow!("wasm 运行线程异常: {e}"))?
}

fn run_module_sync(engine: &Engine, module: &Module, stdin: Vec<u8>) -> Result<Vec<u8>> {
    use wasmtime::Linker;
    use wasmtime_wasi::pipe::{MemoryInputPipe, MemoryOutputPipe};
    use wasmtime_wasi::preview1::{self, WasiP1Ctx};
    use wasmtime_wasi::WasiCtxBuilder;

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
    let instance = linker.instantiate(&mut store, module)?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| anyhow!("插件缺少 _start 导出: {e}"))?;
    if let Err(e) = start.call(&mut store, ()) {
        let err_bytes = stderr.contents();
        let err_text = String::from_utf8_lossy(&err_bytes);
        bail!("插件运行失败: {e}\n插件 stderr: {err_text}");
    }
    Ok(stdout.contents().to_vec())
}
