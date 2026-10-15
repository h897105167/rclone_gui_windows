//! 插件宿主能力实现：KV 持久化 + rclone 桥接 + 上传桥接

use crate::config;
use anyhow::{anyhow, Result};
use base64::Engine as _;
use plugin_host::CapProvider;
use rclone_client::{local_fs, RcloneManager};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct KvStore {
    inner: Mutex<HashMap<String, HashMap<String, Value>>>,
    dir: PathBuf,
}

impl KvStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            dir,
        }
    }

    fn file_of(&self, plugin_id: &str) -> PathBuf {
        self.dir.join(plugin_id).join("kv.json")
    }

    fn load_ns(&self, plugin_id: &str) -> HashMap<String, Value> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(ns) = guard.get(plugin_id) {
            return ns.clone();
        }
        let ns: HashMap<String, Value> = std::fs::read(self.file_of(plugin_id))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        guard.insert(plugin_id.to_string(), ns.clone());
        ns
    }

    fn save_ns(&self, plugin_id: &str, ns: &HashMap<String, Value>) -> Result<()> {
        let data = serde_json::to_vec_pretty(ns)?;
        config::write_atomic(&self.file_of(plugin_id), &data)?;
        self.inner
            .lock()
            .unwrap()
            .insert(plugin_id.to_string(), ns.clone());
        Ok(())
    }

    pub fn get_all(&self, plugin_id: &str) -> Value {
        Value::Object(self.load_ns(plugin_id).into_iter().collect())
    }

    pub fn set(&self, plugin_id: &str, key: &str, value: Value) -> Result<()> {
        let mut ns = self.load_ns(plugin_id);
        ns.insert(key.to_string(), value);
        self.save_ns(plugin_id, &ns)
    }

    pub fn delete(&self, plugin_id: &str, key: &str) -> Result<()> {
        let mut ns = self.load_ns(plugin_id);
        ns.remove(key);
        self.save_ns(plugin_id, &ns)
    }
}

pub struct RealCaps {
    pub mgr: std::sync::Arc<RcloneManager>,
    pub store: std::sync::Arc<KvStore>,
}

#[async_trait::async_trait]
impl CapProvider for RealCaps {
    async fn rclone_call(&self, method: String, params: Value) -> Result<Value> {
        let client = self.mgr.client.read().await.clone();
        client.call(&method, &params).await
    }

    async fn upload_file(
        &self,
        fs: String,
        remote_path: String,
        _filename: String,
        data_b64: String,
    ) -> Result<Value> {
        let data = base64::engine::general_purpose::STANDARD
            .decode(data_b64.as_bytes())
            .map_err(|e| anyhow!("base64 解码失败: {e}"))?;
        let tmp_dir = config::data_dir().join("tmp");
        std::fs::create_dir_all(&tmp_dir)?;
        let name = format!("up-{}-{}", std::process::id(), chrono_ts());
        let tmp = tmp_dir.join(&name);
        std::fs::write(&tmp, &data)?;
        let client = self.mgr.client.read().await.clone();
        let r = client.copyfile(&local_fs(&tmp_dir), &name, &fs, &remote_path).await;
        let _ = std::fs::remove_file(&tmp);
        r?;
        Ok(serde_json::json!({}))
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
        let dir = config::data_dir().join("plugins").join("imagehost");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("log.txt"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{message}\n").as_bytes()));
    }
}

fn chrono_ts() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}{}", now.as_millis(), now.subsec_nanos())
}
