//! 宿主那一半：插件要的能力，按主程序的做法回答。
//!
//! 主程序的实现在 `crates/app/src/caps.rs`，这里用不上：那个 crate 是纯二进制、
//! 没有库目标，`HostCaps` 还要整个 GUI 上下文。所以这里用同样的参数名、同样的
//! 返回形状重新实现一遍，只把最不值得重写的一项（`http.upload`，multipart 发送）
//! 直接委派给 `plugin_host::upload` 里的真实现——和主程序调的是同一份代码。
//!
//! 目的不是在这儿重测宿主，而是让插件的逻辑（拼地址、解析回复、翻页、分批删除）
//! 跑在与生产同样形状的回答上。插件若开始要主程序不提供的能力，这里也一样失败。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use plugin_host::CapProvider;
use serde_json::{json, Map, Value};

/// 代替 `crates/app/src/caps.rs` 的能力提供者。
pub struct MockHost {
    /// `config/get` 的扁平回答，只描述被测的那一条远程。
    remote: Value,
    /// 明文令牌，由 `config/reveal` 交出，相当于主程序把存着的令牌解密。
    token: String,
    kv: Mutex<BTreeMap<String, Value>>,
    logs: Mutex<Vec<String>>,
    /// 按顺序记下每一次能力调用：既能证明插件只拿自己命名空间的数据，也能证明
    /// 某条路径下一次都没调用过。
    calls: Mutex<Vec<String>>,
}

impl MockHost {
    /// 一条 K-Vault 远程：带标记参数，以及它自己的存储与目录设置。
    pub fn kvault(dav_url: &str, token: &str) -> Arc<MockHost> {
        Arc::new(MockHost {
            remote: json!({
                "type": "webdav",
                "url": dav_url,
                "vendor": "other",
                "kvault": "1",
                "bearer_token": token,
                "kvault_storage": "telegram",
                "kvault_folder": "album",
                "kvault_dedup": "1",
            }),
            token: token.to_string(),
            kv: Mutex::new(BTreeMap::new()),
            logs: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// 一条普通 WebDAV 远程：形状相同，没有标记参数。插件必须拒绝动它，而不是猜。
    pub fn plain_webdav(dav_url: &str, user: &str) -> Arc<MockHost> {
        Arc::new(MockHost {
            remote: json!({
                "type": "webdav",
                "url": dav_url,
                "vendor": "other",
                "user": user,
                "pass": "secret",
            }),
            token: String::new(),
            kv: Mutex::new(BTreeMap::new()),
            logs: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// 插件向宿主提过的全部要求。
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// 插件经 `kv.set` 存下的东西，也就是它给每条远程记的备注。
    pub fn kv_snapshot(&self) -> BTreeMap<String, Value> {
        self.kv.lock().unwrap().clone()
    }

    /// 预置插件自己命名空间里的数据：用来模拟「插件已经存过一份设置」。
    /// 只接受对象，因为插件的存储形状就是「每个键一份 JSON」。
    pub fn put_kv(&self, key: &str, value: Value) {
        self.kv.lock().unwrap().insert(key.to_string(), value);
    }

    pub fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().clone()
    }

    fn note(&self, what: impl Into<String>) {
        self.calls.lock().unwrap().push(what.into());
    }
}

#[async_trait]
impl CapProvider for MockHost {
    async fn rclone_call(&self, method: String, params: Value) -> Result<Value> {
        self.note(format!("rclone.call {method}"));
        match method.as_str() {
            // 扁平回答，没有 `params` 外壳——rclone 自己的形状就是这样。
            "config/get" => {
                let wanted = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if wanted.is_empty() {
                    return Err(anyhow!("config/get 缺少 name"));
                }
                Ok(self.remote.clone())
            }
            "config/reveal" => Ok(json!({ "value": self.token })),
            "config/listremotes" => Ok(json!({ "remotes": ["vault"] })),
            other => Err(anyhow!("测试宿主没有实现 rclone 方法 {other}")),
        }
    }

    async fn upload_file(
        &self,
        _fs: String,
        _remote_path: String,
        _filename: String,
        _data_b64: String,
    ) -> Result<Value> {
        // K-Vault 上传走 `http.upload`。这条老路必须保持没被用到，所以这里直接
        // 报错——真有人改回老路子，测试会立刻看见。
        Err(anyhow!("K-Vault 插件不该走 rclone.upload_file"))
    }

    async fn kv_get_all(&self, _plugin_id: &str) -> Result<Value> {
        self.note("kv.get_all");
        let mut out = Map::new();
        for (k, v) in self.kv.lock().unwrap().iter() {
            out.insert(k.clone(), v.clone());
        }
        Ok(Value::Object(out))
    }

    async fn kv_set(&self, _plugin_id: &str, key: String, value: Value) -> Result<()> {
        self.note(format!("kv.set {key}"));
        self.kv.lock().unwrap().insert(key, value);
        Ok(())
    }

    async fn kv_delete(&self, _plugin_id: &str, key: String) -> Result<()> {
        self.note(format!("kv.delete {key}"));
        self.kv.lock().unwrap().remove(&key);
        Ok(())
    }

    async fn log(&self, message: String) {
        self.logs.lock().unwrap().push(message);
    }

    async fn cap_call(&self, op: &str, params: Value) -> Result<Value> {
        self.note(op.to_string());
        match op {
            "http.request" => self.http_request(&params).await,
            "http.text" | "http.json" => self.http_text(&params).await,
            // 唯一不重写的能力：真的 multipart 发送器，与主程序调用的是同一份。
            "http.upload" => {
                let plugin = params
                    .get("_plugin")
                    .and_then(|v| v.as_str())
                    .unwrap_or("kvault");
                plugin_host::upload::upload(plugin, &params).await
            }
            other => Err(anyhow!("测试宿主没有实现能力 {other}")),
        }
    }
}

impl MockHost {
    async fn http_request(&self, params: &Value) -> Result<Value> {
        let url = p_str(params, "url");
        if url.is_empty() {
            return Err(anyhow!("http.request 缺少 url"));
        }
        let method = p_str(params, "method").to_uppercase();
        let method = if method.is_empty() {
            "GET".to_string()
        } else {
            method
        };
        let timeout = match params.get("timeout_ms").and_then(|v| v.as_u64()) {
            Some(ms) if ms > 0 => ms,
            _ => 30_000,
        };
        let cap = match params.get("max_bytes").and_then(|v| v.as_u64()) {
            Some(n) if n > 0 => n as usize,
            _ => 16 * 1024 * 1024,
        };
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_millis(timeout))
            .user_agent(concat!("RCGUI-IT/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let verb = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| anyhow!("http.request 不支持的方法: {method}"))?;
        let mut req = client.request(verb, &url);
        if let Some(headers) = params.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in headers {
                if let Some(s) = v.as_str() {
                    req = req.header(k.as_str(), s);
                }
            }
        }
        let body = p_str(params, "body");
        if !body.is_empty() {
            req = req.body(body);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = resp.bytes().await?;
        if bytes.len() > cap {
            return Err(anyhow!("响应超过 {} 字节上限: {}", cap, bytes.len()));
        }
        Ok(json!({
            "status": status,
            "content_type": ctype,
            "body": String::from_utf8_lossy(&bytes).to_string(),
            "size": bytes.len(),
        }))
    }

    async fn http_text(&self, params: &Value) -> Result<Value> {
        let url = p_str(params, "url");
        let resp = reqwest::Client::new().get(&url).send().await?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await?;
        Ok(json!({ "status": status, "body": String::from_utf8_lossy(&bytes).to_string() }))
    }
}

fn p_str(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}
