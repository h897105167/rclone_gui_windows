//! 内嵌 HTTP 服务：
//! - /api/plugin/{id}/*  → 转发到插件（multipart / raw body 双模式，兼容旧 Obsidian 插件）
//! - GET /i/{rootId}/{*path} → 图床直链代理（磁盘缓存 + rclone cat 回源）
//! - GET /api/health

use crate::config;
use crate::kv::RealCaps;
use anyhow::{anyhow, Result};
use axum::extract::{FromRequest, Multipart, Path as AxPath, Query as AxQuery, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::Router;
use base64::Engine as _;
use plugin_host::PluginHost;

use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

const BODY_LIMIT: usize = 100 * 1024 * 1024;

#[derive(Clone)]
pub struct HttpState {
    pub host: Arc<PluginHost>,
    pub caps: Arc<RealCaps>,
    pub cfg: Arc<std::sync::Mutex<config::AppConfig>>,
}

impl HttpState {
    /// 插件是否已启用（默认启用）
    fn plugin_enabled(&self, id: &str) -> bool {
        self.cfg
            .lock()
            .map(|c| c.plugin_enabled.get(id).copied().unwrap_or(true))
            .unwrap_or(true)
    }
}

pub fn build_router(state: HttpState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/plugin/{id}/{*rest}", any(plugin_dispatch))
        // 旧版 Obsidian 插件约定的裸路径（raw-body + query），保持向后兼容
        .route("/upload", any(legacy_upload))
        .route("/i/{root_id}/{*path}", get(direct_link))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

pub async fn serve(
    state: HttpState,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(json!({"ok": true})))
}

pub(crate) fn now_json() -> Value {
    use chrono::{Datelike, Local, Timelike};
    let now = Local::now();
    json!({
        "ymd": format!("{:04}{:02}{:02}", now.year(), now.month(), now.day()),
        "hms": format!("{:02}{:02}{:02}", now.hour(), now.minute(), now.second()),
        "ms": format!("{:03}", now.timestamp_subsec_millis()),
        "date_iso": now.format("%Y-%m-%d").to_string(),
        "iso": now.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
        "epoch_ms": now.timestamp_millis().to_string(),
    })
}

/// 插件请求统一入口：把 HTTP 请求转成协议 HttpReq，交给插件多轮协商
async fn plugin_dispatch(
    State(st): State<HttpState>,
    method: Method,
    AxPath((id, rest)): AxPath<(String, String)>,
    AxQuery(q): AxQuery<HashMap<String, String>>,
    headers: HeaderMap,
    req: Request,
) -> impl IntoResponse {
    dispatch_to_plugin(st, id, format!("/{rest}"), method, q, headers, req).await
}

/// 旧版 Obsidian 插件兼容入口：POST /upload → imagehost 的 /upload。
///
/// 早期 Node/Vue 时代的 Obsidian 插件约定直接向 `127.0.0.1:5273/upload`
/// 以 raw-body + query 方式上传（响应 `{items:[...]}`）。当前实现只暴露
/// `/api/plugin/imagehost/upload`，旧插件会收到 404 而静默失败。
/// 这里保留该别名，使旧插件无需改动即可继续工作。
async fn legacy_upload(
    State(st): State<HttpState>,
    method: Method,
    AxQuery(q): AxQuery<HashMap<String, String>>,
    headers: HeaderMap,
    req: Request,
) -> impl IntoResponse {
    dispatch_to_plugin(st, "imagehost".to_string(), "/upload".to_string(), method, q, headers, req)
        .await
}

async fn dispatch_to_plugin(
    st: HttpState,
    id: String,
    plugin_path: String,
    method: Method,
    q: HashMap<String, String>,
    headers: HeaderMap,
    req: Request,
) -> axum::response::Response {
    if !st.plugin_enabled(&id) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": format!("插件 {id} 已停用")})),
        )
            .into_response();
    }
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    let origin = format!("http://{host}");

    let query: Value = q
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect::<serde_json::Map<String, Value>>()
        .into();

    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // multipart 或 raw body
    let (multipart_json, body_b64) = if ctype.starts_with("multipart/form-data") {
        let mut fields = serde_json::Map::new();
        let mut files: Vec<Value> = Vec::new();
        match Multipart::from_request(req, &()).await {
            Ok(mut mp) => loop {
                match mp.next_field().await {
                    Ok(Some(f)) => {
                        let name = f.name().unwrap_or("").to_string();
                        let filename = f.file_name().map(|s| s.to_string());
                        let data = f.bytes().await.unwrap_or_default();
                        if let Some(fname) = filename {
                            files.push(json!({
                                "filename": fname,
                                "dataB64": base64::engine::general_purpose::STANDARD.encode(&data),
                            }));
                        } else {
                            fields.insert(
                                name,
                                Value::String(String::from_utf8_lossy(&data).to_string()),
                            );
                        }
                    }
                    _ => break,
                }
            },
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": format!("multipart 解析失败: {e}")})),
                )
                    .into_response();
            }
        }
        (Some(json!({"fields": fields, "files": files})), String::new())
    } else {
        let bytes = match axum::body::to_bytes(req.into_body(), BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": format!("读取请求体失败: {e}")})),
                )
                    .into_response();
            }
        };
        (None, base64::engine::general_purpose::STANDARD.encode(&bytes))
    };

    let http_req = json!({
        "method": method.as_str(),
        "path": plugin_path,
        "query": query,
        "authorization": authorization,
        "bodyB64": body_b64,
        "multipart": multipart_json,
        "origin": origin,
    });

    let params = json!({
        "http": http_req,
        "now": now_json(),
        "req_id": uuid::Uuid::new_v4().simple().to_string(),
    });

    let result = st
        .host
        .invoke(&plugin_wasm_path(&id), "request", params, Some(st.caps.clone()), &id)
        .await;

    match result {
        Ok(v) => {
            let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(200) as u16;
            let body = v.get("body").cloned().unwrap_or(json!({}));
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
                axum::Json(body),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// 插件 wasm 定位：data/plugins/{id}.wasm → exe旁 assets → 内置释放
fn plugin_wasm_path(id: &str) -> PathBuf {
    let data = config::data_dir().join("plugins").join(format!("{id}.wasm"));
    if data.exists() {
        return data;
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.parent()
                .map(|d| d.join("assets/plugins").join(format!("{id}.wasm")))
        })
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("assets/plugins").join(format!("{id}.wasm")))
}

