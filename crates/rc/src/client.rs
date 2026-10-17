//! RC API 客户端。
//!
//! 本文件从 `crates/rclone/src/rc.rs` 迁入：只保留「HTTP + 鉴权 + 方法封装」，
//! 与 rclone 进程的启动/终止完全无关。
//!
//! 兼容性约定：rclone v1.75+ 把 `operations/copy` 改名为 `operations/copyfile`、
//! `moveto` 改名为 `operations/movefile`，这里对旧名做自动回退。

use crate::error::{classify_response, RcError, RcErrorKind, RcResult};
use crate::types::{parse_mounts, ConfigPaths, CoreStats, ListEntry, MountInfo, VersionInfo};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct RcClient {
    http: reqwest::Client,
    base_url: String,
    auth: Option<(String, String)>,
}

impl RcClient {
    /// `base_url` 形如 `http://127.0.0.1:5572`（尾部斜杠会被去掉）。
    /// 用户名/密码为空时视为无鉴权。
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

    /// 规范化后的端点地址（不含凭据，可安全进日志）。
    pub fn endpoint(&self) -> &str {
        &self.base_url
    }

    /// 只暴露用户名，不暴露密码。
    pub fn auth_user(&self) -> Option<&str> {
        self.auth.as_ref().map(|(u, _)| u.as_str())
    }

    /// 是否配置了鉴权（布尔值，不泄露凭据本身）。
    pub fn has_auth(&self) -> bool {
        self.auth.is_some()
    }

    /// 把 reqwest 的传输层错误归类。`detail` 里不会出现凭据。
    fn transport_error(&self, method: &str, e: reqwest::Error) -> RcError {
        if e.is_timeout() {
            return RcError::new(
                RcErrorKind::Timeout,
                method,
                format!("{} 无响应（超时）", self.base_url),
            );
        }
        if e.is_connect() {
            return RcError::new(
                RcErrorKind::Connect,
                method,
                format!("无法连接 {}（{e}）", self.base_url),
            );
        }
        RcError::new(RcErrorKind::Connect, method, e.to_string())
    }

    /// 调用 RC 方法：POST {base}/{method}，返回 JSON。
    pub async fn call(&self, method: &str, params: &Value) -> RcResult<Value> {
        let url = format!("{}/{}", self.base_url, method);
        let mut req = self.http.post(&url).json(params);
        if let Some((u, p)) = &self.auth {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req.send().await.map_err(|e| self.transport_error(method, e))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
            let kind = classify_response(status.as_u16(), &msg);
            return Err(RcError::new(kind, method, msg));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| RcError::new(RcErrorKind::Decode, method, format!("响应解析失败: {e}")))
    }

