//! 插件 KV 持久化（每个插件一个命名空间，落盘到 `<data>/plugins/<id>/kv.json`）。
//!
//! 插件的能力实现（含 OS 原语）在 [`crate::caps`] 里；这里只管存值。

use crate::config;
use anyhow::Result;
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