fn sha1_hex(data: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 直链代理：data/cache/{rootId}/{sha1}_{name}，miss 时 rclone cat 回源
async fn direct_link(
    State(st): State<HttpState>,
    AxPath((root_id, path)): AxPath<(String, String)>,
) -> impl IntoResponse {
    match direct_link_inner(&st, &root_id, &path).await {
        Ok((bytes, ctype)) => (
            [
                (header::CONTENT_TYPE, ctype.to_string()),
                (header::CACHE_CONTROL, "public, max-age=3600".to_string()),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => (StatusCode::NOT_FOUND, format!("直链获取失败: {e}")).into_response(),
    }
}

type BodyOf = (Vec<u8>, &'static str);

/// 图床 roots：kv 有 config 直接用；否则向插件取当前（含默认）配置
async fn fetch_roots(st: &HttpState) -> Vec<Value> {
    let cfg_raw = st.caps.store.get_all("imagehost");
    if let Some(roots) = cfg_raw
        .get("config")
        .and_then(|c| c.get("roots"))
        .and_then(|r| r.as_array())
    {
        return roots.clone();
    }
    let params = json!({
        "http": {"method":"GET","path":"/config","query":{},"authorization":"","bodyB64":"","multipart":null,"origin":"local"},
        "now": now_json(),
        "req_id": uuid::Uuid::new_v4().simple().to_string(),
    });
    match st
        .host
        .invoke(
            &plugin_wasm_path("imagehost"),
            "request",
            params,
            Some(st.caps.clone()),
            "imagehost",
        )
        .await
    {
        Ok(v) => v
            .get("body")
            .and_then(|b| b.get("roots"))
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default(),
        Err(e) => {
            let _ = std::fs::write(
                config::data_dir().join("directlink.err"),
                format!("{}\n", e),
            );
            Vec::new()
        }
    }
}

/// 规范化远程相对路径：统一分隔符并剔除 `.` / `..` 段。
///
/// 直链路径来自 URL，未规范化时 `..` 会被原样传给 `rclone cat`，
/// 从而读到图床根目录之外的文件。这里与插件的 normalize_sub 保持同一规则。
pub(crate) fn normalize_rel_path(p: &str) -> String {
    p.replace('\\', "/")
        .split('/')
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .collect::<Vec<_>>()
        .join("/")
}

async fn direct_link_inner(st: &HttpState, root_id: &str, path: &str) -> Result<BodyOf> {
    let roots = fetch_roots(st).await;
    let root = roots
        .iter()
        .find(|r| r.get("id").and_then(|x| x.as_str()) == Some(root_id))
        .ok_or_else(|| anyhow!("root 不存在: {root_id}"))?;
    let remote = root
        .get("remote")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("root 缺少 remote"))?
        .to_string();

    // 防目录穿越：路径必须先规范化再回源
    let path = normalize_rel_path(path);
    if path.is_empty() {
        return Err(anyhow!("路径为空"));
    }

    let name = path.rsplit('/').next().unwrap_or("file").to_string();
    let cache_dir = config::data_dir().join("cache").join(root_id);
    let key = &sha1_hex(format!("{remote}:{path}").as_bytes())[..16];
    let file = cache_dir.join(format!("{key}_{name}"));

    if !file.exists() {
        std::fs::create_dir_all(&cache_dir)?;
        // 回源：只走 RC（operations/copyfile 到唯一临时文件 → 读字节 → 写缓存）。
        // 不再启动 `rclone cat` 子进程；该 rclone 版本没有 operations/cat（实测 404）。
        let dir = cache_dir.join(format!("tmp-{}", std::process::id()));
        let client = st.caps.rc.client().await;
        let bytes = rcgui_rc::fetch_to_temp(&client, &remote, &path, &dir)
            .await
            .map_err(|e| anyhow!("回源失败（{}）: {}", e.kind.label(), e))?;
        // 原子落缓存：先写临时名再改名，避免读到半截文件
        let tmp = file.with_extension(format!("dl-{}", std::process::id()));
        tokio::fs::write(&tmp, &bytes).await?;
        std::fs::rename(&tmp, &file)?;
        let _ = std::fs::remove_dir_all(&dir);
    }
    let bytes = tokio::fs::read(&file).await?;
    Ok((bytes, content_type_of(&name)))
}

fn content_type_of(name: &str) -> &'static str {
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
        "txt" | "md" => "text/plain",
        _ => "application/octet-stream",
    }
}
