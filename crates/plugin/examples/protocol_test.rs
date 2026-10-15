//! 插件协议端到端测试：编译好的 imagehost.wasm + 假能力提供者。
//! 用法：cargo run -p plugin_host --example protocol_test
//! 前置：cargo build -p imagehost-plugin --target wasm32-wasip1

use anyhow::Result;
use plugin_host::{CapProvider, PluginHost};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct FakeCaps {
    kv: Mutex<HashMap<String, Value>>,
    list_entries: Vec<Value>,
    uploads: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl CapProvider for FakeCaps {
    async fn rclone_call(&self, method: String, _params: Value) -> Result<Value> {
        if method == "operations/list" {
            return Ok(json!({ "list": self.list_entries }));
        }
        Ok(Value::Null)
    }
    async fn upload_file(&self, fs: String, remote_path: String, _filename: String, _data_b64: String) -> Result<Value> {
        self.uploads.lock().unwrap().push((fs, remote_path));
        Ok(json!({}))
    }
    async fn kv_get_all(&self, plugin_id: &str) -> Result<Value> {
        Ok(Value::Object(
            self.kv.lock().unwrap().get(plugin_id).cloned().unwrap_or(json!({})).as_object().cloned().unwrap_or_default(),
        ))
    }
    async fn kv_set(&self, plugin_id: &str, key: String, value: Value) -> Result<()> {
        let mut kv = self.kv.lock().unwrap();
        let ns = kv.entry(plugin_id.to_string()).or_insert(json!({}));
        ns[key.as_str()] = value;
        Ok(())
    }
    async fn kv_delete(&self, plugin_id: &str, key: String) -> Result<()> {
        if let Some(ns) = self.kv.lock().unwrap().get_mut(plugin_id) {
            if let Some(o) = ns.as_object_mut() {
                o.remove(&key);
            }
        }
        Ok(())
    }
    async fn log(&self, _message: String) {}
}

fn now() -> Value {
    json!({
        "ymd": "20261015",
        "hms": "213000",
        "ms": "123",
        "date_iso": "2026-10-15",
        "iso": "2026-10-15T13:30:00.123Z",
        "epoch_ms": "1792000000123",
    })
}

fn wasm_path() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest)
        .join("../../target/wasm32-wasip1/debug/imagehost.wasm")
}