    /// 依次尝试多个方法名（新旧版本改名兼容），**仅**在「方法不存在」时回退。
    pub async fn call_fallback(&self, methods: &[&str], params: &Value) -> RcResult<Value> {
        let mut last_err = None;
        for m in methods {
            match self.call(m, params).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if e.is_unsupported() {
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            RcError::new(RcErrorKind::Unsupported, methods.join("|"), "无可用 RC 方法")
        }))
    }

    // ---------- core ----------

    pub async fn version(&self) -> RcResult<VersionInfo> {
        let v = self.call("core/version", &json!({})).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// 优雅退出 rclone（会自动卸载挂载、停止服务）。
    ///
    /// **legacy**：重构指南禁止 RCGUI 主动终止 rclone 进程，阶段 7 会移除该调用点。
    /// 保留方法本身，是为了让现有「退出时清理」行为在本阶段不退化。
    pub async fn core_quit(&self) -> RcResult<()> {
        self.call("core/quit", &json!({})).await.map(drop)
    }

    pub async fn core_stats(&self) -> RcResult<CoreStats> {
        let v = self.call("core/stats", &json!({})).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    pub async fn core_transferred(&self) -> RcResult<Value> {
        self.call("core/transferred", &json!({ "group": "" })).await
    }

    // ---------- 握手 / 能力探测（不依赖任何子进程） ----------

    /// rclone 实际使用的配置/缓存/临时目录。
    ///
    /// 取代旧实现里 `rclone config file` 子进程（`detect_config_path`，已删除）。
    /// 指南要求「配置路径必须问运行的实例，不能假设 %APPDATA%\rclone\rclone.conf」。
    pub async fn config_paths(&self) -> RcResult<ConfigPaths> {
        let v = self.call("config/paths", &json!({})).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// 该实例实际注册的全部 RC 方法名（`rc/list`）。
    ///
    /// 支持特性探测：先查再调，缺失接口明确报「不支持」而不是通用失败。
    pub async fn rc_methods(&self) -> RcResult<Vec<String>> {
        let v = self.call("rc/list", &json!({})).await?;
        Ok(parse_methods(&v))
    }
    // ---------- config ----------

    /// 远程名统一去掉结尾冒号，调用方拼接 fs 时用 `format!("{name}:")`
    pub async fn list_remotes(&self) -> RcResult<Vec<String>> {
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

    /// 完整配置（**含 OAuth token 与密码**）。调用方不得把结果写入日志或普通文件。
    pub async fn config_dump(&self) -> RcResult<Value> {
        self.call("config/dump", &json!({})).await
    }

    /// 获取所有后端类型及其参数定义
    pub async fn config_providers(&self) -> RcResult<Value> {
        self.call("config/providers", &json!({})).await
    }

    /// 新建 remote：parameters 为键值对（type 单独传）
    pub async fn config_create(&self, name: &str, rtype: &str, parameters: Value) -> RcResult<()> {
        self.call(
            "config/create",
            &json!({ "name": name, "type": rtype, "parameters": parameters }),
        )
        .await
        .map(drop)
    }

    /// 更新 remote 参数（仅覆盖给出的键）
    pub async fn config_update(&self, name: &str, parameters: Value) -> RcResult<()> {
        self.call("config/update", &json!({ "name": name, "parameters": parameters }))
            .await
            .map(drop)
    }

    /// 读取单个 remote 的全部键值。
    ///
    /// **代理功能必须用这个而不是 `config/dump`**：`dump` 会返回所有远程的
    /// OAuth token 与密码，为了读一个 `override.http_proxy` 而拉全量凭据没有必要，
    /// 也容易在日志/错误里泄露。
    pub async fn config_get(&self, name: &str) -> RcResult<serde_json::Map<String, Value>> {
        let v = self.call("config/get", &json!({ "name": name })).await?;
        Ok(v.as_object().cloned().unwrap_or_default())
    }

    /// 删除 remote 上的若干键。返回**实际被删除**的键名。
    ///
    /// rclone 的行为（v1.75.4 实测）：
    /// - 键不存在时不报错，返回 `{"removed": null}`；
    /// - 因此调用方不能靠「有没有报错」判断「原来是否存在」，要看返回的 `removed`。
    pub async fn config_unset(&self, name: &str, keys: &[&str]) -> RcResult<Vec<String>> {
        let v = self
            .call(
                "config/unset",
                &json!({ "name": name, "keys": keys }),
            )
            .await?;
        Ok(v.get("removed")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn config_delete(&self, name: &str) -> RcResult<()> {
        self.call("config/delete", &json!({ "name": name }))
            .await
            .map(drop)
    }

    // ---------- serve ----------

    pub async fn serve_list(&self) -> RcResult<Value> {
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
    ) -> RcResult<String> {
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

    pub async fn serve_stop(&self, id: &str) -> RcResult<()> {
        self.call("serve/stop", &json!({ "id": id })).await.map(drop)
    }

    // ---------- job ----------

    pub async fn job_list(&self) -> RcResult<Vec<i64>> {
        let v = self.call("job/list", &json!({})).await?;
        Ok(v.get("jobids")
            .and_then(|j| j.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
            .unwrap_or_default())
    }

    pub async fn job_status(&self, jobid: i64) -> RcResult<Value> {
        self.call("job/status", &json!({ "jobid": jobid })).await
    }

    pub async fn job_stop(&self, jobid: i64) -> RcResult<()> {
        self.call("job/stop", &json!({ "jobid": jobid })).await.map(drop)
    }

    // ---------- options / bwlimit ----------

    /// 设置带宽限速，如 "1M"、"10M:100K"、"off"
    pub async fn bwlimit(&self, rate: &str) -> RcResult<String> {
        let v = self.call("core/bwlimit", &json!({ "rate": rate })).await?;
        Ok(v.get("rate").and_then(|x| x.as_str()).unwrap_or(rate).to_string())
    }

    pub async fn options_get(&self) -> RcResult<Value> {
        self.call("options/get", &json!({})).await
    }

    /// 设置运行时选项：{"main": {"Transfers": 4, ...}}（字段名为 Go 风格大写）
    pub async fn options_set(&self, opts: Value) -> RcResult<()> {
        self.call("options/set", &opts).await.map(drop)
    }

    // ---------- operations ----------

    pub async fn list(&self, fs: &str, remote: &str, options: Option<Value>) -> RcResult<Vec<ListEntry>> {
        let mut params = json!({ "fs": fs, "remote": remote });
        if let Some(opts) = options {
            params["options"] = opts;
        }
        let v = self.call("operations/list", &params).await?;
        let list = v.get("list").cloned().unwrap_or_else(|| Value::Array(vec![]));
        Ok(serde_json::from_value(list).unwrap_or_default())
    }

    pub async fn mkdir(&self, fs: &str, remote: &str) -> RcResult<()> {
        self.call("operations/mkdir", &json!({ "fs": fs, "remote": remote }))
            .await
            .map(drop)
    }

    pub async fn rmdir(&self, fs: &str, remote: &str) -> RcResult<()> {
        self.call("operations/rmdir", &json!({ "fs": fs, "remote": remote }))
            .await
            .map(drop)
    }

    pub async fn purge(&self, fs: &str, remote: &str) -> RcResult<()> {
        self.call("operations/purge", &json!({ "fs": fs, "remote": remote }))
            .await
            .map(drop)
    }

    pub async fn deletefile(&self, fs: &str, remote: &str) -> RcResult<()> {
        self.call("operations/deletefile", &json!({ "fs": fs, "remote": remote }))
            .await
            .map(drop)
    }

    /// 复制文件（v1.75: copyfile；旧版: copy）
    pub async fn copyfile(
        &self,
        src_fs: &str,
        src_remote: &str,
        dst_fs: &str,
        dst_remote: &str,
    ) -> RcResult<()> {
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
    pub async fn movefile(
        &self,
        src_fs: &str,
        src_remote: &str,
        dst_fs: &str,
        dst_remote: &str,
    ) -> RcResult<()> {
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

    /// 递归移动目录（`sync/move`）。
    ///
    /// 与 [`Self::movefile`] 不同，这里接收**完整 fs 串**（`remote:path`）：
    /// rclone 会先尝试服务端目录重命名（`DirMove`），不行再逐个文件移动。
    /// `create_empty_src_dirs` 让空目录也能在目标侧建出来（否则空文件夹会「消失」）；
    /// `delete_empty_src_dirs` 在移动完成后删掉空的源目录。
    pub async fn sync_move(
        &self,
        src_fs: &str,
        dst_fs: &str,
        create_empty_src_dirs: bool,
        delete_empty_src_dirs: bool,
    ) -> RcResult<()> {
        self.call(
            "sync/move",
            &json!({
                "srcFs": src_fs,
                "dstFs": dst_fs,
                "createEmptySrcDirs": create_empty_src_dirs,
                "deleteEmptySrcDirs": delete_empty_src_dirs,
            }),
        )
        .await
        .map(drop)
    }

    pub async fn about(&self, fs: &str) -> RcResult<Value> {
        self.call("operations/about", &json!({ "fs": fs })).await
    }

    pub async fn publiclink(&self, fs: &str, remote: &str) -> RcResult<String> {
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
    pub async fn mount(&self, fs: &str, mount_point: &str, options: Value) -> RcResult<String> {
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

    pub async fn unmount(&self, mount_point: &str) -> RcResult<()> {
        self.call("mount/unmount", &json!({ "mountPoint": mount_point }))
            .await
            .map(drop)
    }

    /// 列出挂载点（v1.75: mount/listmounts；旧版 mount/list 404 时返回空）
    pub async fn mounts(&self) -> RcResult<Vec<MountInfo>> {
        Ok(parse_mounts(self.mounts_raw().await?))
    }

    /// 挂载列表的原始 JSON（导出时还需要 MountedOn 等字段）。
    pub async fn mounts_raw(&self) -> RcResult<serde_json::Value> {
        match self.call("mount/listmounts", &json!({})).await {
            Ok(v) => Ok(v),
            Err(_) => match self.call("mount/list", &json!({})).await {
                Ok(v) => Ok(v),
                Err(_) => Ok(json!({})),
            },
        }
    }

    // ---------- sync ----------

    /// 递归复制目录（`sync/copy`）。
    ///
    /// 参数是**完整 fs 串**（`remote:path`）。RC 的 `sync/*` 系列没有
    /// `srcRemote` / `dstRemote`，路径必须拼进 fs 串里。
    pub async fn sync_copy(
        &self,
        src_fs: &str,
        dst_fs: &str,
        create_empty_src_dirs: bool,
    ) -> RcResult<()> {
        self.call(
            "sync/copy",
            &json!({
                "srcFs": src_fs,
                "dstFs": dst_fs,
                "createEmptySrcDirs": create_empty_src_dirs,
            }),
        )
        .await
        .map(drop)
    }

    /// 递归镜像目录（`sync/sync`，会删除目标侧多余文件）。
    pub async fn sync_sync(
        &self,
        src_fs: &str,
        dst_fs: &str,
        create_empty_src_dirs: bool,
    ) -> RcResult<()> {
        self.call(
            "sync/sync",
            &json!({
                "srcFs": src_fs,
                "dstFs": dst_fs,
                "createEmptySrcDirs": create_empty_src_dirs,
            }),
        )
        .await
        .map(drop)
    }
}

/// 解析 `rc/list` 的返回。
///
/// **实测差异**：rclone v1.75.4 的容器键是 `commands`，而 gulp79 的 extra 构建用的是
/// `calls`；早期版本还可能直接返回根数组。这里全部兼容，避免「明明支持却报缺失」。
pub fn parse_methods(v: &Value) -> Vec<String> {
    let arr = v
        .get("commands")
        .or_else(|| v.get("calls"))
        .or_else(|| v.get("Calls"))
        .or_else(|| v.get("Commands"))
        .and_then(|c| c.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for it in arr {
        if let Some(s) = it.as_str() {
            out.push(s.to_string());
            continue;
        }
        if let Some(p) = it
            .get("Path")
            .or_else(|| it.get("path"))
            .and_then(|p| p.as_str())
        {
            out.push(p.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}
#[cfg(test)]
mod method_parse_tests {
    use super::parse_methods;
    use serde_json::json;

    #[test]
    fn parses_v1754_commands_key() {
        let v = json!({ "commands": [ {"Path": "config/listremotes"}, {"Path": "mount/mount"} ] });
        assert_eq!(parse_methods(&v), vec!["config/listremotes", "mount/mount"]);
    }

    #[test]
    fn parses_extra_build_calls_key() {
        let v = json!({ "calls": [ {"Path": "serve/list"} ] });
        assert_eq!(parse_methods(&v), vec!["serve/list"]);
    }

    #[test]
    fn parses_root_array_of_strings() {
        let v = json!(["a/b", "c/d"]);
        assert_eq!(parse_methods(&v), vec!["a/b", "c/d"]);
    }

    #[test]
    fn unknown_shape_yields_empty() {
        assert!(parse_methods(&json!({})).is_empty());
        assert!(parse_methods(&json!({ "commands": null })).is_empty());
    }
}