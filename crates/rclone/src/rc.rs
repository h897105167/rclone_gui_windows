//! RC API 客户端。rclone v1.75+ 已将 operations/copy 改名为 operations/copyfile、
//! moveto 改名为 operations/movefile，这里对旧名做自动回退兼容。

use crate::types::{parse_mounts, CoreStats, ListEntry, MountInfo, VersionInfo};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct RcClient {
    http: reqwest::Client,
    base_url: String,
    auth: Option<(String, String)>,
}

impl RcClient {
    pub fn new(base_url: impl Into<String>, user: Option<String>, pass: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: match (user, pass) {
                (Some(u), Some(p)) if !u.is_empty() => Some((u, p)),
                _ => None,
            },
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 调用 RC 方法：POST {base}/{method}，返回 JSON。
    pub async fn call(&self, method: &str, params: &Value) -> Result<Value> {
        let url = format!("{}/{}", self.base_url, method);
        let mut req = self.http.post(&url).json(params);
        if let Some((u, p)) = &self.auth {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req.send().await.map_err(|e| anyhow!("连接 rclone 失败: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
            anyhow::bail!("{}: {}", method, msg);
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text).map_err(|e| anyhow!("{} 响应解析失败: {e}", method))?)
    }

    /// 依次尝试多个方法名（新旧版本改名兼容），仅在"方法不存在"时回退。
    pub async fn call_fallback(&self, methods: &[&str], params: &Value) -> Result<Value> {
        let mut last_err = None;
        for m in methods {
            match self.call(m, params).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let msg = e.to_string();
                    let unknown = msg.contains("404")
                        || msg.to_lowercase().contains("unknown")
                        || msg.to_lowercase().contains("not found")
                        || msg.to_lowercase().contains("doesn't exist")
                        || msg.to_lowercase().contains("no such");
                    if unknown {
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("无可用 RC 方法")))
    }

    // ---------- core ----------

    pub async fn version(&self) -> Result<VersionInfo> {
        let v = self.call("core/version", &json!({})).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// 优雅退出 rclone（会自动卸载挂载、停止服务）
    pub async fn core_quit(&self) -> Result<()> {
        self.call("core/quit", &json!({})).await.map(drop)
    }

    pub async fn core_stats(&self) -> Result<CoreStats> {
        let v = self.call("core/stats", &json!({})).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    pub async fn core_transferred(&self) -> Result<Value> {
        self.call("core/transferred", &json!({ "group": "" })).await
    }

    // ---------- config ----------

    /// 远程名统一去掉结尾冒号，调用方拼接 fs 时用 format!("{name}:")
    pub async fn list_remotes(&self) -> Result<Vec<String>> {
        let v = self.call("config/listremotes", &json!({})).await?;
        Ok(v.get("remotes")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim_end_matches(':').to_string()))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn config_dump(&self) -> Result<Value> {
        self.call("config/dump", &json!({})).await
    }

    /// 获取所有后端类型及其参数定义
    pub async fn config_providers(&self) -> Result<Value> {
        self.call("config/providers", &json!({})).await
    }

    /// 新建 remote：parameters 为键值对（type 单独传）
    pub async fn config_create(&self, name: &str, rtype: &str, parameters: Value) -> Result<()> {
        self.call(
            "config/create",
            &json!({ "name": name, "type": rtype, "parameters": parameters }),
        )
        .await
        .map(drop)
    }

    /// 更新 remote 参数（仅覆盖给出的键）
    pub async fn config_update(&self, name: &str, parameters: Value) -> Result<()> {
        self.call("config/update", &json!({ "name": name, "parameters": parameters }))
            .await
            .map(drop)
    }

    pub async fn config_delete(&self, name: &str) -> Result<()> {
        self.call("config/delete", &json!({ "name": name }))
            .await
            .map(drop)
    }

    // ---------- serve ----------

    pub async fn serve_list(&self) -> Result<Value> {
        self.call("serve/list", &json!({})).await
    }

    /// 启动服务（webdav/sftp/http/ftp/dlna/nfs/docker），返回分配的 id
    pub async fn serve_start(
        &self,
        stype: &str,
        fs: &str,
        addr: &str,
        user: &str,
        pass: &str,
    ) -> Result<String> {
        let mut params = json!({ "type": stype, "fs": fs, "addr": addr });
        if !user.is_empty() {
            params["user"] = json!(user);
        }
        if !pass.is_empty() {
            params["pass"] = json!(pass);
        }
        let v = self.call("serve/start", &params).await?;
        Ok(v.get("id").and_then(|x| x.as_str()).unwrap_or_default().to_string())
    }

    pub async fn serve_stop(&self, id: &str) -> Result<()> {
        self.call("serve/stop", &json!({ "id": id })).await.map(drop)
    }

    // ---------- job ----------

    pub async fn job_list(&self) -> Result<Vec<i64>> {
        let v = self.call("job/list", &json!({})).await?;
        Ok(v.get("jobids")
            .and_then(|j| j.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
            .unwrap_or_default())
    }

    pub async fn job_status(&self, jobid: i64) -> Result<Value> {
        self.call("job/status", &json!({ "jobid": jobid })).await
    }

    pub async fn job_stop(&self, jobid: i64) -> Result<()> {
        self.call("job/stop", &json!({ "jobid": jobid })).await.map(drop)
    }

    // ---------- options / bwlimit ----------

    /// 设置带宽限速，如 "1M"、"10M:100K"、"off"
    pub async fn bwlimit(&self, rate: &str) -> Result<String> {
        let v = self.call("core/bwlimit", &json!({ "rate": rate })).await?;
        Ok(v.get("rate").and_then(|x| x.as_str()).unwrap_or(rate).to_string())
    }

    pub async fn options_get(&self) -> Result<Value> {
        self.call("options/get", &json!({})).await
    }

    /// 设置运行时选项：{"main": {"Transfers": 4, ...}}（字段名为 Go 风格大写）
    pub async fn options_set(&self, opts: Value) -> Result<()> {
        self.call("options/set", &opts).await.map(drop)
    }

    // ---------- operations ----------

    pub async fn list(&self, fs: &str, remote: &str, options: Option<Value>) -> Result<Vec<ListEntry>> {
        let mut params = json!({ "fs": fs, "remote": remote });
        if let Some(opts) = options {
            params["options"] = opts;
        }
        let v = self.call("operations/list", &params).await?;
        let list = v.get("list").cloned().unwrap_or_else(|| Value::Array(vec![]));
        Ok(serde_json::from_value(list).unwrap_or_default())
    }

    pub async fn mkdir(&self, fs: &str, remote: &str) -> Result<()> {
        self.call("operations/mkdir", &json!({ "fs": fs, "remote": remote }))
            .await
            .map(drop)
    }

    pub async fn rmdir(&self, fs: &str, remote: &str) -> Result<()> {
        self.call("operations/rmdir", &json!({ "fs": fs, "remote": remote }))
            .await
            .map(drop)
    }

    pub async fn purge(&self, fs: &str, remote: &str) -> Result<()> {
        self.call("operations/purge", &json!({ "fs": fs, "remote": remote }))
            .await
            .map(drop)
    }

    pub async fn deletefile(&self, fs: &str, remote: &str) -> Result<()> {
        self.call("operations/deletefile", &json!({ "fs": fs, "remote": remote }))
            .await
            .map(drop)
    }

    /// 复制文件（v1.75: copyfile；旧版: copy）
    pub async fn copyfile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        self.call_fallback(
            &["operations/copyfile", "operations/copy"],
            &json!({
                "srcFs": src_fs, "srcRemote": src_remote,
                "dstFs": dst_fs, "dstRemote": dst_remote,
            }),
        )
        .await
        .map(drop)
    }

    /// 移动/重命名文件（v1.75: movefile；旧版: moveto）
    pub async fn movefile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        self.call_fallback(
            &["operations/movefile", "operations/moveto"],
            &json!({
                "srcFs": src_fs, "srcRemote": src_remote,
                "dstFs": dst_fs, "dstRemote": dst_remote,
            }),
        )
        .await
        .map(drop)
    }

    pub async fn about(&self, fs: &str) -> Result<Value> {
        self.call("operations/about", &json!({ "fs": fs })).await
    }

    pub async fn publiclink(&self, fs: &str, remote: &str) -> Result<String> {
        let v = self
            .call("operations/publiclink", &json!({ "fs": fs, "remote": remote }))
            .await?;
        Ok(v.get("url")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string())
    }

    // ---------- mount ----------

    /// 挂载。options 为顶层扁平 CLI 风格键（如 vfs_cache_mode/read_only/volname）。
    /// mountPoint 可为 "X:"、"\\\\server\\share" 或 "*"（自动分配盘符，返回实际盘符）。
    pub async fn mount(&self, fs: &str, mount_point: &str, options: Value) -> Result<String> {
        let mut params = json!({ "fs": fs, "mountPoint": mount_point });
        if let (Value::Object(dst), Value::Object(src)) = (&mut params, &options) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        let v = self.call("mount/mount", &params).await?;
        Ok(v.get("mountPoint")
            .and_then(|x| x.as_str())
            .unwrap_or(mount_point)
            .to_string())
    }

    pub async fn unmount(&self, mount_point: &str) -> Result<()> {
        self.call("mount/unmount", &json!({ "mountPoint": mount_point }))
            .await
            .map(drop)
    }

    /// 列出挂载点（v1.75: mount/listmounts；旧版 mount/list 404 时返回空）
    pub async fn mounts(&self) -> Result<Vec<MountInfo>> {
        let v = match self.call("mount/listmounts", &json!({})).await {
            Ok(v) => v,
            Err(_) => match self.call("mount/list", &json!({})).await {
                Ok(v) => v,
                Err(_) => return Ok(vec![]),
            },
        };
        Ok(parse_mounts(v))
    }

    // ---------- sync ----------

    pub async fn sync_copy(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        self.call(
            "sync/copy",
            &json!({ "srcFs": src_fs, "srcRemote": src_remote, "dstFs": dst_fs, "dstRemote": dst_remote }),
        )
        .await
        .map(drop)
    }

    pub async fn sync_sync(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        self.call(
            "sync/sync",
            &json!({ "srcFs": src_fs, "srcRemote": src_remote, "dstFs": dst_fs, "dstRemote": dst_remote }),
        )
        .await
        .map(drop)
    }

    pub async fn sync_move(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        self.call(
            "sync/move",
            &json!({ "srcFs": src_fs, "srcRemote": src_remote, "dstFs": dst_fs, "dstRemote": dst_remote }),
        )
        .await
        .map(drop)
    }
}
