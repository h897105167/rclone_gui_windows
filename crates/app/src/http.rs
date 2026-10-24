//! 内嵌 HTTP 服务（**与具体插件无关**）。
//!
//! 只有两条路由：
//! - `/plugin/{id}/{*rest}`：把请求转给 `config.json` 里声明了 `http_entry: true`
//!   的插件。路径、查询串、请求头、原始 body 原样交给插件，路由表、鉴权、
//!   响应格式全部由插件自己决定（宿主只负责超时与错误兜底）。
//! - `/api/health`：探活。
//!
//! `/api/plugin/{id}/{*rest}` 是早期版本用过的别名，保留一段时间，
//! 新接入方请用 `/plugin/...`。
//!
//! 这里**不认识任何插件 id、任何业务路径**：图床的 `/upload`、`/i/...`、短链
//! `/s/...` 都住在图床插件自己的路由表里（见 `rcgui-plugin-imagehost`）。

use crate::plugins;
use crate::Ctx;
use anyhow::Result;
use axum::extract::{FromRequest, Multipart, Path as AxPath, Query as AxQuery, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::Router;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// 单次请求体上限。上传图片够用，同时挡住误把 GB 级文件怼进来的情况。
const BODY_LIMIT: usize = 100 * 1024 * 1024;

#[derive(Clone)]
pub struct HttpState {
    pub ctx: Arc<Ctx>,
}

pub fn build_router(state: HttpState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/plugin/{id}/{*rest}", any(plugin_dispatch))
        .route("/plugin/{id}", any(plugin_root))
        // 兼容旧路径
        .route("/api/plugin/{id}/{*rest}", any(plugin_dispatch))
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

async fn plugin_root(
    State(st): State<HttpState>,
    method: Method,
    AxPath(id): AxPath<String>,
    AxQuery(q): AxQuery<HashMap<String, String>>,
    headers: HeaderMap,
    req: Request,
) -> impl IntoResponse {
    dispatch(st, id, "/".to_string(), method, q, headers, req).await
}

async fn plugin_dispatch(
    State(st): State<HttpState>,
    method: Method,
    AxPath((id, rest)): AxPath<(String, String)>,
    AxQuery(q): AxQuery<HashMap<String, String>>,
    headers: HeaderMap,
    req: Request,
) -> impl IntoResponse {
    dispatch(st, id, format!("/{}", rest.trim_start_matches('/')), method, q, headers, req).await
}

/// 统一入口：把 HTTP 请求打包成插件协议里的 `http` 对象，交给插件。
///
/// 打包规则（写进 `docs/PLUGIN.md`，插件作者按这个读）：
/// - `body_b64`：原始请求体，base64（二进制安全；JSON 字符串装不下非 UTF-8）
/// - `multipart`：`multipart/form-data` 时额外给出
///   `{"fields":{...},"files":[{"filename","dataB64"}]}`
/// - `authorization`：原始 Authorization 头（插件自己校验）
/// - `headers`：其余请求头（小写键）
async fn dispatch(
    st: HttpState,
    id: String,
    plugin_path: String,
    method: Method,
    q: HashMap<String, String>,
    headers: HeaderMap,
    req: Request,
) -> axum::response::Response {
    if !plugin_enabled(&st, &id) {
        return fail(
            StatusCode::FORBIDDEN,
            format!("插件 {id} 已停用，请先在「插件」页启用"),
        );
    }

    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1")
        .to_string();
    let origin = format!("http://{host}");
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // 其余请求头：给插件留出读自定义头的余地（如短链服务需要的 X-* 头）
    let mut header_map = serde_json::Map::new();
    for (k, v) in headers.iter() {
        let name = k.as_str().to_lowercase();
        if name == "authorization" || name == "host" || name == "content-length" {
            continue;
        }
        if let Ok(text) = v.to_str() {
            header_map.insert(name, Value::String(text.to_string()));
        }
    }

    let query: Value = q
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect::<serde_json::Map<String, Value>>()
        .into();

    // multipart 走 axum 的解析器，其余一律原始字节
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
                return fail(StatusCode::BAD_REQUEST, format!("multipart 解析失败: {e}"));
            }
        }
        (Some(json!({"fields": fields, "files": files})), String::new())
    } else {
        let bytes = match axum::body::to_bytes(req.into_body(), BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => {
                return fail(StatusCode::BAD_REQUEST, format!("读取请求体失败: {e}"));
            }
        };
        (
            None,
            base64::engine::general_purpose::STANDARD.encode(&bytes),
        )
    };

    let http_req = json!({
        "method": method.as_str(),
        "path": plugin_path,
        "query": query,
        "headers": header_map,
        "authorization": authorization,
        "body_b64": body_b64,
        "multipart": multipart_json,
        "origin": origin,
        "remote_addr": headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("127.0.0.1")
            .to_string(),
        "req_id": uuid::Uuid::new_v4().simple().to_string(),
    });

    match plugins::dispatch_http(&st.ctx, &id, http_req).await {
        Ok(v) => render(v),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// 插件返回的 `result` → HTTP 响应。
///
/// 约定（见 `docs/PLUGIN.md`）：
/// - `status`：HTTP 状态码，缺省 200
/// - `body`：JSON 响应体
/// - `body_b64` + `content_type`：二进制响应（图片、下载流）
/// - `headers`：附加响应头
fn render(v: Value) -> axum::response::Response {
    let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(200) as u16;
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);

    let mut extra: Vec<(String, String)> = Vec::new();
    if let Some(h) = v.get("headers").and_then(|h| h.as_object()) {
        for (k, val) in h {
            if let Some(s) = val.as_str() {
                extra.push((k.clone(), s.to_string()));
            }
        }
    }

    if let Some(b64) = v.get("body_b64").and_then(|b| b.as_str()).filter(|s| !s.is_empty()) {
        match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
            Ok(bytes) => {
                let ctype = v
                    .get("content_type")
                    .and_then(|c| c.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let mut resp = (
                    [
                        (header::CONTENT_TYPE, ctype),
                        (header::CACHE_CONTROL, "public, max-age=3600".to_string()),
                    ],
                    bytes,
                )
                    .into_response();
                apply_headers(resp.headers_mut(), &extra);
                *resp.status_mut() = status;
                return resp;
            }
            Err(e) => {
                return fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("插件返回的 body_b64 不是合法 base64: {e}"),
                );
            }
        }
    }

    let body = v.get("body").cloned().unwrap_or(json!({}));
    let mut resp = (status, axum::Json(body)).into_response();
    apply_headers(resp.headers_mut(), &extra);
    resp
}

fn apply_headers(headers: &mut HeaderMap, extra: &[(String, String)]) {
    for (k, val) in extra {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(k.as_str()),
            axum::http::HeaderValue::try_from(val.as_str()),
        ) {
            headers.insert(name, value);
        }
    }
}

fn fail(status: StatusCode, message: String) -> axum::response::Response {
    (status, axum::Json(json!({"error": message}))).into_response()
}

fn plugin_enabled(st: &HttpState, id: &str) -> bool {
    st.ctx
        .cfg
        .lock()
        .map(|c| c.plugin_enabled.get(id).copied().unwrap_or(true))
        .unwrap_or(true)
}