#[tokio::main]
async fn main() -> Result<()> {
    let wasm = wasm_path();
    assert!(wasm.exists(), "缺少 wasm：{}", wasm.display());
    let host = PluginHost::new()?;

    // 1) manifest
    let m = host.load_manifest(&wasm).await?;
    assert_eq!(m.id, "imagehost");
    println!("[test] manifest ok: {} {} ({})", m.name, m.version, m.description);

    let caps = Arc::new(FakeCaps {
        list_entries: vec![
            json!({"Name": "001.png", "Path": "001.png", "Size": 10, "IsDir": false, "ModTime": "2026-01-01T00:00:00Z"}),
        ],
        ..Default::default()
    });

    // 2) GET /config → 默认配置
    let http = |m: &str, p: &str, body: Value, mp: Option<Value>| {
        json!({
            "http": {
                "method": m, "path": p, "query": {}, "authorization": "",
                "bodyB64": base64_of(&serde_json::to_vec(&body).unwrap_or_default()),
                "multipart": mp, "origin": "http://127.0.0.1:5273",
            },
            "now": now(), "req_id": "deadbeefdeadbeefdeadbeef",
        })
    };
    let r = host.invoke(&wasm, "request", http("GET", "/config", json!({}), None), Some(caps.clone()), "imagehost").await?;
    assert_eq!(r["status"], 200);
    assert!(r["body"]["roots"].as_array().unwrap().len() >= 1);
    println!("[test] GET /config ok");

    // 3) POST /upload（multipart，timestamp 命名）
    let png = b"\x89PNG\r\n\x1a\nfakepngdata";
    let mp = json!({
        "fields": {"subPath": "wallpaper"},
        "files": [{"filename": "cat.png", "dataB64": base64_of(png)}],
    });
    let r = host.invoke(&wasm, "request", http("POST", "/upload", json!({}), Some(mp)), Some(caps.clone()), "imagehost").await?;
    assert_eq!(r["status"], 200, "upload 响应: {r}");
    let item = &r["body"]["items"][0];
    assert_eq!(item["name"], "20261015213000123.png", "实际: {item}");
    assert_eq!(item["storedPath"], "wallpaper/20261015213000123.png");
    assert_eq!(item["url"], "http://127.0.0.1:5273/i/default/wallpaper/20261015213000123.png");
    assert_eq!(item["deduped"], false);
    assert_eq!(caps.uploads.lock().unwrap().len(), 1);
    println!("[test] upload timestamp ok");

    // 4) 重复上传同内容 → 去重
    let mp = json!({
        "fields": {"subPath": "wallpaper"},
        "files": [{"filename": "again.png", "dataB64": base64_of(png)}],
    });
    let r = host.invoke(&wasm, "request", http("POST", "/upload", json!({}), Some(mp)), Some(caps.clone()), "imagehost").await?;
    assert_eq!(r["body"]["items"][0]["deduped"], true, "实际: {r}");
    assert_eq!(caps.uploads.lock().unwrap().len(), 1, "去重后不应再上传");
    println!("[test] dedupe ok");

    // 5) seq 命名：目录已有 001.png → 002.png
    let mp = json!({
        "fields": {"subPath": "", "naming": "seq", "seqPad": "3"},
        "files": [{"filename": "x.png", "dataB64": base64_of(b"other-bytes")}],
    });
    let r = host.invoke(&wasm, "request", http("POST", "/upload", json!({}), Some(mp)), Some(caps.clone()), "imagehost").await?;
    assert_eq!(r["body"]["items"][0]["name"], "002.png", "实际: {r}");
    println!("[test] seq naming ok");

    // 6) obsidian 布局：app=obsidian, vault=MyVault, notePath=日记/2026/abc.md, layout=note
    //    默认配置 dirLayout=note → obsidian/MyVault/日记/2026/
    let mp = json!({
        "fields": {"app": "obsidian", "vault": "MyVault", "notePath": "日记/2026/abc.md"},
        "files": [{"filename": "shot.png", "dataB64": base64_of(b"shot-bytes")}],
    });
    let r = host.invoke(&wasm, "request", http("POST", "/upload", json!({}), Some(mp)), Some(caps.clone()), "imagehost").await?;
    assert_eq!(r["body"]["items"][0]["storedPath"], "obsidian/MyVault/日记/2026/20261015213000123.png", "实际: {r}");
    println!("[test] obsidian layout ok");

    // 7) raw body 模式（Obsidian 插件路径）
    let mut params = http("POST", "/upload", json!({}), None);
    params["http"]["query"] = json!({"filename": "raw.png", "app": "obsidian", "vault": "V", "notePath": "n.md"});
    params["http"]["bodyB64"] = Value::String(base64_of(b"raw-body"));
    let r = host.invoke(&wasm, "request", params, Some(caps.clone()), "imagehost").await?;
    assert_eq!(r["status"], 200, "实际: {r}");
    println!("[test] raw body upload ok");

    // 8) token 流程
    let r = host.invoke(&wasm, "request", http("POST", "/token", json!({"name": "obsidian"}), None), Some(caps.clone()), "imagehost").await?;
    assert_eq!(r["status"], 200);
    let token = r["body"]["token"].as_str().unwrap();
    assert_eq!(token.len(), 24);
    println!("[test] token gen ok: {token}");

    // 9) /files 与 /stats
    let r = host.invoke(&wasm, "request", http("GET", "/files", json!({}), None), Some(caps.clone()), "imagehost").await?;
    assert!(r["body"]["total"].as_u64().unwrap() >= 3);
    let r = host.invoke(&wasm, "request", http("GET", "/stats", json!({}), None), Some(caps.clone()), "imagehost").await?;
    assert!(r["body"]["count"].as_u64().unwrap() >= 3);
    println!("[test] files/stats ok");

    // 10) PUT /config 校验：空 roots → 400
    let r = host.invoke(&wasm, "request", http("PUT", "/config", json!({"roots": []}), None), Some(caps.clone()), "imagehost").await?;
    assert_eq!(r["status"], 400);
    println!("[test] config validation ok");

    println!("[test] ALL OK");
    Ok(())
}

fn base64_of(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}
